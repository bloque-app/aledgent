// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Agent core: ingress's [`Core`] over the core Agent (`F-005`, feature `ingress`).
//!
//! [`AgentCore`] answers ingress's four operations the way [`ingress::inprocess::InProcessCore`]
//! does, with one difference: nothing is committed until the core Agent has committed it. Each
//! input runs on the replica (ingress's own executor, [`Running`], over the host's stores); the
//! [`Driver`] sends the calls it made to the Agent and checks the Agent's transition against it;
//! only then is the reply recorded and returned. Queries, the outbox and the last tick are
//! answered from the replica, which is the Agent's book at the Agent's roots after every input
//! (`DEC-4`: reads go to the host's store, not through the Agent).
//!
//! **When the Agent does not commit.** The replica has run an input the Agent refused (a full
//! ring or outbox, the budget, the runtime unreachable). The host rebuilds the replica from what
//! the Agent committed ([`Driver::recover`]) and answers [`CoreError::Unavailable`]: the input did
//! not run, and ingress keeps it `Received` and retries (`REQ-CS-7`). An input the Agent may have
//! committed (an ambiguous failure) is settled the same way: after the rebuild, the Agent's input
//! number says whether it did. A transition the replica does not reproduce is an incident: the
//! core stops answering (`Unavailable`) until an operator restarts it. So is an input the Agent
//! fails alone, definitively, [`FAILED_ATTEMPTS`] times in a row (finding F18 of `F-032`): the
//! core halts on it, with an alert, instead of rebuilding from the whole archive on every retry.
//!
//! **Batches** (`T-031-03`, `REQ-CS-7`, `INV-18`). [`Core::apply_batch`] runs its inputs natively in
//! order, stages each ([`Driver::stage`]) and sends them in one invocation ([`Driver::flush`]).
//! After a refused or ambiguous flush the host rebuilds, as above: the inputs the Agent committed
//! (its input number after the rebuild says which) keep their native outcomes, in order, and every
//! input after the first it did not commit is sent again alone, one at a time, in order, until one
//! fails. A faulty input then fails alone and the inputs before it commit: the hybrid isolation
//! ruled for the settlement contract's batches (#102), applied to the core. No input is answered
//! before one ahead of it.
//!
//! **Durability.** Two logs, in a [`HostLog`]: the archive (every input the Agent committed, as
//! its delta-ring entry) and the replies (every input's reply, by key). The archive is written
//! before the Agent is told it is archived, so the Agent's ring always holds whatever the archive
//! lacks. A start rebuilds everything from the archive and the Agent.
//!
//! **The audit trail** (`T-013-13`, `REQ-AT-2`, finding G-02 of `F-032`). Opened with
//! [`AgentCore::open_audited`], the core ships its replica's audit outbox to the audit store from
//! a thread of its own ([`ingress::audit_ship`]): only what the Agent committed, each record once
//! and in order, each dropped from the outbox once the store confirmed it. Every rebuild of the
//! replica drops, as it replays, what the store already holds. While `max_unshipped` records wait,
//! the core refuses new inputs (`Unavailable`): they stay `Received`, in order.
//!
//! **Memory** (`T-031-14`, finding F6 of `F-032`). The replica's stores can live on disk
//! ([`AgentCore::open_in`] with [`ReplicaStores::Disk`]; `aledgent-core-host serve
//! --replica-on-disk`), each with a bounded cache: they are rebuilt from the archive on every
//! start anyway. Opt-in: the stress suite measured it slower for little less memory over an hour. Only the
//! [`RECENT_REPLIES`] most recent replies stay in memory; an older one is read from the log when
//! its key is retried, and every key answered is remembered, so no input runs twice (`INV-18`).

use std::boxed::Box;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::PathBuf;
use std::string::{String, ToString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use ingress::gate::Gate;
use std::vec::Vec;
use std::{format, vec};

use ingress::audit_ship::{AuditOutbox, Head, Shared as AuditShared, ShipHandle, Shipper};
use ingress::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, InputBody, InputKey,
    OutboxPage, Outcome, Query, QueryReply,
};
use ingress::inprocess::{Running, SharedClerk};
use ledger_core::account::Time;
use ledger_core::id::OperationId;
use ledger_core::ledger::{Founded, Genesis};
use ledger_core::lifecycle::keys::NotificationId;

use crate::envelope::{AgentRefusal, Fenced, decode, encode};
use crate::host::{
    AgentPort, BatchLimits, DriveError, Driver, HostStore, HostVault, PortError, ReplicaStores,
};
use crate::hot::{Limits, RingEntry};

/// Where the host keeps what the Agent committed, and every reply.
pub trait HostLog: Send + 'static {
    /// Every archived entry, in input order.
    fn entries(&self) -> Result<Vec<RingEntry>, String>;
    /// Archive one entry durably.
    fn push_entry(&mut self, e: &RingEntry) -> Result<(), String>;
    /// Every reply, in order.
    fn replies(&self) -> Result<Vec<(InputKey, CoreReply)>, String>;
    /// One input's reply. The core keeps only the most recent replies in memory and reads an
    /// older one here, when a key is retried long after it was answered (`T-031-14`).
    fn reply(&self, key: &InputKey) -> Result<Option<CoreReply>, String> {
        Ok(self
            .replies()?
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, r)| r))
    }
    /// Record one reply durably.
    fn push_reply(&mut self, key: &InputKey, reply: &CoreReply) -> Result<(), String>;
    /// Archive several entries durably, in order: one sync for all of them where the log can
    /// (`T-032-13`). Either every entry is archived or the call fails; a torn last record is
    /// ignored on reading, as for one entry.
    fn push_entries(&mut self, entries: &[RingEntry]) -> Result<(), String> {
        entries.iter().try_for_each(|e| self.push_entry(e))
    }
    /// Record several replies durably, in order: one sync for all of them where the log can
    /// (`T-032-13`).
    fn push_replies(&mut self, replies: &[(InputKey, CoreReply)]) -> Result<(), String> {
        replies.iter().try_for_each(|(k, r)| self.push_reply(k, r))
    }
}

/// The archived entries and the replies, in order.
type Logged = (Vec<RingEntry>, Vec<(InputKey, CoreReply)>);

/// A log in memory, shared by its clones (tests: it outlives the cores they start and stop).
#[derive(Clone, Default)]
pub struct MemLog(Arc<Mutex<Logged>>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl HostLog for MemLog {
    fn entries(&self) -> Result<Vec<RingEntry>, String> {
        Ok(lock(&self.0).0.clone())
    }
    fn push_entry(&mut self, e: &RingEntry) -> Result<(), String> {
        lock(&self.0).0.push(e.clone());
        Ok(())
    }
    fn replies(&self) -> Result<Vec<(InputKey, CoreReply)>, String> {
        Ok(lock(&self.0).1.clone())
    }
    fn push_reply(&mut self, key: &InputKey, reply: &CoreReply) -> Result<(), String> {
        lock(&self.0).1.push((*key, reply.clone()));
        Ok(())
    }
}

/// A log in two files in a directory: `archive.bin` (length-prefixed entries) and
/// `replies.jsonl`. Each append is synced before it counts; a torn last record is an append that
/// never completed, and is ignored.
pub struct FileLog {
    dir: PathBuf,
    /// Where each reply's line starts in `replies.jsonl`, and its length, built on the first
    /// lookup and caught up with the file on every later one (`T-032-15`).
    index: Mutex<Option<ReplyIndex>>,
}

/// An index of `replies.jsonl`: each key's line, as (offset, length), and how far it has read.
#[derive(Default)]
struct ReplyIndex {
    at: HashMap<InputKey, (u64, usize)>,
    end: u64,
}

/// The archive's file in a [`FileLog`]'s directory.
pub const ARCHIVE_FILE: &str = "archive.bin";
/// The replies' file in a [`FileLog`]'s directory.
pub const REPLIES_FILE: &str = "replies.jsonl";

/// The length of the complete records at the start of `archive.bin`'s bytes: each a 4-byte
/// big-endian length and that many bytes. A torn last record is not counted.
pub fn complete_entries_len(bytes: &[u8]) -> usize {
    let mut at = 0usize;
    while let Some(head) = bytes.get(at..at + 4) {
        let n = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if bytes.len() < at + 4 + n {
            break;
        }
        at += 4 + n;
    }
    at
}

/// The length of the complete lines at the start of `replies.jsonl`'s bytes: up to and including
/// the last newline.
pub fn complete_replies_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |at| at + 1)
}

impl FileLog {
    /// The log in `dir`, created if absent. A torn last record in either file (an append that
    /// never completed: the host died inside the write) is cut off first (`T-005-15`): every later
    /// append would otherwise follow it, and a reader that stops at the torn record would never
    /// see them. The archive copies these files byte for byte, and a restore lays them back.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, String> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for (name, complete) in [
            (ARCHIVE_FILE, complete_entries_len as fn(&[u8]) -> usize),
            (REPLIES_FILE, complete_replies_len),
        ] {
            let path = dir.join(name);
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let keep = complete(&bytes);
            if keep < bytes.len() {
                std::eprintln!(
                    "aledgent-core: {name}: a torn last record of {} bytes is cut off",
                    bytes.len() - keep
                );
                let f = OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(|e| e.to_string())?;
                f.set_len(keep as u64).map_err(|e| e.to_string())?;
                f.sync_all().map_err(|e| e.to_string())?;
            }
        }
        Ok(FileLog {
            dir,
            index: Mutex::new(None),
        })
    }

    /// The directory the log lives in.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn append(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(name))
            .map_err(|e| e.to_string())?;
        f.write_all(bytes).map_err(|e| e.to_string())?;
        f.sync_data().map_err(|e| e.to_string())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ReplyLine {
    key: InputKey,
    reply: CoreReply,
}

