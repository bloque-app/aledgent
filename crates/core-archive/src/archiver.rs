// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Continuous backup of the core host (`T-005-15`, `NFR-4`, `NFR-10`, `NFR-21` (c)).
//!
//! **What streams.** Everything the core host must not lose is in its state directory
//! (`agents/core-host/src/serve.rs`): `archive.bin` (every input the Agent committed, as its
//! delta-ring entry, hash-chained), `replies.jsonl` (every input's reply, by key), and
//! `agent.state` (the Agent's runtime state). [`ArchivingLog`] wraps the host's log and marks a
//! **cut** at every decision boundary, after the decision's entries and replies are durable on
//! the node and before it is answered: the two files' lengths there. The [`Archiver`], a thread of
//! its own, ships the bytes between the last cut it archived and the latest as one segment
//! ([`crate::record`]), at most [`ArchiverConfig::interval`] after the decision, and every
//! [`ArchiverConfig::checkpoint_every_entries`] inputs (or [`ArchiverConfig::checkpoint_every`])
//! a checkpoint of `agent.state` read at a cut. The replica's leaf store is not shipped: it is a
//! projection of the archive, which a restore rebuilds and checks root by root (`DEC-4`).
//!
//! **Never on the critical path.** A cut is two `stat` calls and a lock; nothing the core
//! commits, acknowledges or answers waits for the sink. A sink that is down or slow only grows
//! the lag, which is a metric and pages ([`ArchiveStatus`]).
//!
//! **The recovery point** (the operator states it). If the node and its storage are
//! lost, the archive holds every decision up to the last segment it published: the loss window
//! is the archive's lag at the failure, about `interval` plus one upload while the sink is healthy,
//! and at most the lag the alert pages at (`AledgentCoreArchiveLag`). With ingress's
//! received-log intact, the window is replayed from it (`opsctl restore core-tail`), so no
//! acknowledged or answered operation is lost.
//!
//! **One history.** On start the archiver reads the last segment back and checks the local logs
//! hold exactly its bytes where it says. A node whose logs are another history (two hosts of one
//! core, or a node restored from an older snapshot) never writes: it reports a conflict, and the
//! write-once sink refuses it anyway.

use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use core_binding::core::{ARCHIVE_FILE, HostLog, REPLIES_FILE, complete_entries_len};
use core_binding::envelope::decode;
use core_binding::hot::RingEntry;
use ingress::boundary::{CoreReply, InputKey};
use serde::{Deserialize, Serialize};

use crate::archive::{Archive, ArchiveSink as _, ArchiveSource as _, contiguous_end};
use crate::record::{
    CheckpointHeader, FORMAT, Point, SegmentHeader, Span, encode_checkpoint, encode_segment,
    sha256_hex,
};
use crate::store::StoreError;

/// The archiver's status file in the state directory, read by `opsctl exporter`.
pub const STATUS_FILE: &str = "archive.status.json";

/// How the archiver paces itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiverConfig {
    /// The longest a committed decision waits before its segment is sent: the recovery point
    /// while the sink is healthy.
    pub interval: Duration,
    /// The most payload one segment carries; a larger backlog is split ([`crate::record`]).
    pub max_segment_bytes: u64,
    /// A checkpoint every this many Agent inputs.
    pub checkpoint_every_entries: u64,
    /// And at least this often while inputs commit.
    pub checkpoint_every: Duration,
    /// The longest wait between two attempts after the sink failed.
    pub max_backoff: Duration,
}

impl Default for ArchiverConfig {
    fn default() -> Self {
        ArchiverConfig {
            interval: Duration::from_secs(1),
            max_segment_bytes: 16 * 1024 * 1024,
            checkpoint_every_entries: 50_000,
            checkpoint_every: Duration::from_secs(15 * 60),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Reads the Agent's runtime state at a cut, for a checkpoint: `agent.state` on the VOS host.
pub type StateReader = Box<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>;

/// A decision boundary: the logs' lengths after it.
#[derive(Clone, Debug)]
struct Cut {
    archive: u64,
    replies: u64,
    /// The Agent's runtime state there, when a checkpoint was asked for.
    state: Option<Vec<u8>>,
}

#[derive(Default)]
struct Pending {
    cuts: Vec<Cut>,
    /// When the oldest decision not yet archived was cut, in ms.
    since: Option<u64>,
}

/// What the log and the archiver share.
pub struct Tap {
    pending: Mutex<Pending>,
    wake: Condvar,
    want_checkpoint: AtomicBool,
    state: Option<StateReader>,
    last_input: AtomicU64,
    stop: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Tap {
    fn push(&self, cut: Cut) {
        let mut p = lock(&self.pending);
        // A later cut supersedes an earlier one, unless that one carries a checkpoint, which
        // stands only at its own cut.
        match p.cuts.last_mut() {
            Some(last) if last.state.is_none() => *last = cut,
            _ => p.cuts.push(cut),
        }
        p.since.get_or_insert_with(now_ms);
        self.wake.notify_one();
    }
}

/// The host's log, with a cut at every decision boundary (module documentation).
pub struct ArchivingLog<L: HostLog> {
    inner: L,
    dir: PathBuf,
    tap: Arc<Tap>,
}

impl<L: HostLog> ArchivingLog<L> {
    /// Wrap `inner`, whose files are in `dir`; `state` reads the Agent's runtime state for a
    /// checkpoint (none: the archive has no checkpoints, and a restore founds the Agent afresh).
    /// Returns the log, and the tap to start the [`Archiver`] with once the core is open.
    pub fn new(inner: L, dir: impl Into<PathBuf>, state: Option<StateReader>) -> (Self, Arc<Tap>) {
        let tap = Arc::new(Tap {
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            want_checkpoint: AtomicBool::new(false),
            state,
            last_input: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        (
            ArchivingLog {
                inner,
                dir: dir.into(),
                tap: Arc::clone(&tap),
            },
            tap,
        )
    }

    fn cut(&self) {
        let len = |name: &str| fs::metadata(self.dir.join(name)).map(|m| m.len());
        let (Ok(archive), Ok(replies)) = (len(ARCHIVE_FILE), len(REPLIES_FILE)) else {
            // Nothing written yet (or the files cannot be read): the next cut covers it.
            return;
        };
        let mut state = None;
        if self.tap.want_checkpoint.swap(false, Ordering::AcqRel)
            && let Some(read) = &self.tap.state
        {
            match read() {
                Ok(s) => state = Some(s),
                Err(e) => {
                    std::eprintln!("aledgent-archive: the checkpoint was not read: {e}");
                    self.tap.want_checkpoint.store(true, Ordering::Release);
                }
            }
        }
        self.tap.push(Cut {
            archive,
            replies,
            state,
        });
    }
}

impl<L: HostLog> HostLog for ArchivingLog<L> {
    fn entries(&self) -> Result<Vec<RingEntry>, String> {
        self.inner.entries()
    }
    fn push_entry(&mut self, e: &RingEntry) -> Result<(), String> {
        self.inner.push_entry(e)
    }
    fn replies(&self) -> Result<Vec<(InputKey, CoreReply)>, String> {
        self.inner.replies()
    }
    fn reply(&self, key: &InputKey) -> Result<Option<CoreReply>, String> {
        self.inner.reply(key)
    }
    fn push_reply(&mut self, key: &InputKey, reply: &CoreReply) -> Result<(), String> {
        self.push_replies(&[(*key, reply.clone())])
    }
    fn push_entries(&mut self, entries: &[RingEntry]) -> Result<(), String> {
        self.inner.push_entries(entries)
    }
    /// The end of a decision: its entries were pushed before, its replies now. The cut follows.
    fn push_replies(&mut self, replies: &[(InputKey, CoreReply)]) -> Result<(), String> {
        self.inner.push_replies(replies)?;
        if let Some((_, r)) = replies.last() {
            self.tap.last_input.store(r.input_seq, Ordering::Release);
        }
        self.cut();
        Ok(())
    }
}

/// Where the latest checkpoint stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointAt {
    /// Its segment.
    pub n: u64,
    /// The Agent's input number there.
    pub entries: u64,
    /// When it was taken, in ms.
    pub at: u64,
}

/// The archiver's status, written to [`STATUS_FILE`] about once a second. Counts and times only:
/// no ledger fact (`REQ-PV-5`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveStatus {
    /// When it was written, in ms.
    pub at: u64,
    /// The sink (no credential).
    pub sink: String,
    /// Segments published.
    pub segments: u64,
    /// The last restore point published.
    pub point: Option<Point>,
    /// The core's last committed input number.
    pub committed_input: u64,
    /// Committed inputs not yet in the sink.
    pub lag_inputs: u64,
    /// How long the oldest decision not yet in the sink has waited, in ms; 0 when none waits.
    pub lag_ms: u64,
    /// Bytes of the logs not yet in the sink.
    pub pending_bytes: u64,
    /// Attempts that failed in a row.
    pub failures: u64,
    /// The last failure.
    pub last_error: Option<String>,
    /// Why the archiver refuses to write: the local logs are not the archive's history.
    pub conflict: Option<String>,
    /// The latest checkpoint.
    pub checkpoint: Option<CheckpointAt>,
}

impl ArchiveStatus {
    /// Read a status file.
    pub fn read(path: &Path) -> Result<ArchiveStatus, String> {
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// A running archiver.
pub struct Archiver {
    tap: Arc<Tap>,
    thread: Option<JoinHandle<()>>,
}

impl Archiver {
    /// Start archiving the logs in `dir` (the core is open: its files stand at a boundary) to
    /// `archive`, for the book `book` (its genesis operation reference, hex).
    pub fn start(
        tap: Arc<Tap>,
        dir: impl Into<PathBuf>,
        archive: Archive,
        book: String,
        config: ArchiverConfig,
    ) -> Archiver {
        let dir = dir.into();
        let mut run = Run {
            dir,
            sink: archive.describe(),
            archive,
            book,
            config,
            tap: Arc::clone(&tap),
            next: 1,
            prev: String::new(),
            off_archive: 0,
            off_replies: 0,
            point: None,
            checkpoint: None,
            failures: 0,
            last_error: None,
            conflict: None,
            last_status: 0,
            latest: (0, 0),
            inflight: None,
        };
        let thread = std::thread::Builder::new()
            .name("core-archiver".into())
            .spawn(move || run.main())
            .unwrap_or_else(|e| panic!("the archiver thread does not start: {e}"));
        Archiver {
            tap,
            thread: Some(thread),
        }
    }

    /// Archive everything cut so far, then stop (tests, an orderly shutdown). Waits for the
    /// thread.
    pub fn stop(mut self) {
        self.tap.stop.store(true, Ordering::Release);
        self.tap.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Run {
    dir: PathBuf,
    archive: Archive,
    sink: String,
    book: String,
    config: ArchiverConfig,
    tap: Arc<Tap>,
    next: u64,
    prev: String,
    off_archive: u64,
    off_replies: u64,
    point: Option<Point>,
    checkpoint: Option<CheckpointAt>,
    failures: u64,
    last_error: Option<String>,
    conflict: Option<String>,
    last_status: u64,
    /// The latest cut's lengths, for the pending bytes.
    latest: (u64, u64),
    /// When the oldest cut being uploaded now was cut.
    inflight: Option<u64>,
}

enum Failed {
    /// Retry later.
    Retry(String),
    /// Never write again.
    Conflict(String),
}

impl From<StoreError> for Failed {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Conflict(_)
            | StoreError::Corrupt(_)
            | StoreError::InvalidName(_)
            | StoreError::Sealed(_) => Failed::Conflict(e.to_string()),
            StoreError::TooLarge(_) | StoreError::Unavailable(_) => Failed::Retry(e.to_string()),
        }
    }
}

fn read_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>, String> {
    let mut f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.seek(SeekFrom::Start(start))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let n = usize::try_from(end - start).map_err(|_| "a range too large".to_string())?;
    let mut out = vec![0u8; n];
    f.read_exact(&mut out)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(out)
}

/// Parts of a backlog: (archive bytes, replies bytes), entries first (module documentation of
/// [`crate::record`]). `a` holds whole records, `r` whole lines.
fn parts<'a>(a: &'a [u8], r: &'a [u8], max: u64) -> Vec<(&'a [u8], &'a [u8])> {
    let max = usize::try_from(max).unwrap_or(usize::MAX).max(1);
    if a.len() + r.len() <= max {
        return vec![(a, r)];
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < a.len() {
        let mut end = at;
        // Whole records, at least one.
        while let Some(head) = a.get(end..end + 4) {
            let n = 4 + u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
            if end > at && end + n - at > max {
                break;
            }
            end = (end + n).min(a.len());
        }
        out.push((&a[at..end], &r[..0]));
        at = end;
    }
    let mut at = 0;
    while at < r.len() {
        if r.len() - at <= max {
            out.push((&a[a.len()..], &r[at..]));
            break;
        }
        let end = match r[at..at + max].iter().rposition(|b| *b == b'\n') {
            Some(i) => at + i + 1,
            // One line longer than the bound: alone.
            None => r[at..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(r.len(), |i| at + i + 1),
        };
        out.push((&a[a.len()..], &r[at..end]));
        at = end;
    }
    out
}

#[derive(Deserialize)]
struct ReplyLine {
    #[allow(dead_code)]
    key: InputKey,
    reply: CoreReply,
}

/// The last entry of `a` (whole records), if any.
fn last_entry(a: &[u8]) -> Result<Option<RingEntry>, String> {
    let mut at = 0;
    let mut last = None;
    while let Some(head) = a.get(at..at + 4) {
        let n = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let body = a
            .get(at + 4..at + 4 + n)
            .ok_or("a torn record in the archive's bytes")?;
        last = Some(body);
        at += 4 + n;
    }
    last.map(decode::<RingEntry>).transpose()
}

/// The input number of the last line of `r` (whole lines), if any.
fn last_input(r: &[u8]) -> Result<Option<u64>, String> {
    let Some(body) = r.strip_suffix(b"\n") else {
        return Ok(None);
    };
    let line = body
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(body, |i| &body[i + 1..]);
    serde_json::from_slice::<ReplyLine>(line)
        .map(|l| Some(l.reply.input_seq))
        .map_err(|e| format!("a reply line: {e}"))
}

impl Run {
    fn main(&mut self) {
        if let Err(e) = self.resume() {
            self.conflict = Some(e);
        }
        loop {
            let stopping = self.tap.stop.load(Ordering::Acquire);
            let (cuts, since) = self.take_due(stopping);
            if let Some(c) = cuts.last() {
                self.latest = (c.archive, c.replies);
            }
            if self.conflict.is_none() && !cuts.is_empty() {
                self.inflight = since;
                self.upload_all(cuts, since);
                self.inflight = None;
            } else if self.conflict.is_some() {
                // Never write; keep the lag growing so it shows, and do not spin.
                self.put_back(cuts, since);
                let p = lock(&self.tap.pending);
                drop(
                    self.tap
                        .wake
                        .wait_timeout(p, Duration::from_secs(1))
                        .unwrap_or_else(|e| e.into_inner()),
                );
            }
            self.ask_checkpoint();
            self.status(stopping);
            let drained = lock(&self.tap.pending).cuts.is_empty();
            if stopping && (drained || self.conflict.is_some() || self.failures > 3) {
                self.status(true);
                return;
            }
        }
    }

    /// Where the archive stands, and whether the local logs are its history.
    fn resume(&mut self) -> Result<(), String> {
        let numbers = loop {
            match self.archive.segments() {
                Ok(n) => break n,
                Err(e) => {
                    self.last_error = Some(e.to_string());
                    self.failures += 1;
                    self.status(true);
                    if self.tap.stop.load(Ordering::Acquire) {
                        return Err(format!("the sink did not answer: {e}"));
                    }
                    std::thread::sleep(self.backoff());
                }
            }
        };
        self.failures = 0;
        let end = contiguous_end(&numbers);
        if numbers.last().copied().unwrap_or(0) != end {
            return Err(format!(
                "the archive has segments after a gap at {}: not this host's history",
                end + 1
            ));
        }
        if end > 0 {
            // F-044: the header and digest only, which a sealed archive keeps in clear, so the
            // core host resumes with no key that opens the archive. The local logs are checked
            // against the header's SHA-256 of each payload below, as before.
            let (header, digest) = self
                .archive
                .segment_head(end)
                .map_err(|e| e.to_string())?
                .ok_or("the last segment vanished")?;
            if header.book != self.book {
                return Err(format!(
                    "the archive is of book {}, this host's is {}",
                    header.book, self.book
                ));
            }
            for (name, span) in [
                (ARCHIVE_FILE, &header.archive),
                (REPLIES_FILE, &header.replies),
            ] {
                let local =
                    read_range(&self.dir.join(name), span.start, span.end).map_err(|_| {
                        format!("{name} is shorter than the archive: not this host's history")
                    })?;
                if sha256_hex(&local) != span.sha256 {
                    return Err(format!(
                        "{name} differs from the archive at its last segment: not this host's history"
                    ));
                }
            }
            self.next = end + 1;
            self.prev = digest;
            self.off_archive = header.archive.end;
            self.off_replies = header.replies.end;
            self.point = header.point;
            if let Ok(cps) = self.archive.checkpoints()
                && let Some(&n) = cps.iter().rev().find(|n| **n <= end)
                && let Ok(Some(c)) = self.archive.checkpoint_head(n)
            {
                self.checkpoint = Some(CheckpointAt {
                    n,
                    entries: c.point.entries,
                    at: c.taken_at,
                });
            }
        }
        // The logs as the core left them on opening: a boundary, as after a restart.
        let len = |name: &str| fs::metadata(self.dir.join(name)).map_or(0, |m| m.len());
        let (archive, replies) = (len(ARCHIVE_FILE), len(REPLIES_FILE));
        if archive < self.off_archive || replies < self.off_replies {
            return Err(
                "the local logs are shorter than the archive: not this host's history".into(),
            );
        }
        if archive > self.off_archive || replies > self.off_replies {
            let mut p = lock(&self.tap.pending);
            p.cuts.insert(
                0,
                Cut {
                    archive,
                    replies,
                    state: None,
                },
            );
            p.since.get_or_insert_with(now_ms);
        }
        Ok(())
    }

    fn backoff(&self) -> Duration {
        let s = 1u64 << self.failures.min(6);
        Duration::from_secs(s).min(self.config.max_backoff)
    }

    /// The cuts due now: after `interval`, or one carrying a checkpoint, or a large backlog; and
    /// when the oldest of them was cut.
    fn take_due(&mut self, stopping: bool) -> (Vec<Cut>, Option<u64>) {
        let mut p = lock(&self.tap.pending);
        let interval = u64::try_from(self.config.interval.as_millis()).unwrap_or(u64::MAX);
        loop {
            let due = !p.cuts.is_empty()
                && (stopping
                    || p.cuts.iter().any(|c| c.state.is_some())
                    || p.since
                        .is_some_and(|s| now_ms().saturating_sub(s) >= interval)
                    || p.cuts.last().is_some_and(|c| {
                        (c.archive - self.off_archive.min(c.archive))
                            + (c.replies - self.off_replies.min(c.replies))
                            >= self.config.max_segment_bytes
                    }));
            if due || stopping || self.tap.stop.load(Ordering::Acquire) {
                break;
            }
            let wait = Duration::from_millis(interval.clamp(10, 250));
            p = self
                .tap
                .wake
                .wait_timeout(p, wait)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
            if p.cuts.is_empty() {
                // Wake for the status and the checkpoint request at least this often.
                return (Vec::new(), None);
            }
        }
        if p.cuts.is_empty() {
            return (Vec::new(), None);
        }
        (core::mem::take(&mut p.cuts), p.since.take())
    }

    /// Cuts not archived go back in front of any cut since, with their age.
    fn put_back(&mut self, mut cuts: Vec<Cut>, since: Option<u64>) {
        if cuts.is_empty() {
            return;
        }
        let mut p = lock(&self.tap.pending);
        cuts.append(&mut p.cuts);
        // Keep every cut with a checkpoint, and the latest.
        let last = cuts.len() - 1;
        let mut kept: Vec<Cut> = cuts
            .into_iter()
            .enumerate()
            .filter(|(i, c)| *i == last || c.state.is_some())
            .map(|(_, c)| c)
            .collect();
        p.cuts.append(&mut kept);
        p.since = match (p.since, since) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b).or_else(|| Some(now_ms())),
        };
    }

    fn upload_all(&mut self, cuts: Vec<Cut>, since: Option<u64>) {
        let mut rest = cuts.into_iter();
        while let Some(cut) = rest.next() {
            match self.upload(&cut) {
                Ok(()) => {
                    self.failures = 0;
                    self.last_error = None;
                }
                Err(Failed::Conflict(e)) => {
                    std::eprintln!("aledgent-archive: ALERT: the archiver stops writing: {e}");
                    self.conflict = Some(e);
                    let mut back = vec![cut];
                    back.extend(rest);
                    self.put_back(back, since);
                    return;
                }
                Err(Failed::Retry(e)) => {
                    self.failures += 1;
                    self.last_error = Some(e);
                    let mut back = vec![cut];
                    back.extend(rest);
                    self.put_back(back, since);
                    self.status(true);
                    if !self.tap.stop.load(Ordering::Acquire) || self.failures <= 3 {
                        std::thread::sleep(self.backoff());
                    }
                    return;
                }
            }
        }
    }

    /// Ship one cut: its bytes as one segment or a few parts, then its checkpoint.
    fn upload(&mut self, cut: &Cut) -> Result<(), Failed> {
        if cut.archive < self.off_archive || cut.replies < self.off_replies {
            return Err(Failed::Conflict(
                "the local logs shrank below what is archived".into(),
            ));
        }
        let a = read_range(&self.dir.join(ARCHIVE_FILE), self.off_archive, cut.archive)
            .map_err(Failed::Retry)?;
        let r = read_range(&self.dir.join(REPLIES_FILE), self.off_replies, cut.replies)
            .map_err(Failed::Retry)?;
        if complete_entries_len(&a) != a.len() || r.last().is_some_and(|b| *b != b'\n') {
            return Err(Failed::Retry("a cut inside a record".into()));
        }
        let start = (self.off_archive, self.off_replies);
        let pieces = parts(&a, &r, self.config.max_segment_bytes);
        let count = pieces.len();
        for (i, (pa, pr)) in pieces.into_iter().enumerate() {
            if pa.is_empty() && pr.is_empty() {
                continue;
            }
            let is_cut = i + 1 == count;
            let point = if is_cut {
                Some(self.point_after(start, &a, &r)?)
            } else {
                self.point.clone()
            };
            let header = SegmentHeader {
                format: FORMAT,
                book: self.book.clone(),
                n: self.next,
                prev: self.prev.clone(),
                archive: Span {
                    start: self.off_archive,
                    end: self.off_archive + pa.len() as u64,
                    sha256: sha256_hex(pa),
                },
                replies: Span {
                    start: self.off_replies,
                    end: self.off_replies + pr.len() as u64,
                    sha256: sha256_hex(pr),
                },
                cut: is_cut,
                point: point.clone(),
                taken_at: now_ms(),
            };
            let record = encode_segment(&header, pa, pr);
            self.archive.put_segment(self.next, &record)?;
            self.prev = sha256_hex(&record);
            self.next += 1;
            self.off_archive = header.archive.end;
            self.off_replies = header.replies.end;
            if is_cut {
                self.point = point;
            }
        }
        if let (Some(state), Some(point)) = (&cut.state, &self.point)
            && point.archive_bytes == cut.archive
            && point.replies_bytes == cut.replies
            && self.next > 1
        {
            let n = self.next - 1;
            let header = CheckpointHeader {
                format: FORMAT,
                book: self.book.clone(),
                n,
                segment: self.prev.clone(),
                point: point.clone(),
                bytes: state.len() as u64,
                sha256: sha256_hex(state),
                taken_at: now_ms(),
            };
            self.archive
                .put_checkpoint(n, &encode_checkpoint(&header, state))?;
            self.checkpoint = Some(CheckpointAt {
                n,
                entries: point.entries,
                at: header.taken_at,
            });
        }
        Ok(())
    }

    /// The restore point at the end of a cut whose bytes `a` and `r` start at `start`.
    fn point_after(&self, start: (u64, u64), a: &[u8], r: &[u8]) -> Result<Point, Failed> {
        let before = self.point.as_ref();
        let entry = last_entry(a).map_err(Failed::Retry)?;
        let inputs = match last_input(r).map_err(Failed::Retry)? {
            Some(n) => n,
            None => before.map_or(0, |p| p.inputs),
        };
        let (entries, log_head, kernel_root, service_root, audit) = match (&entry, before) {
            (Some(e), _) => (
                e.seq,
                hex::encode(e.log_hash()),
                hex::encode(e.roots.kernel),
                hex::encode(e.roots.service),
                hex::encode(e.roots.audit),
            ),
            (None, Some(p)) => (
                p.entries,
                p.log_head.clone(),
                p.kernel_root.clone(),
                p.service_root.clone(),
                p.audit.clone(),
            ),
            (None, None) => (
                0,
                hex::encode([0u8; 32]),
                String::new(),
                String::new(),
                String::new(),
            ),
        };
        Ok(Point {
            inputs,
            entries,
            log_head,
            kernel_root,
            service_root,
            audit,
            at: now_ms(),
            archive_bytes: start.0 + a.len() as u64,
            replies_bytes: start.1 + r.len() as u64,
        })
    }

    fn ask_checkpoint(&self) {
        if self.tap.state.is_none() || self.conflict.is_some() {
            return;
        }
        let Some(point) = &self.point else {
            return;
        };
        let every = u64::try_from(self.config.checkpoint_every.as_millis()).unwrap_or(u64::MAX);
        let due = match &self.checkpoint {
            None => point.entries > 0,
            Some(c) => {
                point.entries >= c.entries + self.config.checkpoint_every_entries
                    || (point.entries > c.entries && now_ms().saturating_sub(c.at) >= every)
            }
        };
        if due {
            self.tap.want_checkpoint.store(true, Ordering::Release);
        }
    }

    fn status(&mut self, force: bool) {
        let now = now_ms();
        if !force && now.saturating_sub(self.last_status) < 1_000 {
            return;
        }
        self.last_status = now;
        let (since, latest) = {
            let p = lock(&self.tap.pending);
            let since = match (p.since, self.inflight) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            (since, p.cuts.last().map(|c| (c.archive, c.replies)))
        };
        let (la, lr) = latest.unwrap_or(self.latest);
        let committed = self.tap.last_input.load(Ordering::Acquire);
        let archived = self.point.as_ref().map_or(0, |p| p.inputs);
        let s = ArchiveStatus {
            at: now,
            sink: self.sink.clone(),
            segments: self.next - 1,
            point: self.point.clone(),
            committed_input: committed.max(archived),
            lag_inputs: committed.saturating_sub(archived),
            lag_ms: since.map_or(0, |s| now.saturating_sub(s)),
            pending_bytes: la.saturating_sub(self.off_archive)
                + lr.saturating_sub(self.off_replies),
            failures: self.failures,
            last_error: self.last_error.clone(),
            conflict: self.conflict.clone(),
            checkpoint: self.checkpoint.clone(),
        };
        let path = self.dir.join(STATUS_FILE);
        let tmp = self.dir.join(format!("{STATUS_FILE}.tmp"));
        let written = serde_json::to_vec(&s)
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|b| {
                let mut f = File::create(&tmp)?;
                f.write_all(&b)?;
                f.sync_data()
            })
            .and_then(|_| fs::rename(&tmp, &path));
        if let Err(e) = written {
            std::eprintln!("aledgent-archive: the status file is not written: {e}");
        }
    }
}