impl HostLog for FileLog {
    fn entries(&self) -> Result<Vec<RingEntry>, String> {
        let mut bytes = Vec::new();
        match File::open(self.dir.join("archive.bin")) {
            Ok(mut f) => {
                f.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            }
            Err(_) => return Ok(Vec::new()),
        }
        let mut out = Vec::new();
        let mut rest = bytes.as_slice();
        while rest.len() >= 4 {
            let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            if rest.len() < 4 + n {
                break;
            }
            out.push(decode::<RingEntry>(&rest[4..4 + n])?);
            rest = &rest[4 + n..];
        }
        Ok(out)
    }

    fn push_entry(&mut self, e: &RingEntry) -> Result<(), String> {
        self.push_entries(core::slice::from_ref(e))
    }

    fn push_entries(&mut self, entries: &[RingEntry]) -> Result<(), String> {
        let mut bytes = Vec::new();
        for e in entries {
            let body = encode(e);
            bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&body);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        self.append("archive.bin", &bytes)
    }

    fn replies(&self) -> Result<Vec<(InputKey, CoreReply)>, String> {
        let Ok(f) = File::open(self.dir.join("replies.jsonl")) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line.map_err(|e| e.to_string())?;
            match serde_json::from_str::<ReplyLine>(&line) {
                Ok(l) => out.push((l.key, l.reply)),
                Err(_) => break,
            }
        }
        Ok(out)
    }

    fn push_reply(&mut self, key: &InputKey, reply: &CoreReply) -> Result<(), String> {
        self.push_replies(&[(*key, reply.clone())])
    }

    fn push_replies(&mut self, replies: &[(InputKey, CoreReply)]) -> Result<(), String> {
        let mut bytes = Vec::new();
        for (key, reply) in replies {
            serde_json::to_writer(
                &mut bytes,
                &ReplyLine {
                    key: *key,
                    reply: reply.clone(),
                },
            )
            .map_err(|e| e.to_string())?;
            bytes.push(b'\n');
        }
        if bytes.is_empty() {
            return Ok(());
        }
        self.append("replies.jsonl", &bytes)
    }

    /// One reply, through the index (`T-032-15`, the burst of 2026-10-04): the default scanned
    /// and parsed the whole file for every lookup, and a backlog's retries, each looking up keys
    /// answered long before, held the core for 78 s a decision, past ingress's 60 s timeout, so it
    /// retried again and the core stopped draining.
    fn reply(&self, key: &InputKey) -> Result<Option<CoreReply>, String> {
        use std::io::{Seek as _, SeekFrom};
        let path = self.dir.join("replies.jsonl");
        let Ok(mut f) = File::open(&path) else {
            return Ok(None);
        };
        let mut guard = lock(&self.index);
        let index = guard.get_or_insert_with(ReplyIndex::default);
        // Catch up with what was appended since the last lookup; a torn last line is left for
        // later, as `replies` leaves it.
        let len = f.metadata().map_err(|e| e.to_string())?.len();
        if len > index.end {
            f.seek(SeekFrom::Start(index.end))
                .map_err(|e| e.to_string())?;
            let mut tail = Vec::new();
            f.read_to_end(&mut tail).map_err(|e| e.to_string())?;
            let mut at = 0usize;
            while let Some(n) = tail[at..].iter().position(|b| *b == b'\n') {
                let line = &tail[at..at + n];
                match serde_json::from_slice::<ReplyLine>(line) {
                    Ok(l) => {
                        index.at.insert(l.key, (index.end + at as u64, n));
                    }
                    Err(_) => break,
                }
                at += n + 1;
            }
            index.end += at as u64;
        }
        let Some(&(offset, n)) = index.at.get(key) else {
            return Ok(None);
        };
        drop(guard);
        f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut line = vec![0u8; n];
        f.read_exact(&mut line).map_err(|e| e.to_string())?;
        serde_json::from_slice::<ReplyLine>(&line)
            .map(|l| Some(l.reply))
            .map_err(|e| e.to_string())
    }
}

/// What the book is founded on, as the host keeps it: the genesis the Agent took, the keys it
/// runs over (`F-035`: a key custody and a clerk, in production the signer service's clients, so
/// no key is in this process), and the Agent's limits.
#[derive(Clone)]
pub struct Founding {
    /// The genesis configuration.
    pub genesis: Genesis,
    /// Genesis's operation reference.
    pub op: OperationId,
    /// Genesis's time.
    pub at: Time,
    /// The key custody and the clerk.
    pub keys: crate::host::HostKeys,
    /// The Agent's limits.
    pub limits: Limits,
    /// For a chainspec book (`F-036`): the genesis roots the deployment's manifest states
    /// (`aledgent-core-host build-spec`). The core rebuilds the genesis state from the chainspec
    /// on every start and refuses to start if its roots are not these. `None`: no manifest.
    pub genesis_roots: Option<crate::envelope::Roots>,
}

impl core::fmt::Debug for Founding {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Founding")
            .field("op", &self.op)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

/// For a chainspec book (`F-036`, owner ruling `genesis-chainspec`): the genesis this host just
/// rebuilt from the chainspec must stand at the roots the deployment's manifest states, or the
/// core does not start. The Agent's own roots were checked against the same rebuild already
/// (`Driver::found_in`, `Driver::recover_in`).
fn check_genesis<P: AgentPort>(founding: &Founding, driver: &Driver<P>) -> Result<(), String> {
    match founding.genesis_roots {
        Some(want) if want != driver.genesis_roots() => Err(format!(
            "the genesis rebuilt from the chainspec stands at {:?}, not at the manifest's {want:?}: \
             the core does not start (F-036)",
            driver.genesis_roots()
        )),
        _ => Ok(()),
    }
}

/// Where a roll-forward left the Agent ([`roll_forward_agent`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RolledForward {
    /// The Agent's input number before: what its checkpoint held, 0 when it was founded afresh.
    pub from: u64,
    /// Its input number after: the archive's last entry.
    pub to: u64,
    /// Whether the Agent was founded afresh (no checkpoint).
    pub founded: bool,
    /// Its roots after, as its hot row reads.
    pub roots: crate::envelope::Roots,
    /// Its log head after.
    pub log_head: [u8; 32],
}

/// Bring the Agent behind `port` to the end of `archive` (`T-005-16`, a restore from the
/// off-node archive, `ops/runbooks/restore.md`).
///
/// - An Agent that holds a checkpoint (a runtime state the archive saved) must stand on the
///   archive's chain: its input number at most the archive's, its log head the archived entry's
///   at that number. The replica is rebuilt from the archive up to there and must reach the
///   Agent's roots ([`Driver::recover_in`]).
/// - An empty Agent is founded afresh on `founding`, exactly as at genesis.
///
/// Either way the inputs after are re-sent through [`Driver::roll_forward`], each checked against
/// its archived entry, and the Agent must end at the archive's last log hash and roots. The host
/// writes nothing: `archive` is already its log. Returns the port, and where the Agent stands.
pub fn roll_forward_agent<P: AgentPort>(
    mut port: P,
    founding: &Founding,
    archive: &[RingEntry],
    stores: &ReplicaStores,
) -> Result<(P, RolledForward), String> {
    let f = founding;
    let held = port.hot().map_err(|e| format!("the Agent: {e:?}"))?;
    let founded = held.is_none();
    let (mut driver, mut replica, from) = match held {
        Some(hot) => {
            let h = usize::try_from(hot.inputs).map_err(|_| "the Agent's input number")?;
            if h > archive.len() {
                return Err(format!(
                    "the checkpoint stands at input {h}, past the archive's {}: it is not of this archive",
                    archive.len()
                ));
            }
            if h > 0 && archive[h - 1].log_hash() != hot.log_head {
                return Err(format!(
                    "the checkpoint's log head at input {h} is not the archive's: it is not of this archive"
                ));
            }
            let mut prefix = archive[..h].to_vec();
            let (driver, replica) = Driver::recover_in(
                stores,
                port,
                f.keys.clone(),
                f.genesis.clone(),
                f.op,
                f.at,
                &mut prefix,
            )
            .map_err(|e| format!("the checkpoint: {e:?}"))?;
            (driver, replica, hot.inputs)
        }
        None => {
            let (driver, replica, _) = Driver::found_in(
                stores,
                port,
                f.keys.clone(),
                f.genesis.clone(),
                f.op,
                f.at,
                f.limits,
            )
            .map_err(|e| format!("genesis: {e:?}"))?;
            (driver, replica, 0)
        }
    };
    check_genesis(f, &driver)?;
    let rest = &archive[usize::try_from(from)
        .unwrap_or(usize::MAX)
        .min(archive.len())..];
    driver
        .roll_forward(&mut replica, rest)
        .map_err(|e| format!("roll forward: {e:?}"))?;
    let mut port = driver.into_port();
    let hot = port
        .hot()
        .map_err(|e| format!("the Agent: {e:?}"))?
        .ok_or("the Agent is not founded")?;
    let (want_seq, want_head, want_roots) = match archive.last() {
        Some(e) => (e.seq, e.log_hash(), Some(e.roots)),
        None => (0, [0; 32], None),
    };
    if hot.inputs != want_seq
        || hot.log_head != want_head
        || want_roots.is_some_and(|r| r != hot.roots())
    {
        return Err(format!(
            "the Agent stands at input {} and the archive ends at {want_seq}, or their roots differ",
            hot.inputs
        ));
    }
    Ok((
        port,
        RolledForward {
            from,
            to: hot.inputs,
            founded,
            roots: hot.roots(),
            log_head: hot.log_head,
        },
    ))
}

/// How many times in a row the Agent may fail one input alone, definitively (it refused the
/// invocation, or the runtime aborted it: a panic, out of gas), before the core halts on it
/// (finding F18 of `F-032`). Each failure rebuilds the replica from the archive, which grows with
/// the book; retried for ever, as before, the core replayed its whole archive on every retry and
/// stalled. A definitive failure does not change on a retry with the same build, so the second
/// confirms the first.
///
/// **The halt.** The core stops as on an incident: it says why on standard error (an
/// `aledgent-core: ALERT` line naming the input) and in its report (`halted=1`), answers every
/// later input `Unavailable` without running it or rebuilding, and keeps answering reads at what
/// the Agent committed. Inputs stay `Received` in ingress's log, in order (`REQ-CS-7`). Nothing
/// skips the input or applies it another way (`REQ-AT-3`, `REQ-RT-13`): the operator restarts
/// the core host with a build whose Agent applies it, and the core resumes from there.
pub const FAILED_ATTEMPTS: u32 = 2;

/// How many replies the core keeps in memory: far more than the forwarders can have in flight
/// (`REPLY_TAIL` in `ingress::inprocess` is 64). An older one is read from the log (`T-031-14`).
pub const RECENT_REPLIES: usize = 4_096;

/// What the core reports about its own work (`T-032-11`, stress findings F14 and F15): how long
/// reads waited for the executor, how long each decision held it, and that hold's parts. Windows
/// are drained by [`CoreStats::report`]; counters only grow.
#[derive(Default)]
pub struct CoreStats {
    /// Reads answered (queries, credentials, ties, the outbox, the clock).
    pub reads: AtomicU64,
    /// Of them, credentials answered from the committed credentials, without the gate.
    pub committed_reads: AtomicU64,
    /// Decisions made (one batch or one input each).
    pub decisions: AtomicU64,
    /// Inputs those decisions carried.
    pub inputs: AtomicU64,
    /// 1 once the core has halted on an input the Agent cannot apply ([`FAILED_ATTEMPTS`]).
    pub halted: AtomicU64,
    /// 1 once the core has stopped for any reason: the halt above, or an incident (a divergence,
    /// an archive or a reply log that does not write). Every later input is `Unavailable`.
    pub stopped: AtomicU64,
    /// The runtime's unacknowledged Authority decisions, after the last decision; `u64::MAX`
    /// while the port does not count them ([`AgentPort::unacknowledged`]).
    pub unacknowledged: AtomicU64,
    /// While the writer is fenced (`T-037-15`): the newest time input offered to the core when the
    /// fence first refused an input since an input last committed; 0 when it is not fenced. The signals report how long
    /// ago that was (`aledgent_writer_fenced_seconds`, `NFR-11`).
    pub fenced_since: AtomicU64,
    /// The writer's code as of the last input committed (`T-037-23`): its version, generation and
    /// scheduled upgrade, kept here so the signals read it without the execution lock, and never
    /// ahead of a commit. `None` until the driver is open.
    pub writer_code: Mutex<Option<crate::fence::WriterCode>>,
    read_wait_us: Window,
    hold_us: Window,
    native_us: Window,
    flush_us: Window,
    log_us: Window,
}

/// The samples of one series since the last report.
#[derive(Default)]
struct Window(Mutex<Vec<u64>>);

impl Window {
    fn push(&self, v: u64) {
        let mut w = lock(&self.0);
        // Bounded between two reports: a report every few seconds keeps far fewer.
        if w.len() < 100_000 {
            w.push(v);
        }
    }

    /// `n p50/p99/max`, microseconds, and empty.
    fn take(&self) -> String {
        let mut v = core::mem::take(&mut *lock(&self.0));
        if v.is_empty() {
            return "0".into();
        }
        v.sort_unstable();
        let at = |pct: usize| v[(v.len() * pct).div_ceil(100).saturating_sub(1)];
        format!("{} {}/{}/{}", v.len(), at(50), at(99), v[v.len() - 1])
    }
}

fn micros(t: std::time::Instant) -> u64 {
    u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX)
}

impl CoreStats {
    /// One line: the counters, and each window's count and p50/p99/max in µs since the last
    /// report.
    pub fn report(&self) -> String {
        format!(
            "reads={} committed_reads={} decisions={} inputs={} halted={} read_wait_us=[{}] hold_us=[{}] native_us=[{}] flush_us=[{}] log_us=[{}]",
            self.reads.load(Ordering::Relaxed),
            self.committed_reads.load(Ordering::Relaxed),
            self.decisions.load(Ordering::Relaxed),
            self.inputs.load(Ordering::Relaxed),
            self.halted.load(Ordering::Relaxed),
            self.read_wait_us.take(),
            self.hold_us.take(),
            self.native_us.take(),
            self.flush_us.take(),
            self.log_us.take(),
        )
    }
}

/// The replies (`INV-18`): every key answered, the most recent replies in memory, the rest in the
/// log. Before `T-031-14` every reply stayed in memory for ever, about 1.3 KB a command.
#[derive(Default)]
struct Replies {
    known: HashSet<InputKey>,
    recent: HashMap<InputKey, CoreReply>,
    order: VecDeque<InputKey>,
}

impl Replies {
    fn insert(&mut self, key: InputKey, reply: CoreReply) {
        self.known.insert(key);
        if self.recent.insert(key, reply).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > RECENT_REPLIES {
            if let Some(old) = self.order.pop_front() {
                self.recent.remove(&old);
            }
        }
    }

    /// The reply to `key`, if it was answered: from memory, or from `log`. `Err`: answered, but
    /// the log cannot be read; the input must not run again.
    fn get(&self, key: &InputKey, log: &dyn HostLog) -> Result<Option<CoreReply>, String> {
        if let Some(r) = self.recent.get(key) {
            return Ok(Some(r.clone()));
        }
        if !self.known.contains(key) {
            return Ok(None);
        }
        match log.reply(key)? {
            Some(r) => Ok(Some(r)),
            None => Err("an answered input's reply is not in the log".into()),
        }
    }
}

/// The replica's executor: what reads are answered from.
type Exec = Running<HostStore, HostVault>;

struct Inner<P: AgentPort> {
    /// The executor, moved here from [`Parts::running`] while an input runs (and only then).
    running: Option<Exec>,
    driver: Option<Driver<P>>,
    founded: Founded,
    founding: Founding,
    log: Box<dyn HostLog>,
    replies: Replies,
    /// Where the replica's stores live (`T-031-14`), for every rebuild.
    stores: ReplicaStores,
    next_seq: u64,
    stopped: Option<String>,
    /// The driver's batch bounds, kept across rebuilds.
    batch: BatchLimits,
    /// The core's measurements, shared with [`Parts`].
    stats: Arc<CoreStats>,
    /// Credentials the last decision created, for [`Credentials`] (`T-032-13`).
    fresh: Vec<String>,
    /// The input the Agent last failed on alone, definitively, and how many times in a row
    /// (finding F18 of `F-032`, [`FAILED_ATTEMPTS`]).
    failing: Option<(InputKey, u32)>,
    /// The audit trail's shipping (`T-013-13`): the floor every rebuild drops up to.
    audit: Option<AuditShared>,
    /// A `verify-genesis` segment's chainspec and first chunk: what a rebuild applies natively
    /// before the segment's archive ([`Driver::recover_segment`]).
    segment: Option<(ledger_core::chainspec::SpecRef, u32)>,
    /// The node's own trail in the audit store (`T-037-15`, amendment 0033 D25), where an input
    /// the fence refused is audited, never in the book. Beside it, always, a line on standard
    /// error.
    node_trail: Option<audit_store::NodeTrail>,
    /// The latest time input of any input offered to the core (`T-037-15`): where the fenced clock
    /// starts, so that an input logged long before it was refused does not age the fence.
    latest_time: Time,
}

/// The core's two locks (`T-031-12`, finding F3 of `F-032`). An input takes `ctl`, then the
/// executor exclusively, for the whole of its run: the replica runs it, the Agent commits it (or
/// the replica is rebuilt), and only then is the executor put back. A read takes only the
/// executor, shared, through a [`Gate`] that lets a waiting input in before any read that arrives
/// after it. So reads run side by side, a read storm cannot starve decisions, and a read never
/// sees a state the Agent has not committed (`DEC-4`). The order is always `ctl`, then `running`.
struct Parts<P: AgentPort> {
    ctl: Mutex<Inner<P>>,
    running: Gate<Option<Exec>>,
    stats: Arc<CoreStats>,
    committed: Mutex<Credentials>,
    /// The one thread every decision runs on ([`decider`]).
    decider: std::sync::mpsc::Sender<Job>,
    /// The audit trail's shipping (`T-013-13`), when the core ships.
    audit: Option<AuditShared>,
    /// The shipper's wake, once its thread runs.
    ship: OnceLock<ShipHandle>,
    /// A chainspec book's `verify-genesis` gate (`T-036-25`), once the host sets it.
    genesis_gate: OnceLock<crate::verified::GenesisGate>,
    /// Where this node fetches code from, once the host sets it (`T-037-04`): without one, no
    /// upgrade can be staged, so none is logged.
    code_source: OnceLock<Arc<dyn crate::staging::CodeFetch>>,
}

/// A decision, sent to the decider thread.
type Job = Box<dyn FnOnce() + Send>;

/// The thread every decision runs on (`T-032-15`, stress finding F15). Decisions are serial
/// anyway (`ctl`); on one thread, the runtime's per-thread preparation of the actor stays warm
/// and exists once. On the async runtime's blocking pool, each of its threads that happened to
/// run a decision prepared and kept its own copy, and a new one paid the preparation again.
fn decider() -> std::sync::mpsc::Sender<Job> {
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("core-decider".into())
        .spawn(move || {
            for job in rx {
                // A panicking decision answers nothing (its caller sees the channel close) and
                // does not take the thread with it.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }
        })
        .unwrap_or_else(|e| panic!("the decider thread does not start: {e}"));
    tx
}

/// The most credentials [`Credentials`] keeps: past it, it starts again.
const COMMITTED_CREDENTIALS: usize = 4_096;

/// Credentials as the Agent last committed them (`T-032-13`, stress finding F14): every answer
/// [`Core::credential`] gave, refreshed from the replica at the end of every decision, while the
/// executor is still held exclusively, so the cache is always exactly the committed book. A
/// credential asked before is answered from here without waiting for the decision in flight
/// (on the Agent, one invocation); one never asked is read through the gate once, and inserted
/// while the read still holds it, so no commit falls between the read and the insert. An API
/// key's acknowledgement no longer waits for the core, and a revocation still takes effect at the
/// commit that made it (`NFR-20`, `REQ-AU-3`).
#[derive(Default)]
struct Credentials(HashMap<String, Option<CredentialBody>>);

impl Credentials {
    fn refresh(&mut self, exec: &Exec) {
        for (id, found) in self.0.iter_mut() {
            *found = exec.credential(id);
        }
    }

    fn insert(&mut self, id: String, found: Option<CredentialBody>) {
        if self.0.len() >= COMMITTED_CREDENTIALS && !self.0.contains_key(&id) {
            self.0.clear();
        }
        self.0.insert(id, found);
    }
}

/// ingress's [`Core`] over the core Agent (module documentation). Cloning shares the one core.
pub struct AgentCore<P: AgentPort> {
    inner: Arc<Parts<P>>,
}

impl<P: AgentPort> Clone for AgentCore<P> {
    fn clone(&self) -> Self {
        AgentCore {
            inner: Arc::clone(&self.inner),
        }
    }
}

fn founded_of(replica: &crate::host::Replica, founding: &Founding) -> Founded {
    let roles = replica.state().roles().expect("a founded book has roles");
    Founded {
        operator: roles.operator,
        operator_credential: ledger_core::id::CredentialId::mint(&founding.op, 1),
        issuance: roles.issuance,
    }
}

impl<P: AgentPort + Send + 'static> AgentCore<P> {
    /// Open the core over `port`: found the Agent if it is empty, or rebuild the replica from the
    /// log and the Agent if it is not. The replica's stores are in memory.
    pub fn open(port: P, founding: Founding, log: impl HostLog) -> Result<Self, String> {
        AgentCore::open_in(port, founding, log, ReplicaStores::Memory)
    }

    /// [`AgentCore::open`], with the replica's stores where `stores` says (`T-031-14`):
    /// `aledgent-core-host serve --replica-on-disk` keeps them on disk under its state directory.
    pub fn open_in(
        port: P,
        founding: Founding,
        log: impl HostLog,
        stores: ReplicaStores,
    ) -> Result<Self, String> {
        AgentCore::open_with(port, founding, log, stores, None, None)
    }

    /// A scratch core for one **segment** of `verify-genesis` (`--segments`, `F-036`): the Agent
    /// founded on the chainspec's commitment alone (`founding.genesis.spec` is `None`) with chunks
    /// `0..start` applied natively first ([`Driver::found_at_chunk`]); it then takes chunks
    /// `start..` as inputs. Never reopened: a segment's core is in memory and lives for one run.
    /// It keeps `spec` to rebuild its replica as it was founded, should the Agent not commit an
    /// invocation ([`Driver::recover_segment`]).
    pub fn open_segment(
        port: P,
        founding: Founding,
        log: impl HostLog,
        stores: ReplicaStores,
        spec: &ledger_core::chainspec::SpecRef,
        start: u32,
    ) -> Result<Self, String> {
        AgentCore::open_with(port, founding, log, stores, None, Some((spec, start)))
    }

    /// [`AgentCore::open_in`], shipping the audit trail to `sink`, the audit store, from a thread
    /// of its own (`T-013-13`, module documentation), and refusing new inputs while
    /// `max_unshipped` records wait for it. The replica is rebuilt dropping what the store
    /// already holds.
    pub fn open_audited(
        port: P,
        founding: Founding,
        log: impl HostLog,
        stores: ReplicaStores,
        sink: Box<dyn ledger_core::audit::AuditSink + Send>,
        max_unshipped: u64,
    ) -> Result<Self, String> {
        let core = AgentCore::open_audited_unstarted(
            port,
            founding,
            log,
            stores,
            sink.head()
                .map_err(|e| format!("the audit store's head does not read: {e:?}"))?,
            max_unshipped,
        )?;
        let shared = core.audit().ok_or("the audit state is missing")?;
        let outbox = AgentOutbox(Arc::downgrade(&core.inner));
        let handle =
            Shipper::new(sink, shared).spawn(Box::new(outbox), ingress::audit_ship::SHIP_INTERVAL);
        let _ = core.inner.ship.set(handle);
        Ok(core)
    }

    /// [`AgentCore::open_audited`] without its thread: the caller ships with [`AgentCore::ship`]
    /// (tests that step the shipper themselves). `head` is the store's head.
    pub fn open_audited_unstarted(
        port: P,
        founding: Founding,
        log: impl HostLog,
        stores: ReplicaStores,
        head: Option<Head>,
        max_unshipped: u64,
    ) -> Result<Self, String> {
        AgentCore::open_with(
            port,
            founding,
            log,
            stores,
            Some(AuditShared::new(head, max_unshipped)),
            None,
        )
    }

    /// Audit what the execution node refuses without the book (an input the fence refused,
    /// `T-037-15`) in the audit store's node trail too, beside the line on standard error that
    /// every refusal has.
    pub fn set_node_trail(&self, trail: audit_store::NodeTrail) {
        lock(&self.inner.ctl).node_trail = Some(trail);
    }

    /// The audit shipping's shared state, when the core ships.
    pub fn audit(&self) -> Option<AuditShared> {
        self.inner.audit.clone()
    }

    /// Run `shipper` over this core's outbox until it is empty or a round fails (tests, and a
    /// host that ships on its own schedule).
    pub fn ship(
        &self,
        shipper: &mut Shipper,
    ) -> Option<Result<u64, ingress::audit_ship::ShipError>> {
        shipper.drain(&AgentOutbox(Arc::downgrade(&self.inner)))
    }

    fn open_with(
        mut port: P,
        founding: Founding,
        log: impl HostLog,
        stores: ReplicaStores,
        audit: Option<AuditShared>,
        segment: Option<(&ledger_core::chainspec::SpecRef, u32)>,
    ) -> Result<Self, String> {
        let floor = audit.as_ref().and_then(AuditShared::floor);
        let mut log: Box<dyn HostLog> = Box::new(log);
        let founded_already = port.hot().map_err(|e| format!("{e:?}"))?.is_some();
        if founded_already && segment.is_some() {
            return Err(
                "a verify-genesis segment founds a fresh Agent; this one holds state".into(),
            );
        }
        let (mut driver, replica, founded) = if let Some((spec, start)) = segment {
            Driver::found_at_chunk(
                &stores,
                port,
                founding.keys.clone(),
                founding.genesis.clone(),
                founding.op,
                founding.at,
                founding.limits,
                spec,
                start,
            )
            .map_err(|e| format!("{e:?}"))?
        } else if founded_already {
            let mut archive = log.entries()?;
            let archived = archive.len();
            let (driver, replica) = Driver::recover_audited(
                &stores,
                port,
                founding.keys.clone(),
                founding.genesis.clone(),
                founding.op,
                founding.at,
                &mut archive,
                floor,
            )
            .map_err(|e| format!("{e:?}"))?;
            // What the Agent committed beyond the log (the host died before archiving it) is
            // archived now, before the Agent is told so.
            for e in &archive[archived..] {
                log.push_entry(e)?;
            }
            let founded = founded_of(&replica, &founding);
            (driver, replica, founded)
        } else {
            // An Agent that holds nothing over a log that holds inputs is a lost runtime state (or
            // a restore laid out and not finished): founding afresh would serve an empty book
            // beside the old replies. Only a restore brings that Agent back (`T-005-16`).
            let archived = log.entries()?.len();
            if archived > 0 {
                return Err(format!(
                    "the Agent holds no state and the log holds {archived} committed inputs: \
                     restore it from the archive (aledgent-core-host restore), never found it afresh"
                ));
            }
            Driver::found_in(
                &stores,
                port,
                founding.keys.clone(),
                founding.genesis.clone(),
                founding.op,
                founding.at,
                founding.limits,
            )
            .map_err(|e| format!("{e:?}"))?
        };
        check_genesis(&founding, &driver)?;
        // The artefact check at start (`T-037-14`): the platform runs the code that executes the
        // next input. A failure does not stop the host from opening: the same call precedes every
        // send and refuses the input, while reads and a way-out input (which never needs the
        // version it leaves) stay possible.
        if let Err(e) = driver.ensure_code() {
            std::eprintln!(
                "aledgent-core: the code of the next input cannot be run yet, inputs that need it \
                 are not sent: {e:?}"
            );
        }
        driver.archived();
        let mut inner = Inner {
            running: None,
            driver: Some(driver),
            founded,
            founding,
            log,
            replies: Replies::default(),
            stores,
            next_seq: 1,
            stopped: None,
            batch: BatchLimits::DEFAULT,
            stats: Arc::new(CoreStats::default()),
            fresh: Vec::new(),
            failing: None,
            audit,
            segment: segment.map(|(spec, start)| (spec.clone(), start)),
            node_trail: None,
            latest_time: 0,
        };
        // The code the writer is in, and the platform's check at start (`T-037-23`).
        inner.report_code();
        let mut running = inner.executor(replica)?;
        // A book founded here emitted genesis's records: the store may hold them already.
        if let Some(f) = floor {
            running.acknowledge_audit_floor(Some(f));
        }
        if let Some(a) = &inner.audit {
            let (n, oldest) = running.audit_outbox();
            a.stats.observe(n, oldest);
        }
        let inner_audit = inner.audit.clone();
        for (key, reply) in inner.log.replies()? {
            inner.next_seq = inner.next_seq.max(reply.input_seq + 1);
            inner.replies.insert(key, reply);
        }
        Ok(AgentCore {
            inner: Arc::new(Parts {
                stats: Arc::clone(&inner.stats),
                ctl: Mutex::new(inner),
                running: Gate::new(Some(running)),
                committed: Mutex::new(Credentials::default()),
                decider: decider(),
                audit: inner_audit,
                ship: OnceLock::new(),
                genesis_gate: OnceLock::new(),
                code_source: OnceLock::new(),
            }),
        })
    }

    /// The roots of the book's genesis as this host rebuilt them (`F-036`); `None` while the core
    /// is not running.
    pub fn genesis_roots(&self) -> Option<crate::envelope::Roots> {
        lock(&self.inner.ctl)
            .driver
            .as_ref()
            .map(Driver::genesis_roots)
    }

    /// Gate commands on a passing `verify-genesis` report in `state` (`T-036-25`, rulings
    /// `a0022-q2`, `a0022-q2b`, [`crate::verified`]): for a book whose genesis commits a chainspec,
    /// [`Core::genesis_open`] answers `false` until [`crate::verified::VERIFIED_FILE`] in `state`
    /// names that chainspec and the genesis roots this host rebuilt. Returns the gate's state now,
    /// or `None` for a book that commits no chainspec (nothing to gate).
    pub fn gate_on_verified_genesis(
        &self,
        state: &std::path::Path,
    ) -> Result<Option<Result<crate::verified::Report, String>>, String> {
        let (commitment, roots) = {
            let inner = lock(&self.inner.ctl);
            let Some(c) = inner.founding.genesis.chainspec else {
                return Ok(None);
            };
            let roots = inner
                .driver
                .as_ref()
                .map(Driver::genesis_roots)
                .ok_or("the core is not running")?;
            (c, roots)
        };
        let gate = crate::verified::GenesisGate::new(state, commitment, roots);
        let now = gate.read();
        if now.is_ok() {
            gate.is_open();
        }
        self.inner
            .genesis_gate
            .set(gate)
            .map_err(|_| "the verify-genesis gate is set already".to_string())?;
        Ok(Some(now))
    }

    /// Set where this node fetches code from, to stage an `Upgrade`'s code version before the
    /// upgrade is logged (`T-037-04`, [`Core::stage_code`]). Set once.
    ///
    /// # Errors
    ///
    /// A source was set already.
    pub fn set_code_source(
        &self,
        source: Arc<dyn crate::staging::CodeFetch>,
    ) -> Result<(), String> {
        self.inner
            .code_source
            .set(source)
            .map_err(|_| "the code source is set already".to_string())
    }

    /// What genesis created.
    pub fn founded(&self) -> Founded {
        lock(&self.inner.ctl).founded
    }

    /// The core's own measurements (`T-032-11`).
    pub fn stats(&self) -> Arc<CoreStats> {
        Arc::clone(&self.inner.stats)
    }

    /// Whether the core stopped on an incident, and why.
    pub fn stopped(&self) -> Option<String> {
        lock(&self.inner.ctl).stopped.clone()
    }

    /// Set the batch bounds (`T-031-03`): how many inputs [`Core::apply_batch`] sends the Agent in
    /// one invocation. [`BatchLimits::SINGLE`] is one input per invocation.
    pub fn set_batch_limits(&self, limits: BatchLimits) {
        let mut inner = lock(&self.inner.ctl);
        inner.batch = limits;
        if let Some(d) = inner.driver.as_mut() {
            d.set_batch_limits(limits);
        }
    }

    /// Run `f` over the port (tests: fault injection, reading the Agent's state).
    pub fn with_port<T>(&self, f: impl FnOnce(&mut P) -> T) -> Option<T> {
        lock(&self.inner.ctl)
            .driver
            .as_mut()
            .map(|d| f(d.port_mut()))
    }

    fn apply_now(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        let mut inner = lock(&self.inner.ctl);
        match inner.replies.get(&input.key, &*inner.log) {
            Ok(Some(r)) => return Ok(r),
            Ok(None) => {}
            Err(_) => return Err(CoreError::Unavailable),
        }
        if inner.stopped.is_some() {
            return Err(CoreError::Unavailable);
        }
        self.decide(&mut inner, |inner| inner.apply(input))
    }

    fn apply_batch_now(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        let mut inner = lock(&self.inner.ctl);
        self.decide(&mut inner, |inner| inner.apply_batch(inputs))
    }

    /// Run `f` with the executor taken out of the gate, holding the gate exclusively until it is
    /// put back: no read sees the replica between an input's native run and the Agent's commit.
    fn decide<T>(&self, inner: &mut Inner<P>, f: impl FnOnce(&mut Inner<P>) -> T) -> T {
        let mut slot = self.inner.running.write();
        let held = std::time::Instant::now();
        inner.running = slot.take();
        let out = f(inner);
        *slot = inner.running.take();
        // What happened to the writer's code and the platform in this decision, in the log, and
        // the signals' view of the writer's code (`T-037-23`).
        inner.report_code();
        // The committed credentials, before any read sees the new state (`Credentials`), with
        // the ones this decision created: their holders' first requests would otherwise each wait
        // for an invocation.
        let fresh = core::mem::take(&mut inner.fresh);
        // The audit outbox after the decision (`T-013-13`), and the shipper woken.
        if let (Some(a), Some(exec)) = (&inner.audit, slot.as_ref()) {
            let (n, oldest) = exec.audit_outbox();
            a.stats.observe(n, oldest);
            if n > 0
                && let Some(handle) = self.inner.ship.get()
            {
                handle.nudge();
            }
        }
        let unacknowledged = inner
            .driver
            .as_ref()
            .and_then(|d| d.port().unacknowledged())
            .unwrap_or(u64::MAX);
        self.inner
            .stats
            .unacknowledged
            .store(unacknowledged, Ordering::Relaxed);
        match slot.as_ref() {
            Some(exec) => {
                let mut committed = lock(&self.inner.committed);
                committed.refresh(exec);
                for id in fresh {
                    let found = exec.credential(&id);
                    committed.insert(id, found);
                }
            }
            None => lock(&self.inner.committed).0.clear(),
        }
        drop(slot);
        self.inner.stats.hold_us.push(micros(held));
        self.inner.stats.decisions.fetch_add(1, Ordering::Relaxed);
        out
    }

    /// Run `f` on the decider thread ([`decider`]); `None` if it did not answer.
    async fn on_decider<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move || {
            let _ = tx.send(f());
        });
        self.inner.decider.send(job).ok()?;
        rx.await.ok()
    }

    /// A read of the executor, off the async runtime's threads: it may wait for an input in
    /// flight, which on the Agent is one invocation.
    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Exec) -> T + Send + 'static,
    ) -> Result<T, CoreError> {
        let core = self.clone();
        tokio::task::spawn_blocking(move || {
            let asked = std::time::Instant::now();
            let guard = core.inner.running.read();
            core.inner.stats.read_wait_us.push(micros(asked));
            core.inner.stats.reads.fetch_add(1, Ordering::Relaxed);
            guard.as_ref().map(f).ok_or(CoreError::Unavailable)
        })
        .await
        .unwrap_or(Err(CoreError::Unavailable))
    }
}

impl<P: AgentPort> Inner<P> {
    fn executor(
        &self,
        replica: crate::host::Replica,
    ) -> Result<Running<HostStore, HostVault>, String> {
        // F-035: the driver's clerk, which records what it signs for the Agent.
        let clerk: Option<SharedClerk> = self.driver.as_ref().and_then(|d| d.clerk());
        let mut running = Running::new(replica, self.founded, clerk);
        for e in self.log.entries()? {
            for (id, at) in &e.deliveries {
                running.restore_delivery(NotificationId(*id), *at);
            }
        }
        Ok(running)
    }

    /// Write one plain line for everything the driver recorded about the writer's code since the
    /// last call, and publish the code the writer is in as of the last commit (`T-037-23`). The
    /// lines name the input; they are for the operator, never in the book, and none is written
    /// while an archive is replayed (the driver records nothing then).
    fn report_code(&mut self) {
        let Some(driver) = self.driver.as_mut() else {
            return;
        };
        for event in driver.take_code_events() {
            std::eprintln!("aledgent-core: {}", describe_code_event(&event));
        }
        if let Some(w) = driver.committed_writer() {
            *lock(&self.stats.writer_code) = Some(w);
        }
    }

    /// Stop on an incident: every later call is `Unavailable` until the core is reopened.
    fn stop(&mut self, why: String) -> CoreError {
        self.stopped = Some(why);
        self.stats.stopped.store(1, Ordering::Relaxed);
        CoreError::Unavailable
    }

    /// Rebuild the replica and the executor from the log and the Agent. `Ok(n)`: the Agent's
    /// input number after the rebuild.
    fn rebuild(&mut self) -> Result<u64, CoreError> {
        let Some(mut driver) = self.driver.take() else {
            return Err(CoreError::Unavailable);
        };
        // What the old driver recorded before it is dropped (`T-037-23`).
        for event in driver.take_code_events() {
            std::eprintln!("aledgent-core: {}", describe_code_event(&event));
        }
        let port = driver.into_port();
        let mut archive = match self.log.entries() {
            Ok(a) => a,
            Err(e) => return Err(self.stop(format!("the archive does not read: {e}"))),
        };
        let before = archive.len();
        let f = self.founding.clone();
        let floor = self.audit.as_ref().and_then(AuditShared::floor);
        if self.segment.is_some() {
            // A segment's replica is the book up to its chunk, in memory: let the one that ran
            // ahead of the Agent go before the rebuild makes another.
            self.running = None;
        }
        let recovered = match self.segment.clone() {
            Some((spec, start)) => Driver::recover_segment(
                &self.stores,
                port,
                f.keys.clone(),
                f.genesis.clone(),
                f.op,
                f.at,
                &mut archive,
                &spec,
                start,
            ),
            None => Driver::recover_audited(
                &self.stores,
                port,
                f.keys.clone(),
                f.genesis.clone(),
                f.op,
                f.at,
                &mut archive,
                floor,
            ),
        };
        match recovered {
            Ok((mut driver, replica)) => {
                for e in &archive[before..] {
                    if let Err(e) = self.log.push_entry(e) {
                        return Err(self.stop(format!("the archive does not write: {e}")));
                    }
                }
                driver.archived();
                driver.set_batch_limits(self.batch);
                let seq = driver.seq();
                self.driver = Some(driver);
                self.report_code();
                match self.executor(replica) {
                    Ok(r) => self.running = Some(r),
                    Err(e) => return Err(self.stop(e)),
                }
                Ok(seq)
            }
            Err(e) => Err(self.stop(format!("the replica cannot be rebuilt: {e:?}"))),
        }
    }

    /// The fence refused `input` (`T-037-15`, `crate::fence`): start the fenced clock if it is not
    /// running, and audit the refusal at the execution node, on standard error and in the audit
    /// store's node trail (amendment 0033 D25), never in the book. The line and the record name the
    /// input by key, kind and time, never by its body (`REQ-AU-8`).
    fn fenced(&mut self, input: Option<&CoreInput>, f: &Fenced) {
        // The newest time input the core has been offered, not the refused input's own, which may
        // have waited long before it was first refused. With no time known yet (only seed chunks,
        // which carry none), the clock does not start: it would read as decades.
        let at = input
            .and_then(input_time)
            .unwrap_or(0)
            .max(self.latest_time);
        if at > 0 {
            let _ = self.stats.fenced_since.compare_exchange(
                0,
                at,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        let what = input.map_or_else(|| "an input".to_string(), describe);
        let upgrade = f.upgrade.map_or_else(
            || "none".to_string(),
            |(h, n)| {
                let h: String = h.iter().map(|b| format!("{b:02x}")).collect();
                format!("{h} from input {n}")
            },
        );
        let detail = format!(
            "input {} refused by the fence: the code installed is generation {}, the writer's \
             state names generation {} (scheduled upgrade: {upgrade}); {what}",
            f.seq, f.code_generation, f.generation
        );
        std::eprintln!("aledgent-core: AUDIT: {detail}");
        if let Some(trail) = self.node_trail.as_mut()
            && let Err(e) = trail.append(at, "fenced", f.seq, &detail)
        {
            std::eprintln!(
                "aledgent-core: WARNING: the audit store's node trail did not take the fence's \
                 refusal of input {}: {e:?}",
                f.seq
            );
        }
    }

    /// The Agent failed `input` alone, definitively: count it, and halt at [`FAILED_ATTEMPTS`]
    /// in a row (finding F18 of `F-032`). The replica has been rebuilt to what the Agent
    /// committed, so reads go on answering from there.
    fn failed_alone(&mut self, input: &CoreInput, why: &str) {
        if self.stopped.is_some() {
            return;
        }
        let count = match self.failing {
            Some((key, n)) if key == input.key => n + 1,
            _ => 1,
        };
        self.failing = Some((input.key, count));
        if count < FAILED_ATTEMPTS {
            return;
        }
        let what = format!(
            "the Agent cannot apply {}: {count} attempts alone, each committed nothing; the last: {why}",
            describe(input)
        );
        std::eprintln!(
            "aledgent-core: ALERT: the core halted, {what}. Every later input waits, Received, \
             until the core host is restarted with a build whose Agent applies it; nothing skips \
             or forces an input (REQ-AT-3)."
        );
        self.stats.halted.store(1, Ordering::Relaxed);
        self.stop(format!("halted: {what}"));
    }

    fn apply(&mut self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        let (mut out, _) = self.run(core::slice::from_ref(input));
        out.pop().unwrap_or(Err(CoreError::Unavailable))
    }

    /// A batch (`T-031-03`, module documentation): one invocation for the inputs it can carry,
    /// then, after a failure, the inputs the Agent did not commit one at a time, in order, until
    /// one fails. A faulty input then fails alone, and the inputs before it commit: the hybrid
    /// isolation ruled for the settlement contract's batches (#102), applied to the core. Inputs
    /// are never committed out of order: everything after the first failure answers
    /// `Unavailable`.
    ///
    /// **One invocation per call.** The inputs past what one invocation carries
    /// ([`Driver::batch_cap`]: three reservations under stock VOS's actor gas) answer
    /// `Unavailable` without running, and the forwarder sends them again at once. Running every
    /// invocation of a large batch before answering held each reply for the whole batch (a
    /// decided reply took 9 s p50 at a tenth of the reference hour, stress run 37107506920) and kept
    /// every read waiting on the lock for as long.
    fn apply_batch(&mut self, all: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        // An input the Agent has failed alone goes alone again, first: a batch with it would fail
        // and rebuild the replica once more for nothing (finding F18). If it commits, the rest go
        // on as a batch.
        if let (Some((key, _)), Some(head)) = (self.failing, all.first())
            && key == head.key
        {
            let (mut one, _) = self.run(&all[..1]);
            let head = one.pop().unwrap_or(Err(CoreError::Unavailable));
            let ok = head.is_ok();
            let mut out = vec![head];
            if ok && all.len() > 1 {
                out.extend(self.apply_chunk(&all[1..]));
            }
            let rest = not_reached(&out);
            out.resize_with(all.len(), || Err(rest));
            return out;
        }
        self.apply_chunk(all)
    }

    /// One invocation's inputs of `all`, as [`Inner::apply_batch`] says.
    fn apply_chunk(&mut self, all: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        // At most one invocation's inputs between two archive acknowledgements, so the Agent's
        // ring holds them (`crate::hot::Limits::DEFAULT`).
        let chunk = self.driver.as_ref().map_or(1, Driver::batch_cap);
        let inputs = &all[..chunk.min(all.len())];
        let mut out = Vec::with_capacity(all.len());
        let mut at = 0;
        while at < inputs.len() {
            let end = (at + chunk).min(inputs.len());
            let (mut part, rest) = self.run(&inputs[at..end]);
            let mut ok = true;
            if let Some(first) = rest {
                let len = part.len();
                if len == 1 {
                    ok = part[0].is_ok();
                }
                for k in (first..len).filter(|_| len > 1) {
                    let (mut one, _) = self.run(core::slice::from_ref(&inputs[at + k]));
                    part[k] = one.pop().unwrap_or(Err(CoreError::Unavailable));
                    if part[k].is_err() {
                        ok = false;
                        break;
                    }
                }
            }
            out.extend(part);
            at = end;
            if !ok {
                break;
            }
        }
        // What was not reached answers as the last input reached did (amendment 0036, N4): after
        // the fence's refusal, `NotRun`; otherwise `Unavailable`.
        let rest = not_reached(&out);
        out.resize_with(all.len(), || Err(rest));
        out
    }

    /// Run `inputs` as one batch. Returns every input's answer and, if some did not commit, the
    /// index of the first: it and everything after it answer `Unavailable`, or `NotRun` when the
    /// fence refused it (`T-037-18`, amendment 0036): the Agent's refusal as fenced, read after
    /// the replica was rebuilt to what the Agent committed, and nothing else. The record of
    /// replies was read for every input before it was staged, so an input the core decided is
    /// `Ok` here, never `NotRun`.
    fn run(&mut self, inputs: &[CoreInput]) -> (Vec<Result<CoreReply, CoreError>>, Option<usize>) {
        let mut out: Vec<Result<CoreReply, CoreError>> =
            inputs.iter().map(|_| Err(CoreError::Unavailable)).collect();
        if let Some(t) = inputs.iter().filter_map(input_time).max() {
            self.latest_time = self.latest_time.max(t);
        }
        // Back-pressure (`T-013-13`): the audit store has not taken records, and the outbox is at
        // its bound. Nothing runs; every input waits `Received`, in order, and is retried.
        if self.audit.as_ref().is_some_and(|a| a.stats.full()) {
            return (out, Some(0));
        }
        // What each input came to natively, and the input number it was staged under, if it
        // staged anything; in order.
        let mut runs: Vec<(usize, Outcome, Option<u64>)> = Vec::new();
        let mut failed: Option<(usize, DriveError)> = None;
        let mut down: Option<usize> = None;
        // The call ended in the fence's refusal (amendment 0036): what did not commit is `NotRun`.
        let mut fenced_out = false;
        self.stats
            .inputs
            .fetch_add(inputs.len() as u64, Ordering::Relaxed);
        let native = std::time::Instant::now();
        // The kernel leaves reads loaded since the last decision are not this decision's
        // (`T-032-15`): the replica's recording store keeps every load, and a working set loads
        // what it needs afresh for every unit, so only the loads from here on belong in the
        // witness. Before, every key a query loaded between two decisions went into the next
        // input's witness, and the Agent verified its path for nothing.
        if let Some(running) = self.running.as_ref() {
            let _ = running.ledger().book().store().take_keys();
        }
        for (at, input) in inputs.iter().enumerate() {
            match self.replies.get(&input.key, &*self.log) {
                Ok(Some(r)) => {
                    out[at] = Ok(r);
                    continue;
                }
                Ok(None) => {}
                // Answered before, and its reply cannot be read: it must not run again, and
                // nothing after it may run before it.
                Err(_) => {
                    down = Some(at);
                    break;
                }
            }
            // The same key twice in one batch: the second answers as the first, below.
            if runs.iter().any(|(i, _, _)| inputs[*i].key == input.key) {
                continue;
            }
            let (false, Some(running), Some(driver)) = (
                self.stopped.is_some(),
                self.running.as_mut(),
                self.driver.as_mut(),
            ) else {
                // Stopped on an incident: nothing runs until the core is reopened.
                down = Some(at);
                break;
            };
            // A delivery the Agent must record: new, and of a notification the core committed.
            let delivery = match &input.body {
                InputBody::RecordDelivery { notification, at } => hex_id(notification)
                    .filter(|id| !running.is_delivered(id))
                    .filter(|id| {
                        running
                            .ledger()
                            .state()
                            .operations()
                            .notification(id)
                            .is_some()
                    })
                    .map(|id| (id.0, *at)),
                _ => None,
            };
            let outcome = running.execute(input);
            let deliveries = match (&outcome, delivery) {
                (Outcome::Recorded { known: true, .. }, Some(d)) => vec![d],
                _ => Vec::new(),
            };
            match driver.stage(running.ledger_mut(), deliveries) {
                Ok(seq) => runs.push((at, outcome, seq)),
                Err(e) => {
                    failed = Some((at, e));
                    break;
                }
            }
        }
        if let Some(at) = down
            && runs.is_empty()
        {
            for o in out.iter_mut().skip(at) {
                *o = Err(CoreError::Unavailable);
            }
            return (out, Some(at));
        }
        self.stats.native_us.push(micros(native));
        let flush = std::time::Instant::now();
        let flushed = match (failed, self.driver.as_mut()) {
            (Some(f), _) => Err(f),
            (None, Some(driver)) => match driver.flush() {
                Ok(_) => Ok(()),
                Err(e) => Err((down.unwrap_or(inputs.len()), e)),
            },
            (None, None) => Err((
                0,
                DriveError::Port(PortError::Unavailable("no driver".into())),
            )),
        };
        self.stats.flush_us.push(micros(flush));
        let logged = std::time::Instant::now();
        // The inputs that committed: every one, or those up to the Agent's input number after a
        // rebuild. An input that staged nothing commits with the inputs before it.
        let (committed, first) = match flushed {
            Ok(()) => {
                // Archive, then reply: the Agent is told the entries are archived with the next
                // input.
                if let Some(driver) = self.driver.as_mut() {
                    // One sync for the batch's entries (`T-032-13`).
                    if let Err(e) = self.log.push_entries(&driver.take_entries()) {
                        self.stop(format!("the archive does not write: {e}"));
                        return (out, Some(0));
                    }
                    driver.archived();
                }
                if !runs.is_empty() {
                    self.failing = None;
                    // An input committed: the writer is not fenced (`T-037-15`).
                    self.stats.fenced_since.store(0, Ordering::Relaxed);
                }
                (runs.len(), down)
            }
            Err((at, e)) => {
                // Why a decision did not commit, on standard error (`T-032-11`): the burst of
                // 2026-10-04 stalled on decisions that failed and rebuilt the replica each time,
                // and nothing said why.
                let why: String = format!("{e:?}").chars().take(400).collect();
                std::eprintln!(
                    "aledgent-core: the decision of {} inputs did not commit at input {at}: {why}",
                    inputs.len(),
                );
                // The fence (`T-037-15`): the code installed is not the code the writer's state
                // names. Nothing committed and the input's number is not taken; it is retried once
                // the code map's code is installed, or a way out is taken, so it never counts
                // toward the halt. It is audited here, at the execution node, never in the book.
                let fence = match &e {
                    DriveError::Refused(AgentRefusal::Fenced(f)) => Some(f.clone()),
                    _ => None,
                };
                // The Agent ran it and committed nothing, and would again (finding F18).
                let definitive = matches!(
                    e,
                    DriveError::Refused(_) | DriveError::Port(PortError::Aborted(_))
                ) && !matches!(e, DriveError::Refused(AgentRefusal::Fenced(_)));
                let after = match e {
                    DriveError::Diverged(e) | DriveError::Replica(e) => {
                        self.stop(format!("incident: {e}"));
                        None
                    }
                    DriveError::Unsupported(m) => {
                        self.stop(format!("incident: `{m}` reached the Agent path"));
                        None
                    }
                    // Not committed, or not known to be: rebuild, and see. The rebuild archives
                    // what the Agent committed beyond the log.
                    DriveError::Refused(_) | DriveError::Port(_) => self.rebuild().ok(),
                };
                // Only a refusal read after the replica was rebuilt is a definite "not run": a
                // rebuild that failed leaves the outcome unknown (amendment 0036, N2).
                fenced_out = fence.is_some() && after.is_some();
                let n = match after {
                    Some(after) => runs
                        .iter()
                        .position(|(_, _, seq)| seq.is_some_and(|s| s > after))
                        .unwrap_or(runs.len()),
                    None => 0,
                };
                let first = runs.get(n).map_or(at, |(i, _, _)| *i);
                if let Some(f) = &fence {
                    // A prefix of the batch committed: the fence starts again at this input.
                    if n > 0 {
                        self.stats.fenced_since.store(0, Ordering::Relaxed);
                    }
                    self.fenced(inputs.get(first), f);
                }
                // Which input it was (finding F18): the index above is where staging stopped,
                // and a flush inside staging fails the inputs staged before it.
                if let Some(input) = inputs.get(first) {
                    std::eprintln!(
                        "aledgent-core: the first input not committed is {} of {}: {}",
                        first + 1,
                        inputs.len(),
                        describe(input)
                    );
                }
                if definitive && inputs.len() == 1 && n == 0 && first == 0 {
                    self.failed_alone(&inputs[0], &why);
                }
                (n, Some(first))
            }
        };
        // Every committed input's reply, durably in one sync (`T-032-13`), before any is
        // answered or remembered.
        let mut answered: Vec<(usize, CoreReply)> = Vec::with_capacity(committed);
        for (at, outcome, _) in runs.drain(..).take(committed) {
            let input_seq = self.next_seq + answered.len() as u64;
            answered.push((at, CoreReply { input_seq, outcome }));
        }
        let lines: Vec<(InputKey, CoreReply)> = answered
            .iter()
            .map(|(at, r)| (inputs[*at].key, r.clone()))
            .collect();
        if let Err(e) = self.log.push_replies(&lines) {
            self.stop(format!("the replies do not write: {e}"));
            return (out, answered.first().map(|(at, _)| *at));
        }
        for (at, reply) in answered {
            if let Outcome::Decided { result, .. } = &reply.outcome {
                self.fresh.extend(result.credential.iter().cloned());
                self.fresh.extend(result.credentials.iter().cloned());
            }
            self.next_seq += 1;
            self.replies.insert(inputs[at].key, reply.clone());
            out[at] = Ok(reply);
        }
        // A key repeated in the batch answers as its first occurrence, if that committed.
        for (at, input) in inputs.iter().enumerate() {
            if out[at].is_err()
                && first.is_none_or(|f| at < f)
                && let Ok(Some(r)) = self.replies.get(&input.key, &*self.log)
            {
                out[at] = Ok(r);
            }
        }
        if let Some(f) = first {
            // The first input that did not commit and every one after it: the fence's refusal
            // says nothing ran from there (the core takes inputs in order, amendment 0036, N4).
            let rest = if fenced_out {
                CoreError::NotRun
            } else {
                CoreError::Unavailable
            };
            for o in out.iter_mut().skip(f) {
                // An input the record of replies answered stays answered (N3): the core never
                // reports a decided input as not run.
                if fenced_out && o.is_ok() {
                    continue;
                }
                *o = Err(rest);
            }
        }
        self.stats.log_us.push(micros(logged));
        (out, first)
    }
}

/// What the inputs a call did not reach answer (amendment 0036, N4): `NotRun` once the fence has
/// refused one input of the call, `Unavailable` otherwise.
fn not_reached(answered: &[Result<CoreReply, CoreError>]) -> CoreError {
    if answered.contains(&Err(CoreError::NotRun)) {
        CoreError::NotRun
    } else {
        CoreError::Unavailable
    }
}

/// An input's time input, where it has one (`T-037-15`: the fenced clock starts at it).
fn input_time(input: &CoreInput) -> Option<Time> {
    match &input.body {
        InputBody::Command { now, .. } | InputBody::Tick { now } => Some(*now),
        InputBody::RecordDelivery { at, .. } => Some(*at),
        InputBody::SpecChunk { .. } => None,
    }
}

/// An input as an operator reads it in a log line: its key, its kind and its time. Never its
/// body, which may carry credential material (`REQ-AU-8`).
fn describe(input: &CoreInput) -> String {
    let key: String = input.key.0.iter().map(|b| format!("{b:02x}")).collect();
    let what = match &input.body {
        InputBody::Command { command, now, .. } => {
            let name: String = format!("{command:?}")
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            format!("command {name} at {now}")
        }
        InputBody::Tick { now } => format!("tick at {now}"),
        InputBody::RecordDelivery { at, .. } => format!("delivery recorded at {at}"),
        InputBody::SpecChunk { path, .. } => format!("seed chunk ({} nodes of path)", path.len()),
    };
    format!("{what}, key {key}")
}

fn hex_id(s: &str) -> Option<NotificationId> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return None;
    }
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(NotificationId(out))
}

/// The Agent core's side of shipping (`T-013-13`): what the Agent committed, read from the
/// replica through the gate (a read never sees an input the Agent has not committed); an
/// acknowledgement, under the core's lock. Gone once the core is dropped.
struct AgentOutbox<P: AgentPort>(Weak<Parts<P>>);

impl<P: AgentPort + Send + 'static> AuditOutbox for AgentOutbox<P> {
    fn pending(&self, limit: usize) -> Option<Vec<ledger_core::audit::AuditRecord>> {
        let parts = self.0.upgrade()?;
        let r = parts.running.read();
        Some(
            r.as_ref()
                .map(|r| r.audit_pending(limit))
                .unwrap_or_default(),
        )
    }

    fn acknowledge(
        &self,
        head: Head,
    ) -> Option<Result<(), ledger_core::ledger::auditing::AuditGap>> {
        let parts = self.0.upgrade()?;
        let inner = lock(&parts.ctl);
        let mut slot = parts.running.write();
        let Some(exec) = slot.as_mut() else {
            return Some(Ok(()));
        };
        let done = exec.ledger_mut().acknowledge_audit(head).map(|_| ());
        if let Some(a) = &inner.audit {
            let (n, oldest) = exec.audit_outbox();
            a.stats.observe(n, oldest);
        }
        Some(done)
    }
}

impl<P: AgentPort + Send + 'static> Core for AgentCore<P> {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        let core = self.clone();
        let input = input.clone();
        self.on_decider(move || core.apply_now(&input))
            .await
            .unwrap_or(Err(CoreError::Ambiguous))
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        let core = self.clone();
        let batch = inputs.to_vec();
        let n = inputs.len();
        self.on_decider(move || core.apply_batch_now(&batch))
            .await
            .unwrap_or_else(|| vec![Err(CoreError::Ambiguous); n])
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        _now: Time,
    ) -> Result<OutboxPage, CoreError> {
        let cursor = cursor.map(String::from);
        self.read(move |r| r.outbox(cursor.as_deref(), undelivered))
            .await
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        self.read(|r| r.last_tick()).await
    }

    async fn next_due(&self) -> Result<Option<Time>, CoreError> {
        self.read(|r| r.ledger().next_due()).await
    }

    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        let (caller, query) = (caller.clone(), query.clone());
        let mut reply = self.read(move |r| r.query(&caller, &query, now)).await?;
        if let QueryReply::Signals(s) = &mut reply {
            // The host's own signals (`F-018`): the halt, the audit trail's shipping, the
            // runtime's Authority backlog.
            let stats = &self.inner.stats;
            s.core_halted = stats.halted.load(Ordering::Relaxed) == 1
                || stats.stopped.load(Ordering::Relaxed) == 1;
            s.audit = self.inner.audit.as_ref().map(|a| a.stats.body(now));
            let u = stats.unacknowledged.load(Ordering::Relaxed);
            s.vos_unacknowledged = (u != u64::MAX).then_some(u);
            // Which program the writer runs, as of the last input committed (`T-037-23`).
            s.writer_code = (*lock(&stats.writer_code)).map(|w| writer_code_body(&w));
            // How long the writer has been fenced (`T-037-15`, `NFR-11`).
            let since = stats.fenced_since.load(Ordering::Relaxed);
            s.writer_fenced_age = (since != 0).then(|| now.saturating_sub(since));
            // What the code fetcher discarded as not its digest's bytes (`T-037-06`).
            if let Some(source) = self.inner.code_source.get() {
                s.code_source_mismatches = source
                    .mismatches()
                    .into_iter()
                    .map(|(source, count)| ingress::query::CodeSourceMismatchBody { source, count })
                    .collect();
            }
        }
        Ok(reply)
    }

    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        // From the replica, like every read (`DEC-4`): the Agent's book at its roots, as last
        // committed (`Credentials`).
        if let Some(found) = lock(&self.inner.committed).0.get(credential) {
            self.inner.stats.reads.fetch_add(1, Ordering::Relaxed);
            self.inner
                .stats
                .committed_reads
                .fetch_add(1, Ordering::Relaxed);
            return Ok(found.clone());
        }
        let credential = String::from(credential);
        let core = self.clone();
        self.read(move |r| {
            let found = r.credential(&credential);
            // Still under the shared gate: no decision has committed since the read.
            lock(&core.inner.committed).insert(credential, found.clone());
            found
        })
        .await
    }

    async fn genesis_open(&self) -> Result<bool, CoreError> {
        // T-036-25: open unless the host gated a chainspec book that has no report yet.
        Ok(self
            .inner
            .genesis_gate
            .get()
            .is_none_or(crate::verified::GenesisGate::is_open))
    }

    async fn stage_code(&self, code: &[u8; 32]) -> Result<ingress::boundary::Staged, CoreError> {
        use ingress::boundary::Staged;
        let Some(source) = self.inner.code_source.get().cloned() else {
            return Ok(Staged::Refused {
                reason: crate::staging::StageRefusal::NoSource.to_string(),
            });
        };
        let (core, code) = (self.clone(), *code);
        // Retrieval is slow: it runs off the async runtime and outside the core's locks. Only
        // the writer's generation is read before it and the staging done after it, each briefly
        // under the lock, so decisions are not held up by a fetch.
        tokio::task::spawn_blocking(move || {
            let generation = {
                let mut inner = lock(&core.inner.ctl);
                if inner.stopped.is_some() {
                    return Err(CoreError::Unavailable);
                }
                inner
                    .driver
                    .as_mut()
                    .and_then(|d| d.port_mut().hot().ok().flatten())
                    .map(|h| h.generation)
                    .ok_or(CoreError::Unavailable)?
            };
            let checked = match crate::staging::fetch(&*source, &code, generation) {
                Ok(c) => c,
                Err(r) => {
                    return Ok(Staged::Refused {
                        reason: r.to_string(),
                    });
                }
            };
            let mut inner = lock(&core.inner.ctl);
            let Some(driver) = inner.driver.as_mut() else {
                return Err(CoreError::Unavailable);
            };
            // Could the switch install it? Asked before the version is staged, under the same lock
            // as the staging, so a version it would refuse at its input is refused at the door
            // (`T-037-22`, owner ruling w16): nothing is staged and, the door refusing it, nothing
            // is logged.
            if let Err(why) = driver.port_mut().can_install(&checked) {
                let refusal = crate::staging::StageRefusal::CannotInstall(why).to_string();
                std::eprintln!(
                    "aledgent-core: refused an upgrade to code {} before it was logged (the last \
                     committed input is {}): {refusal}",
                    crate::switch::short(&code),
                    driver.seq()
                );
                return Ok(Staged::Refused { reason: refusal });
            }
            match driver.port_mut().stage_code(checked) {
                Ok(()) => Ok(Staged::Staged),
                Err(r) => Ok(Staged::Refused {
                    reason: r.to_string(),
                }),
            }
        })
        .await
        .unwrap_or(Err(CoreError::Unavailable))
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        // From the replica too (`DEC-4`). A tie is never undone, so a replica behind the Agent
        // can only answer "none" for a tie it has not seen yet.
        let principal = String::from(principal);
        self.read(move |r| r.party_of(&principal)).await
    }
}

/// A code version's digest in lower-case hex (`T-037-23`).
fn hex32(code: &[u8; 32]) -> String {
    code.iter().map(|b| format!("{b:02x}")).collect()
}

/// The writer's code as the signals carry it (`T-037-23`).
fn writer_code_body(w: &crate::fence::WriterCode) -> ingress::query::WriterCodeBody {
    ingress::query::WriterCodeBody {
        code: hex32(&w.code),
        generation: w.generation,
        upgrade: w
            .upgrade
            .map(|(code, from)| ingress::query::ScheduledUpgradeBody {
                code: hex32(&code),
                from,
            }),
    }
}

/// The line the operator reads for an event of the writer's code (`T-037-23`): plain, naming the
/// input, with a version as its first eight bytes in hex, the way the fence's refusal does not
/// (it names all of it) and the signals' label does.
fn describe_code_event(event: &crate::fence::CodeEvent) -> String {
    use crate::fence::CodeEvent;
    use crate::switch::short;
    match *event {
        CodeEvent::Installed {
            input,
            code,
            from: Some(from),
        } => format!(
            "the platform now runs code {} (it ran {}) for input {input}",
            short(&code),
            short(&from)
        ),
        CodeEvent::Installed {
            input,
            code,
            from: None,
        } => format!("the platform runs code {} for input {input}", short(&code)),
        CodeEvent::Switched {
            input,
            from,
            to,
            generation,
        } => format!(
            "input {input} is the first to run code {} (generation {generation}), after code {}: \
             the scheduled upgrade took effect",
            short(&to),
            short(&from)
        ),
        CodeEvent::WayOut {
            input,
            taken,
            upgrade,
        } => {
            let scheduled = upgrade.map_or_else(
                || "no upgrade was scheduled".to_string(),
                |(code, from)| {
                    format!(
                        "the upgrade to code {} was scheduled from input {from}",
                        short(&code)
                    )
                },
            );
            let done = match taken {
                ledger_core::code::WayOut::Cancel => "it is cancelled".to_string(),
                ledger_core::code::WayOut::Reschedule { from } => {
                    format!("it now applies from input {from}")
                }
            };
            format!("input {input} took the way out: {scheduled}, {done}")
        }
        CodeEvent::Scheduled { input, code, from } => format!(
            "input {input} scheduled an upgrade to code {}, to apply from input {from}",
            short(&code)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(n: u64) -> CoreReply {
        // Through serde, so that a field added to the reply with a default changes nothing here.
        serde_json::from_value(serde_json::json!({
            "input_seq": n,
            "outcome": { "outcome": "ticked", "ignored": false, "moved": n, "queued": 0, "incidents": 0 },
        }))
        .unwrap_or_else(|e| panic!("a reply: {e}"))
    }

    fn key(n: u64) -> InputKey {
        let mut k = [0u8; 16];
        k[..8].copy_from_slice(&n.to_be_bytes());
        InputKey(k)
    }

    /// `INV-18`, `T-032-15`: the file log finds an old reply through its index, a reply appended
    /// after the index was built included, without reading the whole file each time.
    #[test]
    fn inv_18_an_old_reply_is_read_from_the_file_log_by_its_index() {
        let dir = std::env::temp_dir().join(format!(
            "aledgent-filelog-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut log = FileLog::open(&dir).unwrap();
        let mut replies = Replies::default();
        let n = RECENT_REPLIES as u64 + 2_000;
        let batch: Vec<(InputKey, CoreReply)> = (1..=n).map(|i| (key(i), reply(i))).collect();
        log.push_replies(&batch).unwrap();
        for (k, r) in batch {
            replies.insert(k, r);
        }
        assert_eq!(replies.get(&key(1), &log), Ok(Some(reply(1))));
        // Appended after the index was built: caught up on the next lookup.
        log.push_reply(&key(n + 1), &reply(n + 1)).unwrap();
        replies.insert(key(n + 1), reply(n + 1));
        assert_eq!(log.reply(&key(n + 1)), Ok(Some(reply(n + 1))));
        let started = std::time::Instant::now();
        for i in 1..=100 {
            assert_eq!(replies.get(&key(i), &log), Ok(Some(reply(i))));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "100 old replies in {:?}",
            started.elapsed()
        );
        assert_eq!(log.reply(&key(n + 2)), Ok(None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `INV-18`, `T-031-14`: only the most recent replies stay in memory; an older one is read
    /// from the log, so a key retried long after it was answered gets its first reply and never
    /// runs again; a key never answered is unknown; and an answered key whose reply the log cannot
    /// give is an error, never "unknown".
    #[test]
    fn inv_18_an_old_reply_is_read_from_the_log_and_never_run_again() {
        let mut log = MemLog::default();
        let mut replies = Replies::default();
        let n = RECENT_REPLIES as u64 + 100;
        for i in 1..=n {
            log.push_reply(&key(i), &reply(i)).unwrap();
            replies.insert(key(i), reply(i));
        }
        assert_eq!(replies.recent.len(), RECENT_REPLIES);
        assert_eq!(replies.known.len() as u64, n);
        assert!(
            !replies.recent.contains_key(&key(1)),
            "the oldest left memory"
        );
        assert_eq!(replies.get(&key(1), &log), Ok(Some(reply(1))));
        assert_eq!(replies.get(&key(n), &log), Ok(Some(reply(n))));
        assert_eq!(replies.get(&key(n + 1), &log), Ok(None));
        assert!(replies.get(&key(1), &MemLog::default()).is_err());
    }
}
