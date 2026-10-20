// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The one implementation of [`Core`] this feature provides: `ledger-core` in this process
//! (`DEC-1`), behind a write-ahead journal of its inputs.
//!
//! **Durability by replay.** `ledger-core`'s service store lives in memory (`F-003` decision 1,
//! `F-007` decision 1); its durable form is `F-005`'s and `F-006`'s. So this core makes itself
//! durable the way a deterministic actor does (`REQ-RT-3`): every input is appended to a
//! [`Journal`] *before* it is applied, and a start replays the journal from genesis through a fresh
//! book. Time and every other value an input needs are in the input (`REQ-RT-7`), and the kernel's
//! signing randomness is seeded (`F-001`), so a replay reaches the same state. An input journaled
//! but not applied when the process died is applied by the replay; an input whose reply was lost
//! is answered from the replies the replay rebuilt, with its first input number (`INV-18`).
//!
//! **Delivery records** live beside the book in the same committed state: the set of
//! notifications whose delivery was recorded, rebuilt by the replay like everything else
//! (`REQ-RT-4`, `REQ-EV-6`). The outbox itself is `ledger-core`'s.
//!
//! **Memory** (`T-031-14`, finding F6 of `F-032`). Over [`InProcessCore::in_memory`] the book's
//! leaves and their Merkle nodes live in memory, about 26 KB a command: the node tables of six
//! depth-128 sparse Merkle trees, about a hundred nodes per new leaf. [`InProcessCore::on_disk`]
//! keeps them in a file instead, with a bounded page cache, rebuilt from the journal on every start
//! like the rest of the book; what stays in memory grows by about 5 KB a command (the service
//! store, by design, and the replies). It is opt-in (`core_book`): the stress suite measured it
//! too slow for 10 × the reference hour on a 2-vCPU runner, redb's writes competing with
//! PostgreSQL's syncs (`features/F-031-core-throughput/NOTES-stress-reads.md`).
//!
//! **Faults** can be injected for the chaos tests: stop the core, crash it before or after the
//! journal write, or after the apply with the reply lost.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use ledger_core::Refusal;
use ledger_core::account::Time;
use ledger_core::anchor::clerk::ClerkSigner;
use ledger_core::anchor::{AnchorStore, Checkpoint, CheckpointStatus};
use ledger_core::id::{AccountId, IdentityId, OperationId};
use ledger_core::identity::{IdentityKind, IdentityStatus};
use ledger_core::ledger::{Ctx, Founded, Genesis};
use ledger_core::lifecycle::keys::NotificationId;
use ledger_core::lifecycle::store::Notification;
use ledger_core::principal::{Caller, CredentialStatus, resolve};
use ledger_core::{Ledger, ledger::Accepted};
use ledger_kernel::book::{BindingFault, Book};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::{FileStore, FileStoreOptions, LeafStore, MemStore};
use ledger_kernel::vault::{MemVault, VaultBackend};

use crate::audit_ship::{AuditOutbox, Head, Shared as AuditShared, ShipHandle, Shipper};
use crate::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, InputBody, InputKey, OUTBOX_PAGE,
    OutboxItem, OutboxPage, Outcome, Query, QueryReply,
};
use crate::command::{self, Command};
use crate::custody::CustodyCommand;
use crate::gate::Gate;
use crate::operator::{Justified, OperatorOp};
use crate::query as q;
use crate::wire::{AnchorRefBody, CommandResult, NotificationBody, ShownBody, UndeliveredBody};
use ledger_core::ledger::custody::Claimed;
use ledger_core::ledger::lifecycle::{Acknowledged, Resumed};
use ledger_core::lifecycle::Shown;

/// Where the core's inputs are kept before they are applied.
pub trait Journal: Send + 'static {
    /// Every entry, in order.
    fn entries(&self) -> Result<Vec<(u64, CoreInput)>, String>;
    /// Append one entry durably.
    fn append(&mut self, seq: u64, input: &CoreInput) -> Result<(), String>;
}

/// A journal in memory, shared by its clones: it outlives the core instances a test starts and
/// stops, as a disk would.
#[derive(Clone, Default)]
pub struct MemJournal(Arc<Mutex<Vec<(u64, CoreInput)>>>);

impl MemJournal {
    /// An empty journal.
    pub fn new() -> MemJournal {
        MemJournal::default()
    }

    /// How many inputs it holds.
    pub fn len(&self) -> usize {
        lock(&self.0).len()
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Journal for MemJournal {
    fn entries(&self) -> Result<Vec<(u64, CoreInput)>, String> {
        Ok(lock(&self.0).clone())
    }

    fn append(&mut self, seq: u64, input: &CoreInput) -> Result<(), String> {
        lock(&self.0).push((seq, input.clone()));
        Ok(())
    }
}

/// A journal in a file: one JSON line per input, flushed to disk before the input is applied.
pub struct FileJournal {
    path: PathBuf,
}

impl FileJournal {
    /// The journal at `path`, created if absent.
    pub fn open(path: impl Into<PathBuf>) -> Result<FileJournal, String> {
        let path = path.into();
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        Ok(FileJournal { path })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Line {
    seq: u64,
    input: CoreInput,
}

impl Journal for FileJournal {
    fn entries(&self) -> Result<Vec<(u64, CoreInput)>, String> {
        let f = File::open(&self.path).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Line>(&line) {
                Ok(l) => out.push((l.seq, l.input)),
                // A torn last line: the process died mid-write, before the input was applied
                // and before any reply. It never happened.
                Err(_) => break,
            }
        }
        Ok(out)
    }

    fn append(&mut self, seq: u64, input: &CoreInput) -> Result<(), String> {
        let mut f = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        let mut bytes = serde_json::to_vec(&Line {
            seq,
            input: input.clone(),
        })
        .map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        f.write_all(&bytes).map_err(|e| e.to_string())?;
        f.sync_data().map_err(|e| e.to_string())
    }
}

/// What the core is founded on: the reviewed genesis configuration (`REQ-AS-2`), the operation
/// reference and time of genesis, and the root of the kernel's key ring.
#[derive(Clone)]
pub struct CoreGenesis {
    /// The registry, the operator's verifier and the lifecycle configuration.
    pub genesis: Genesis,
    /// Genesis's operation reference.
    pub op: OperationId,
    /// Genesis's time.
    pub at: Time,
    /// The root of the kernel's key ring. A secret: never logged (`REQ-AU-8`).
    pub root: [u8; 32],
}

/// A fault to inject into the next input (chaos tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The core dies before it journals the input: it never ran.
    CrashBeforeJournal,
    /// The core dies after it journals the input, before it applies it.
    CrashAfterJournal,
    /// The core applies the input and dies before it replies.
    CrashAfterApply,
}

/// How many of the last inputs' replies a [`Sidecar`] keeps: more than the forwarders can have
/// in flight between the core's reply and the received-log's record of it (one per leader, and
/// a brief overlap on failover, `F-009` decision 12).
pub const REPLY_TAIL: u64 = 64;

/// What the in-process core holds beside the book, at one input number (`F-018`). A backup
/// takes it with the `F-014` export, which carries the book only. Public material: identifiers,
/// times and replies, no secret.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sidecar {
    /// How many inputs the core had applied: the backup point `s_b`.
    pub inputs: u64,
    /// The operator's organisation identity.
    pub operator: String,
    /// Its first credential.
    pub operator_credential: String,
    /// The issuance counterparty.
    pub issuance: String,
    /// The delivery records (`F-009` decision 15): notification, hex, and when.
    pub delivered: Vec<(String, Time)>,
    /// The replies of the last [`REPLY_TAIL`] inputs, so that a forwarder retrying an input the
    /// core applied just before the backup gets its first reply, not a second application.
    pub replies: Vec<(InputKey, CoreReply)>,
}

type BookFactory<S, V> = Box<dyn Fn() -> Result<Book<S, V>, BindingFault> + Send>;

/// The clerk key the core signs disclosures with (`F-017`): shared, never cloned, never
/// serialised. `F-005`'s private-input path replaces it (issue #38).
pub type SharedClerk = Arc<dyn ClerkSigner + Send + Sync>;

/// The core's executor of inputs over one ledger: what an input does to the book, what a query
/// answers, what the outbox holds and which deliveries are recorded. [`InProcessCore`] runs one
/// behind its journal; `F-005`'s Agent core (`core_binding::core`) runs one as the replica it
/// checks the Agent against.
pub struct Running<S: LeafStore, V: VaultBackend> {
    ledger: Ledger<S, V>,
    clerk: Option<SharedClerk>,
    founded: Founded,
    next_seq: u64,
    replies: HashMap<InputKey, CoreReply>,
    delivered: BTreeMap<NotificationId, Time>,
}

/// What only the decision path touches: the journal, the factory, faults and the restored book.
struct Ctl<S: LeafStore, V: VaultBackend> {
    /// A restored book waiting for the first start (`F-018`).
    base: Option<Running<S, V>>,
    journal: Box<dyn Journal>,
    factory: BookFactory<S, V>,
    genesis: CoreGenesis,
    clerk: Option<SharedClerk>,
    fault: Option<Fault>,
    starts: u64,
}

/// The core's two locks (`T-031-12`, finding F3 of `F-032`). An input takes `ctl`, then the
/// running book exclusively; a read takes only the book, shared, through a [`Gate`] that lets a
/// waiting input in before any read that arrives after it. So reads run side by side, never see
/// an input half applied, and a read storm cannot starve decisions. The order is always `ctl`,
/// then `running`.
struct Parts<S: LeafStore, V: VaultBackend> {
    ctl: Mutex<Ctl<S, V>>,
    running: Gate<Option<Running<S, V>>>,
    /// The audit trail's shipping, once [`InProcessCore::with_audit`] set it (`T-013-13`).
    audit: OnceLock<(AuditShared, ShipHandle)>,
}

/// `ledger-core` in this process, behind a journal. Cloning shares the one core.
pub struct InProcessCore<S: LeafStore, V: VaultBackend> {
    inner: Arc<Parts<S, V>>,
}

impl<S: LeafStore, V: VaultBackend> Clone for InProcessCore<S, V> {
    fn clone(&self) -> Self {
        InProcessCore {
            inner: Arc::clone(&self.inner),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves nothing half-written that the next holder could
    // misread: every write the core makes goes through the journal first.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl InProcessCore<MemStore, MemVault> {
    /// A core over `F-001`'s in-memory store and vault, rebuilt from `journal` on every start.
    pub fn in_memory(genesis: CoreGenesis, journal: impl Journal) -> Self {
        let root = genesis.root;
        InProcessCore::new(genesis, journal, move || {
            Book::bootstrap(MemStore::new(), MemVault::new(), KeyRing::from_root(root))
        })
    }
}

/// The page cache of an on-disk core's leaf file by default: what its leaves hold in memory at
/// most, whatever the size of the book (`T-031-14`).
pub const BOOK_CACHE_BYTES: usize = 64 * 1024 * 1024;

impl InProcessCore<FileStore, MemVault> {
    /// A core whose book keeps its leaves in the file at `path` (`F-001`'s `FileStore`), with at
    /// most `cache_bytes` of it in memory (`T-031-14`). Like every in-process core, it is rebuilt
    /// from `journal` on every start: the file is deleted and the journal replayed into a fresh
    /// one. So its commits are not synced (the journal is what is durable), and nothing but this
    /// core may use the file. The openings vault stays in memory: one row per account.
    pub fn on_disk(
        genesis: CoreGenesis,
        journal: impl Journal,
        path: impl Into<PathBuf>,
        cache_bytes: usize,
    ) -> Self {
        let root = genesis.root;
        let path = path.into();
        InProcessCore::new(genesis, journal, move || {
            let io = |e: std::io::Error| {
                BindingFault::Store(ledger_kernel::store::StoreError::Io(format!(
                    "{}: {e}",
                    path.display()
                )))
            };
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(e)),
            }
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
            let store = FileStore::open_with(
                &path,
                FileStoreOptions {
                    durable: false,
                    cache_bytes,
                },
            )
            .map_err(BindingFault::Store)?;
            Book::bootstrap(store, MemVault::new(), KeyRing::from_root(root))
        })
    }
}

impl<S, V> InProcessCore<S, V>
where
    S: LeafStore + Send + Sync + 'static,
    V: VaultBackend + Send + Sync + 'static,
{
    /// A core whose book `factory` bootstraps afresh on every start. It is stopped until
    /// [`InProcessCore::start`].
    pub fn new(
        genesis: CoreGenesis,
        journal: impl Journal,
        factory: impl Fn() -> Result<Book<S, V>, BindingFault> + Send + 'static,
    ) -> Self {
        InProcessCore {
            inner: Arc::new(Parts {
                ctl: Mutex::new(Ctl {
                    base: None,
                    journal: Box::new(journal),
                    factory: Box::new(factory),
                    genesis,
                    clerk: None,
                    fault: None,
                    starts: 0,
                }),
                running: Gate::new(None),
                audit: OnceLock::new(),
            }),
        }
    }

    /// Start the core: found the book and replay the journal. A running core is left running.
    ///
    /// A core made by [`InProcessCore::restored`] starts the first time from its restored book
    /// and applies only the journal's entries after the backup point; every later start replays
    /// the whole journal from genesis, as any other core does.
    pub fn start(&self) -> Result<(), String> {
        let mut ctl = lock(&self.inner.ctl);
        let mut slot = self.inner.running.write();
        if slot.is_some() {
            return Ok(());
        }
        let mut running = match ctl.base.take() {
            Some(mut base) => {
                base.clerk = ctl.clerk.clone();
                base
            }
            None => {
                let book =
                    (ctl.factory)().map_err(|f| format!("the book did not bootstrap: {f:?}"))?;
                let g = ctl.genesis.clone();
                let (ledger, founded) = Ledger::genesis(book, g.genesis, g.op, g.at)
                    .map_err(|e| format!("genesis failed: {e:?}"))?;
                Running {
                    ledger,
                    clerk: ctl.clerk.clone(),
                    founded,
                    next_seq: 1,
                    replies: HashMap::new(),
                    delivered: BTreeMap::new(),
                }
            }
        };
        // The records the audit store already holds are dropped as they are replayed, so the
        // outbox never holds the trail again (`T-013-13`).
        let mut floor = self.inner.audit.get().and_then(|(a, _)| a.floor());
        floor = running.acknowledge_audit_floor(floor);
        let from = running.next_seq;
        for (seq, input) in ctl.journal.entries()? {
            if seq < from {
                continue;
            }
            if seq != running.next_seq {
                return Err(format!(
                    "the journal is not gapless: expected input {}, found {seq}",
                    running.next_seq
                ));
            }
            let outcome = running.execute(&input);
            running.replies.insert(
                input.key,
                CoreReply {
                    input_seq: seq,
                    outcome,
                },
            );
            running.next_seq += 1;
            floor = running.acknowledge_audit_floor(floor);
        }
        self.observe_audit(&running);
        *slot = Some(running);
        ctl.starts += 1;
        Ok(())
    }

    /// Ship the audit trail to `sink`, the audit store, from a thread of its own (`T-013-13`,
    /// `REQ-AT-2`; [`crate::audit_ship`]), and refuse new inputs while `max_unshipped` records
    /// wait. Call it before [`InProcessCore::start`], so that the first start drops what the store
    /// already holds as it replays.
    pub fn with_audit(
        self,
        sink: Box<dyn ledger_core::audit::AuditSink + Send>,
        max_unshipped: u64,
    ) -> Result<Self, String> {
        let head = sink
            .head()
            .map_err(|e| format!("the audit store's head does not read: {e:?}"))?;
        let shared = AuditShared::new(head, max_unshipped);
        let outbox = InProcessOutbox(Arc::downgrade(&self.inner));
        let handle = Shipper::new(sink, shared.clone())
            .spawn(Box::new(outbox), crate::audit_ship::SHIP_INTERVAL);
        if self.inner.audit.set((shared, handle)).is_err() {
            return Err("the audit store is set already".into());
        }
        Ok(self)
    }

    /// The audit shipping's shared state, if the core ships.
    pub fn audit(&self) -> Option<&AuditShared> {
        self.inner.audit.get().map(|(a, _)| a)
    }

    /// Whether the audit outbox is at its bound: inputs wait (`T-013-13`).
    fn audit_full(&self) -> bool {
        self.inner.audit.get().is_some_and(|(a, _)| a.stats.full())
    }

    /// Record the outbox's size and age after a decision, and wake the shipper.
    fn observe_audit(&self, running: &Running<S, V>) {
        if let Some((a, handle)) = self.inner.audit.get() {
            let (n, oldest) = running.audit_outbox();
            a.stats.observe(n, oldest);
            if n > 0 {
                handle.nudge();
            }
        }
    }

    /// Hold `clerk` as the deployment's clerk key, from the next start (`F-017`). The core signs
    /// disclosures with it; without one, `issue_disclosure` is `ERR-DisclosureUnsupported`.
    pub fn with_clerk(self, clerk: SharedClerk) -> Self {
        lock(&self.inner.ctl).clerk = Some(clerk);
        self
    }

    /// Stop the core as a crash would: whatever is not in the journal is gone, and so is any
    /// fault not yet fired.
    pub fn stop(&self) {
        let mut ctl = lock(&self.inner.ctl);
        *self.inner.running.write() = None;
        ctl.fault = None;
    }

    /// Whether it is running.
    pub fn is_up(&self) -> bool {
        self.inner.running.read().is_some()
    }

    /// How many times it has started.
    pub fn starts(&self) -> u64 {
        lock(&self.inner.ctl).starts
    }

    /// Inject a fault into the next input.
    pub fn inject(&self, fault: Fault) {
        lock(&self.inner.ctl).fault = Some(fault);
    }

    /// What genesis created, while running.
    pub fn founded(&self) -> Option<Founded> {
        self.inner.running.read().as_ref().map(|r| r.founded)
    }

    /// How many inputs the core has applied, while running.
    pub fn inputs(&self) -> Option<u64> {
        self.inner
            .running
            .read()
            .as_ref()
            .map(|r| r.next_seq.saturating_sub(1))
    }

    /// Read the running ledger (tests and the conformance suite).
    pub fn with_ledger<T>(&self, f: impl FnOnce(&Ledger<S, V>) -> T) -> Option<T> {
        self.inner.running.read().as_ref().map(|r| f(&r.ledger))
    }

    /// Run `f` over the running ledger and the core's [`Sidecar`], under the core's lock: the
    /// book and the sidecar are one point of the input sequence. `F-018`'s backup job takes the
    /// `F-014` export here. `None` while the core is stopped.
    pub fn checkpoint<T>(&self, f: impl FnOnce(&mut Ledger<S, V>, &Sidecar) -> T) -> Option<T> {
        let mut slot = self.inner.running.write();
        let running = slot.as_mut()?;
        let sidecar = running.sidecar();
        Some(f(&mut running.ledger, &sidecar))
    }

    /// A core restored from a backup (`F-018`, `REQ-RT-3`): `ledger` is the book imported from
    /// the backup's `F-014` export, which stood at input number `sidecar.inputs`. The first
    /// [`start`](InProcessCore::start) takes it with the sidecar's delivery records and replies,
    /// and applies the journal's entries after that number. `journal` should hold every input
    /// from the first (`F-018` rebuilds it from the received-log), so that a later start can
    /// replay it from genesis like any other core's.
    pub fn restored(
        genesis: CoreGenesis,
        journal: impl Journal,
        factory: impl Fn() -> Result<Book<S, V>, BindingFault> + Send + 'static,
        ledger: Ledger<S, V>,
        sidecar: &Sidecar,
    ) -> Result<Self, String> {
        let base = Running::restored(ledger, sidecar)?;
        let core = InProcessCore::new(genesis, journal, factory);
        lock(&core.inner.ctl).base = Some(base);
        Ok(core)
    }

    fn apply_now(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        let mut ctl = lock(&self.inner.ctl);
        let mut slot = self.inner.running.write();
        let Some(running) = slot.as_mut() else {
            return Err(CoreError::Unavailable);
        };
        if let Some(reply) = running.replies.get(&input.key) {
            return Ok(reply.clone());
        }
        // Back-pressure (`T-013-13`): the audit store is not taking records, and the outbox is at
        // its bound. The input waits `Received`; nothing decides without its record shipped.
        if self.audit_full() {
            return Err(CoreError::Unavailable);
        }
        let fault = ctl.fault.take();
        if fault == Some(Fault::CrashBeforeJournal) {
            *slot = None;
            return Err(CoreError::Unavailable);
        }
        let seq = running.next_seq;
        if ctl.journal.append(seq, input).is_err() {
            // A write that failed may have reached the disk; nothing can be assumed.
            *slot = None;
            return Err(CoreError::Ambiguous);
        }
        if fault == Some(Fault::CrashAfterJournal) {
            *slot = None;
            return Err(CoreError::Ambiguous);
        }
        let outcome = running.execute(input);
        let reply = CoreReply {
            input_seq: seq,
            outcome,
        };
        running.replies.insert(input.key, reply.clone());
        running.next_seq += 1;
        self.observe_audit(running);
        if fault == Some(Fault::CrashAfterApply) {
            *slot = None;
            return Err(CoreError::Ambiguous);
        }
        Ok(reply)
    }

    /// Apply `inputs` in order under one hold of the book, the kernel's leaves of all of them
    /// committed once (`T-031-19`, `Ledger::begin_batch`): each decides as it would alone, and
    /// its reply is the one it would have alone. A failed commit of the batch loses all of it:
    /// the core stops, and its next start replays the journal (`REQ-RT-3`), so every input of
    /// the batch answers [`CoreError::Ambiguous`] and is retried under its key. With a fault
    /// armed (tests), the inputs go one by one.
    fn apply_batch_now(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        let mut ctl = lock(&self.inner.ctl);
        if ctl.fault.is_some() || inputs.len() < 2 {
            drop(ctl);
            let mut out = Vec::with_capacity(inputs.len());
            for input in inputs {
                let r = self.apply_now(input);
                let failed = r.is_err();
                out.push(r);
                if failed {
                    break;
                }
            }
            out.resize_with(inputs.len(), || Err(CoreError::Unavailable));
            return out;
        }
        let mut slot = self.inner.running.write();
        let Some(running) = slot.as_mut() else {
            return inputs.iter().map(|_| Err(CoreError::Unavailable)).collect();
        };
        if self.audit_full() {
            return inputs.iter().map(|_| Err(CoreError::Unavailable)).collect();
        }
        running.ledger.begin_batch();
        let mut out: Vec<Result<CoreReply, CoreError>> = Vec::with_capacity(inputs.len());
        let mut fresh: Vec<(InputKey, CoreReply)> = Vec::new();
        let mut journal_failed = false;
        for input in inputs {
            let seen = running.replies.get(&input.key).cloned().or_else(|| {
                fresh
                    .iter()
                    .find(|(k, _)| *k == input.key)
                    .map(|(_, r)| r.clone())
            });
            if let Some(reply) = seen {
                out.push(Ok(reply));
                continue;
            }
            let seq = running.next_seq;
            if ctl.journal.append(seq, input).is_err() {
                journal_failed = true;
                break;
            }
            let reply = CoreReply {
                input_seq: seq,
                outcome: running.execute(input),
            };
            running.next_seq += 1;
            fresh.push((input.key, reply.clone()));
            out.push(Ok(reply));
        }
        let committed = running.ledger.end_batch().is_ok();
        if journal_failed || !committed {
            // What the batch decided is not all committed; nothing can be assumed of it.
            *slot = None;
            let fresh_keys: Vec<InputKey> = fresh.iter().map(|(k, _)| *k).collect();
            for (input, r) in inputs.iter().zip(out.iter_mut()) {
                if fresh_keys.contains(&input.key) {
                    *r = Err(CoreError::Ambiguous);
                }
            }
            if journal_failed {
                // The input whose append failed may have reached the disk.
                out.push(Err(CoreError::Ambiguous));
            }
            out.resize_with(inputs.len(), || Err(CoreError::Unavailable));
            return out;
        }
        for (k, r) in fresh {
            running.replies.insert(k, r);
        }
        self.observe_audit(running);
        out
    }

    fn read<T>(&self, f: impl FnOnce(&Running<S, V>) -> T) -> Result<T, CoreError> {
        self.inner
            .running
            .read()
            .as_ref()
            .map(f)
            .ok_or(CoreError::Unavailable)
    }

    /// A read off the async runtime's threads: it may wait for an input in flight.
    async fn read_blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Running<S, V>) -> T + Send + 'static,
    ) -> Result<T, CoreError> {
        let core = self.clone();
        tokio::task::spawn_blocking(move || core.read(f))
            .await
            .unwrap_or(Err(CoreError::Unavailable))
    }
}

fn caller_of(c: &CallerBody) -> Result<Caller, Refusal> {
    let principal = command::identity_id(&c.principal).map_err(|_| Refusal::CredentialInvalid)?;
    let credential = command::credential(&c.credential)?;
    let on_behalf_of = c
        .on_behalf_of
        .as_deref()
        .map(command::identity_id)
        .transpose()?;
    Ok(Caller {
        principal,
        credential,
        on_behalf_of,
    })
}

fn decided<T>(
    op: &OperationId,
    a: Accepted<T>,
    result: impl FnOnce(T) -> CommandResult,
) -> Outcome {
    Outcome::Decided {
        operation_reference: op.to_did(),
        state: a.status.map(ShownBody::from),
        result: result(a.value),
    }
}

fn postings(v: Vec<u64>) -> CommandResult {
    CommandResult {
        postings: v,
        ..CommandResult::default()
    }
}

fn posting(v: u64) -> CommandResult {
    postings(vec![v])
}

fn nothing(_: ()) -> CommandResult {
    CommandResult::default()
}

/// A notification on the wire. The anchor it names is resolved from the anchor store: a final
/// checkpoint never changes, so every read gives the same body (`REQ-EV-3`, `AC-M2.4`).
fn notification_body(n: &Notification, anchors: &AnchorStore) -> NotificationBody {
    let shown = ShownBody::from(n.shown);
    NotificationBody {
        id: hex::encode(n.id.0),
        operation_reference: n.op.to_did(),
        state: shown.state,
        class: shown.class,
        terminal: shown.terminal,
        outcome: n.outcome.map(|s| s.name().to_owned()),
        error: n.refusal,
        anchor: n
            .anchor
            .and_then(|number| anchors.checkpoint(number))
            .and_then(anchor_ref_body),
        // Ingress adds it before it signs the delivery (`REQ-PO-5`): the core never holds it.
        reference: None,
    }
}

/// A final checkpoint as a notification names it; `None` for one that is not final.
fn anchor_ref_body(c: &Checkpoint) -> Option<AnchorRefBody> {
    if c.status != CheckpointStatus::Final {
        return None;
    }
    Some(AnchorRefBody {
        number: c.number,
        entry_sequence: c.entry_sequence,
        coordinate: crate::settlement::coordinate_body(c.coordinate.as_ref()?),
        height: c.height?,
    })
}

impl<S: LeafStore, V: VaultBackend> Running<S, V> {
    /// An executor over a founded ledger, with nothing applied yet through it.
    pub fn new(ledger: Ledger<S, V>, founded: Founded, clerk: Option<SharedClerk>) -> Self {
        Running {
            ledger,
            clerk,
            founded,
            next_seq: 1,
            replies: HashMap::new(),
            delivered: BTreeMap::new(),
        }
    }

    /// The ledger.
    pub fn ledger(&self) -> &Ledger<S, V> {
        &self.ledger
    }

    /// The ledger, mutably: for a driver that sends what the ledger recorded (`F-005`).
    pub fn ledger_mut(&mut self) -> &mut Ledger<S, V> {
        &mut self.ledger
    }

    /// The audit outbox: how many records wait for the audit store, and the oldest one's time
    /// (`T-013-13`).
    pub fn audit_outbox(&self) -> (usize, Option<Time>) {
        let log = self.ledger.state().audit();
        (log.unshipped(), log.oldest_unshipped().map(|r| r.at))
    }

    /// Up to `limit` records of the audit outbox, in order (`T-013-13`).
    pub fn audit_pending(&self, limit: usize) -> Vec<ledger_core::audit::AuditRecord> {
        self.ledger
            .state()
            .audit()
            .outbox()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Drop from the audit outbox what the audit store holds through `floor`, as a rebuild
    /// replays (`T-013-13`). Returns the floor to go on with: `None` once the record emitted at
    /// the floor's number is not the store's, which the shipper then finds and reports.
    pub fn acknowledge_audit_floor(&mut self, floor: Option<Head>) -> Option<Head> {
        let f = floor?;
        match self.ledger.acknowledge_audit(f) {
            Ok(_) => Some(f),
            Err(_) => None,
        }
    }

    /// What genesis created.
    pub fn founded(&self) -> Founded {
        self.founded
    }

    /// Whether the delivery of notification `id` is recorded.
    pub fn is_delivered(&self, id: &NotificationId) -> bool {
        self.delivered.contains_key(id)
    }

    /// Record a delivery found in a committed record (a rebuild from the Agent's inputs).
    pub fn restore_delivery(&mut self, id: NotificationId, at: Time) {
        self.delivered.entry(id).or_insert(at);
    }

    /// The time of the last tick the ledger applied.
    pub fn last_tick(&self) -> Option<Time> {
        self.ledger.state().operations().clock()
    }

    /// The integrating party `principal` is tied to, if any (`REQ-ID-7`): the client key's domain
    /// at ingress (`REQ-OP-10`, T-009-16). An identifier that does not parse names no principal.
    pub fn party_of(&self, principal: &str) -> Option<String> {
        let id = command::identity_id(principal).ok()?;
        self.ledger.party_of(&id).map(|p| p.to_did())
    }

    /// One working credential, for authentication at ingress (`F-028`): absent unless the
    /// credential is active and its identity is active.
    pub fn credential(&self, credential: &str) -> Option<crate::boundary::CredentialBody> {
        let id = command::credential(credential).ok()?;
        let state = self.ledger.state();
        let c = state
            .credential(&id)
            .filter(|c| c.status() == CredentialStatus::Active)?;
        let identity = state
            .identity(&c.identity())
            .filter(|i| i.status() == IdentityStatus::Active)?;
        Some(crate::boundary::CredentialBody {
            identity: identity.id().to_did(),
            kind: match identity.kind() {
                IdentityKind::Person => "person",
                IdentityKind::Organisation => "organisation",
                IdentityKind::Agent => "agent",
            }
            .to_owned(),
            scheme: c.verifier().scheme().name().to_owned(),
            verifier: hex::encode(c.verifier().as_bytes()),
        })
    }

    fn sidecar(&self) -> Sidecar {
        let inputs = self.next_seq.saturating_sub(1);
        let mut replies: Vec<(InputKey, CoreReply)> = self
            .replies
            .iter()
            .filter(|(_, r)| r.input_seq + REPLY_TAIL > inputs)
            .map(|(k, r)| (*k, r.clone()))
            .collect();
        replies.sort_by_key(|(_, r)| r.input_seq);
        Sidecar {
            inputs,
            operator: self.founded.operator.to_did(),
            operator_credential: self.founded.operator_credential.to_did(),
            issuance: self.founded.issuance.to_did(),
            delivered: self
                .delivered
                .iter()
                .map(|(id, at)| (hex::encode(id.0), *at))
                .collect(),
            replies,
        }
    }

    fn restored(ledger: Ledger<S, V>, s: &Sidecar) -> Result<Running<S, V>, String> {
        let bad = |what: &str| format!("the sidecar's {what} does not parse");
        let founded = Founded {
            operator: IdentityId::parse(&s.operator).map_err(|_| bad("operator"))?,
            operator_credential: ledger_core::id::CredentialId::parse(&s.operator_credential)
                .map_err(|_| bad("operator credential"))?,
            issuance: AccountId::parse(&s.issuance).map_err(|_| bad("issuance counterparty"))?,
        };
        let mut delivered = BTreeMap::new();
        for (id, at) in &s.delivered {
            let id = hex::decode(id)
                .ok()
                .and_then(|v| <[u8; 32]>::try_from(v).ok())
                .ok_or_else(|| bad("delivery record"))?;
            delivered.insert(NotificationId(id), *at);
        }
        Ok(Running {
            ledger,
            clerk: None,
            founded,
            next_seq: s.inputs + 1,
            replies: s.replies.iter().cloned().collect(),
            delivered,
        })
    }

    /// Execute one input against the ledger: what [`Core::apply`] does, without the journal or
    /// the input numbering.
    pub fn execute(&mut self, input: &CoreInput) -> Outcome {
        // The writer decides this input as its `next_seq`th: a change of the upgrade-authority
        // mode takes force at its input, and a delay counts inputs (`T-037-05`), as in the Agent.
        self.ledger.set_input(self.next_seq);
        match &input.body {
            InputBody::Command {
                caller,
                operation,
                received_at,
                now,
                command,
            } => {
                let refused = |error| Outcome::Refused {
                    operation_reference: operation.clone(),
                    error,
                };
                let caller = match caller_of(caller) {
                    Ok(c) => c,
                    Err(r) => return refused(r),
                };
                let op = match OperationId::parse(operation) {
                    Ok(op) => op,
                    Err(_) => return refused(Refusal::UnknownOperation),
                };
                let command = match Command::parse(command) {
                    Ok(c) => c,
                    Err(r) => return refused(r),
                };
                let ctx = Ctx {
                    caller,
                    op,
                    now: *now,
                };
                self.command(&ctx, *received_at, command)
                    .unwrap_or_else(refused)
            }
            // A seeded book that is not sealed moves no deadline: it holds nothing but its seed
            // (`F-036`). The tick is answered as one below the clock, and nothing is recorded.
            InputBody::Tick { .. } if self.ledger.state().spec_open() => Outcome::Ticked {
                ignored: true,
                moved: 0,
                queued: 0,
                incidents: 0,
            },
            InputBody::SpecChunk { chunk, path } => self.spec_chunk(chunk, path),
            InputBody::Tick { now } => match self.ledger.tick(*now) {
                Ok(t) => Outcome::Ticked {
                    ignored: t.ignored,
                    moved: t.moved.len() as u64,
                    queued: t.queued.len() as u64,
                    incidents: t.incidents.len() as u64,
                },
                Err(error) => Outcome::Refused {
                    operation_reference: String::new(),
                    error,
                },
            },
            InputBody::RecordDelivery { notification, at } => {
                let id = hex::decode(notification)
                    .ok()
                    .and_then(|v| <[u8; 32]>::try_from(v).ok())
                    .map(NotificationId);
                let known = id.is_some_and(|id| {
                    let known = self.ledger.state().operations().notification(&id).is_some();
                    if known {
                        self.delivered.entry(id).or_insert(*at);
                    }
                    known
                });
                Outcome::Recorded {
                    notification: notification.clone(),
                    known,
                }
            }
        }
    }

    /// One seed chunk, as the operator (`F-036`): its references and time derive from the seed.
    fn spec_chunk(&mut self, chunk: &str, path: &[String]) -> Outcome {
        let refused = |error| Outcome::Refused {
            operation_reference: String::new(),
            error,
        };
        let Ok(chunk) = hex::decode(chunk) else {
            return refused(Refusal::MalformedCommand);
        };
        let mut nodes = Vec::with_capacity(path.len());
        for n in path {
            match hex::decode(n)
                .ok()
                .and_then(|v| <[u8; 32]>::try_from(v).ok())
            {
                Some(n) => nodes.push(n),
                None => return refused(Refusal::MalformedCommand),
            }
        }
        let ctx = Ctx {
            caller: Caller::new(self.founded.operator, self.founded.operator_credential),
            op: OperationId::from_ref(ledger_kernel::posting::OperationRef([0; 16])),
            now: 0,
        };
        match self.ledger.apply_spec_chunk(&ctx, &chunk, &nodes) {
            Ok(a) => Outcome::SpecApplied {
                index: a.value.index,
                applied: a.value.applied,
                chunks: a.value.chunks,
                sealed: a.value.sealed,
            },
            Err(error) => refused(error),
        }
    }

    fn command(
        &mut self,
        ctx: &Ctx,
        received_at: Time,
        command: Command,
    ) -> Result<Outcome, Refusal> {
        let l = &mut self.ledger;
        let op = &ctx.op;
        Ok(match command {
            Command::Transfer(t) => {
                // Taken in at `Received` when ingress logged it, then decided under the same
                // reference: a refusal is recorded as `Received → Rejected` (`F-008`).
                let received = Ctx {
                    now: received_at,
                    ..*ctx
                };
                l.receive_transfer(&received, &t)?;
                decided(op, l.transfer(ctx, &t)?, postings)
            }
            Command::Reserve {
                payer,
                asset,
                amount,
                deadline,
            } => {
                // Acknowledged at `Received` (amendment 0007, `OQ-61` as ruled on 2026-09-29),
                // then decided under the same reference: a refusal is `Received → Rejected`.
                let received = Ctx {
                    now: received_at,
                    ..*ctx
                };
                l.receive_command(&received, Acknowledged::Reserve)?;
                decided(
                    op,
                    l.reserve_until(ctx, payer, asset, amount, deadline)?,
                    posting,
                )
            }
            Command::Capture {
                destination,
                cleared,
                ..
            } => decided(op, l.capture(ctx, destination, cleared)?, posting),
            Command::Release { .. } => decided(op, l.release(ctx)?, posting),
            Command::Issue {
                asset,
                destination,
                amount,
            } => decided(op, l.issue(ctx, asset, destination, amount)?, posting),
            Command::Redeem {
                asset,
                source,
                amount,
            } => decided(op, l.redeem(ctx, asset, source, amount)?, posting),
            Command::Withdraw(w) => {
                let received = Ctx {
                    now: received_at,
                    ..*ctx
                };
                l.receive_command(&received, Acknowledged::Withdraw)?;
                decided(op, l.withdraw(ctx, &w)?, posting)
            }
            Command::OpenIdentity {
                kind,
                acts_for,
                verifier,
            } => decided(op, l.open_identity(ctx, kind, acts_for, verifier.0)?, |o| {
                CommandResult {
                    identity: Some(o.identity.to_did()),
                    credential: Some(o.credential.to_did()),
                    ..CommandResult::default()
                }
            }),
            Command::OpenAccount(o) => decided(op, l.open_account(ctx, o)?, |a| CommandResult {
                account: Some(a.to_did()),
                ..CommandResult::default()
            }),
            Command::CloseAccount { account } => {
                decided(op, l.close_account(ctx, account)?, nothing)
            }
            Command::GrantAuthority(g) => {
                decided(op, l.grant_authority(ctx, g)?, |g| CommandResult {
                    grant: Some(g.to_did()),
                    ..CommandResult::default()
                })
            }
            Command::RevokeAuthority { grant } => {
                decided(op, l.revoke_authority(ctx, grant)?, nothing)
            }
            Command::Operator(j) => self.operator(ctx, j)?,
            Command::Custody(c) => self.custody(ctx, c)?,
            Command::Settlement(s) => crate::settlement::execute(l, ctx, s)?,
            Command::Anchoring(a) => {
                let clerk = self.clerk.as_deref().map(|c| c as &dyn ClerkSigner);
                crate::anchoring::execute(l, ctx, a, clerk)?
            }
        })
    }

    /// A holder's custody command (`F-012`): its semantics are the ledger's; this only
    /// dispatches.
    fn custody(&mut self, ctx: &Ctx, c: CustodyCommand) -> Result<Outcome, Refusal> {
        let l = &mut self.ledger;
        let op = &ctx.op;
        let credential = |c: ledger_core::id::CredentialId| CommandResult {
            credential: Some(c.to_did()),
            ..CommandResult::default()
        };
        Ok(match c {
            CustodyCommand::RegisterCredential(r) => {
                decided(op, l.register_credential(ctx, r)?, credential)
            }
            CustodyCommand::RemoveCredential(c) => {
                decided(op, l.remove_credential(ctx, c)?, nothing)
            }
            CustodyCommand::DelegateCustody(a) => decided(op, l.delegate_custody(ctx, a)?, nothing),
            CustodyCommand::ReclaimCustody(r) => decided(op, l.reclaim_custody(ctx, r)?, nothing),
            CustodyCommand::CancelRecovery(p) => decided(op, l.cancel_recovery(ctx, p)?, nothing),
            CustodyCommand::NameSuccessor(a, s) => {
                decided(op, l.name_successor(ctx, a, s)?, nothing)
            }
            CustodyCommand::ClaimSuccession(a) => {
                decided(op, l.claim_succession(ctx, a)?, |c| match c {
                    Claimed::Pending {
                        pending,
                        effective_at,
                    } => CommandResult {
                        pending: Some(pending.0.to_did()),
                        effective_at: Some(effective_at),
                        ..CommandResult::default()
                    },
                    Claimed::Completed { pending } => CommandResult {
                        pending: Some(pending.0.to_did()),
                        ..CommandResult::default()
                    },
                })
            }
        })
    }

    /// An operator command (`F-013`): its semantics are the ledger's; this only dispatches.
    fn operator(&mut self, ctx: &Ctx, j: Justified) -> Result<Outcome, Refusal> {
        let l = &mut self.ledger;
        let reference = j.operation(ctx.op);
        let why = j.justification.as_slice();
        let grant = |g: ledger_core::id::GrantId| CommandResult {
            grant: Some(g.to_did()),
            ..CommandResult::default()
        };
        Ok(match j.op {
            OperatorOp::Freeze { account, frozen } => {
                let a = if frozen {
                    l.freeze_account(ctx, account, why)?
                } else {
                    l.unfreeze_account(ctx, account, why)?
                };
                decided(&reference, a, nothing)
            }
            OperatorOp::Resume(target) => {
                let state = match l.resume(ctx, target, why)? {
                    Resumed::KernelRefused(r) => return Err(r),
                    Resumed::Moved(s) => Some(Shown::of(s)),
                    Resumed::Nothing => l.operation(&target).map(|o| o.shown()),
                };
                Outcome::Decided {
                    operation_reference: reference.to_did(),
                    state: state.map(ShownBody::from),
                    result: CommandResult::default(),
                }
            }
            OperatorOp::Resolve(r) => {
                let a = l.resolve_operation(ctx, r, why)?;
                let state = Shown::of(a.value);
                Outcome::Decided {
                    operation_reference: reference.to_did(),
                    state: Some(ShownBody::from(state)),
                    result: CommandResult {
                        postings: a.record.postings,
                        ..CommandResult::default()
                    },
                }
            }
            OperatorOp::SubmitEvidence(e) => {
                let a = l.submit_evidence(ctx, e, why)?;
                Outcome::Decided {
                    operation_reference: a.value.to_did(),
                    state: l.operation(&e.op).map(|o| ShownBody::from(o.shown())),
                    result: CommandResult::default(),
                }
            }
            OperatorOp::ProposeReversal(target) => {
                let a = l.propose_reversal(ctx, target, why)?;
                let proposal = a.value;
                Outcome::Decided {
                    operation_reference: proposal.to_did(),
                    state: a.status.map(ShownBody::from),
                    result: CommandResult::default(),
                }
            }
            OperatorOp::ApproveReversal(proposal) => {
                decided(&reference, l.approve_reversal(ctx, proposal, why)?, posting)
            }
            OperatorOp::RegisterAsset(r) => {
                decided(&reference, l.register_asset(ctx, r, why)?, |_| {
                    CommandResult::default()
                })
            }
            OperatorOp::DeprecateAsset(a, r) => {
                decided(&reference, l.deprecate_asset(ctx, a, r, why)?, nothing)
            }
            OperatorOp::BindAsset(a, loc) => {
                decided(&reference, l.bind_asset(ctx, a, loc, why)?, |_| {
                    CommandResult::default()
                })
            }
            OperatorOp::GrantIssuance(g) => {
                decided(&reference, l.grant_issuance(ctx, g, why)?, grant)
            }
            OperatorOp::RevokeIssuance(g) => {
                decided(&reference, l.revoke_issuance(ctx, g, why)?, nothing)
            }
            OperatorOp::GrantOperator {
                principal,
                kinds,
                expires,
            } => decided(
                &reference,
                l.grant_operator(ctx, principal, kinds, expires, why)?,
                grant,
            ),
            OperatorOp::RevokeOperator(g) => {
                decided(&reference, l.revoke_operator(ctx, g, why)?, nothing)
            }
            OperatorOp::DeclareAdapterSupport(legs) => decided(
                &reference,
                l.declare_adapter_support(ctx, legs, why)?,
                nothing,
            ),
            OperatorOp::InitiateRecovery(r) => {
                let a = l.initiate_recovery(ctx, r, why)?;
                let pending = a.value;
                let effective_at = l
                    .state()
                    .custody()
                    .pending(&pending)
                    .map(|c| c.effective_at);
                decided(&reference, a, |p| CommandResult {
                    pending: Some(p.0.to_did()),
                    effective_at,
                    ..CommandResult::default()
                })
            }
            OperatorOp::ImportCredentials(r) => {
                decided(&reference, l.import_credentials(ctx, r, why)?, |ids| {
                    CommandResult {
                        credentials: ids.iter().map(|c| c.to_did()).collect(),
                        ..CommandResult::default()
                    }
                })
            }
            OperatorOp::RegisterIntegratingParty(r) => decided(
                &reference,
                l.register_integrating_party(ctx, r, why)?,
                |p| CommandResult {
                    party: Some(p.to_did()),
                    ..CommandResult::default()
                },
            ),
            OperatorOp::TiePrincipal { party, principal } => decided(
                &reference,
                l.tie_principal(ctx, party, principal, why)?,
                nothing,
            ),
            OperatorOp::RegisterAlias {
                account,
                source,
                name,
            } => decided(
                &reference,
                l.register_alias(ctx, source, name.as_bytes(), account, why)?,
                nothing,
            ),
            OperatorOp::CompleteRecovery(pending, approval) => decided(
                &reference,
                l.complete_recovery(ctx, pending, approval, why)?,
                |c| CommandResult {
                    credential: Some(c.to_did()),
                    pending: Some(pending.0.to_did()),
                    ..CommandResult::default()
                },
            ),
            // The writer's code (`T-037-04`, `T-037-15`): decided here, scheduled by the writer
            // that runs the ledger (the core Agent), which reads what was taken.
            OperatorOp::Upgrade { code, from } => {
                decided(&reference, l.upgrade(ctx, code, from, why)?, nothing)
            }
            OperatorOp::RescheduleUpgrade { from } => {
                decided(&reference, l.reschedule_upgrade(ctx, from, why)?, nothing)
            }
            OperatorOp::CancelUpgrade => decided(&reference, l.cancel_upgrade(ctx, why)?, nothing),
        })
    }

    /// The principal a notification is owed to: the operation's initiator, else the operator.
    fn recipient(&self, op: &OperationId) -> Option<IdentityId> {
        let state = self.ledger.state();
        state
            .operations()
            .get(op)
            .and_then(|o| o.initiator())
            .or_else(|| state.roles().map(|r| r.operator))
    }

    fn created_at(&self, n: &Notification) -> Time {
        self.ledger
            .state()
            .operations()
            .get(&n.op)
            .and_then(|o| {
                o.transitions()
                    .iter()
                    .find(|t| NotificationId::derive(&n.op, &t.key) == n.id)
                    .map(|t| t.at)
            })
            .unwrap_or(0)
    }

    /// A page of the outbox, as [`Core::outbox`] answers it.
    pub fn outbox(&self, cursor: Option<&str>, undelivered: bool) -> OutboxPage {
        let after = cursor
            .and_then(|c| hex::decode(c).ok())
            .and_then(|v| <[u8; 32]>::try_from(v).ok())
            .map(NotificationId);
        let mut items = Vec::new();
        let mut next = None;
        let anchors = self.ledger.state().anchoring();
        for n in self.ledger.state().operations().outbox() {
            if after.is_some_and(|a| n.id <= a) {
                continue;
            }
            let delivered_at = self.delivered.get(&n.id).copied();
            if undelivered && delivered_at.is_some() {
                continue;
            }
            if items.len() == OUTBOX_PAGE {
                next = items.last().map(|i: &OutboxItem| i.notification.id.clone());
                break;
            }
            items.push(OutboxItem {
                notification: notification_body(n, anchors),
                recipient: self.recipient(&n.op).map(|r| r.to_did()),
                created_at: self.created_at(n),
                delivered_at,
            });
        }
        OutboxPage { items, next }
    }

    /// The core's signals with the outbox's: undelivered notifications are the delivery records'
    /// (`F-009` decision 15), which live beside the book here.
    fn signals_body(&self, s: ledger_core::ledger::operator::Signals, now: Time) -> q::SignalsBody {
        let mut undelivered = 0;
        let mut oldest: Option<Time> = None;
        for n in self.ledger.state().operations().outbox() {
            if self.delivered.contains_key(&n.id) {
                continue;
            }
            undelivered += 1;
            let age = now.saturating_sub(self.created_at(n));
            oldest = Some(oldest.map_or(age, |o| o.max(age)));
        }
        q::SignalsBody {
            now,
            open: s.open,
            oldest_open_age: s.oldest_open_age,
            received: s.received,
            review: s.review,
            needs_review: s.needs_review,
            review_overdue: s.review_overdue,
            review_most_overdue: s.review_most_overdue,
            review_service_level: s.review_service_level,
            undelivered,
            oldest_undelivered_age: oldest,
            tick_age: s.last_tick.map(|t| now.saturating_sub(t)),
            oldest_undecided_age: None,
            completion_p50: None,
            completion_p99: None,
            incidents: s
                .incidents
                .iter()
                .map(|(kind, (count, latest))| q::IncidentCountBody {
                    kind: (*kind).to_owned(),
                    count: *count,
                    latest: *latest,
                })
                .collect(),
            anchor_lag: s.anchor_lag,
            anchor_in_flight_age: s.anchor_in_flight_age,
            anchor_incidents: s
                .anchor_incidents
                .iter()
                .map(|(kind, count)| q::IncidentCountBody {
                    kind: (*kind).to_owned(),
                    count: *count,
                    latest: 0,
                })
                .collect(),
            settlement_differing: s.settlement_differing,
            settlement_comparison_age: s.settlement_comparison_age,
            core_halted: false,
            audit: None,
            vos_unacknowledged: None,
            writer_fenced_age: None,
            book: Some(q::BookSizeBody {
                records: s.book.records,
                leaves: s.book.leaves,
                genesis_records: s.book.genesis_records,
            }),
        }
    }

    /// Answer a query, as [`Core::query`] does.
    pub fn query(&self, caller: &CallerBody, query: &Query, now: Time) -> QueryReply {
        let refused = |error| QueryReply::Refused { error };
        let caller = match caller_of(caller) {
            Ok(c) => c,
            Err(r) => return refused(r),
        };
        // The settlement adapter's one read (`F-016`): its principal is refused below.
        if matches!(query, Query::Instructions) {
            return crate::settlement::instructions(&self.ledger, &caller);
        }
        // The executor's read of the call outbox (`F-027`): the adapter's principal only.
        if matches!(query, Query::Calls) {
            return crate::settlement::calls(&self.ledger, &caller);
        }
        // The adapter's read of the evidence it has not checked (`REQ-RC-5`).
        if matches!(query, Query::Evidence) {
            return crate::settlement::evidence(&self.ledger, &caller);
        }
        // The integrating parties and their principals (`REQ-ID-7`): the operator's.
        if matches!(query, Query::IntegratingParties) {
            return match self.ledger.integrating_parties(&caller) {
                Ok(list) => QueryReply::IntegratingParties(parties_body(&list)),
                Err(error) => refused(error),
            };
        }
        // An alias, and the grants over an account (`REQ-RC-7`, amendment 0024).
        if let Query::Alias { source, name } = query {
            let found = crate::operator::alias_source(source).and_then(|s| {
                self.ledger
                    .alias(&caller, &s, name.as_bytes(), now)
                    .map(|a| {
                        QueryReply::Alias(crate::wire::AliasBody {
                            account: a.to_did(),
                        })
                    })
            });
            return found.unwrap_or_else(refused);
        }
        if let Query::Grants { account } = query {
            let found = q::account(account).and_then(|a| {
                self.ledger.grants_on(&caller, &a, now).map(|gs| {
                    QueryReply::Grants(crate::wire::GrantsBody {
                        account: a.to_did(),
                        items: gs.iter().map(command::grant_view_body).collect(),
                    })
                })
            });
            return found.unwrap_or_else(refused);
        }
        if let Query::Account { account } = query {
            let found = q::account(account)
                .and_then(|a| self.ledger.account_view(&caller, &a, now))
                .map(|a| QueryReply::Account(command::account_view_body(&a)));
            return found.unwrap_or_else(refused);
        }
        // The caller's grants over an identity (amendment 0026).
        if let Query::IdentityGrants { identity } = query {
            let found = q::identity(identity).and_then(|i| {
                self.ledger
                    .grants_over_identity(&caller, &i, now)
                    .map(|gs| {
                        QueryReply::IdentityGrants(crate::wire::IdentityGrantsBody {
                            identity: i.to_did(),
                            items: gs.iter().map(command::grant_view_body).collect(),
                        })
                    })
            });
            return found.unwrap_or_else(refused);
        }
        // The operational signals (`F-018`): counts and ages, the operator's.
        if matches!(query, Query::Signals) {
            return match self.ledger.signals(&caller, now) {
                Ok(s) => QueryReply::Signals(self.signals_body(s, now)),
                Err(error) => refused(error),
            };
        }
        // Whether the caller may now put the writer's code in hand (`T-037-04`): the authority
        // function the matching input is decided with, asked at the door.
        if let Query::CodeAuthority { input } = query {
            let input = match input.as_str() {
                "upgrade" => ledger_core::ledger::operator::CodeInput::Upgrade,
                "way_out" => ledger_core::ledger::operator::CodeInput::WayOut,
                _ => return refused(Refusal::MalformedCommand),
            };
            return QueryReply::CodeAuthority {
                authorised: self.ledger.may_change_code(&caller, input),
            };
        }
        // The adapter's read of anchoring, and anchoring's other reads (`F-017`).
        let clerk = self.clerk.as_deref().map(|c| c as &dyn ClerkSigner);
        match query {
            Query::Anchor => return crate::settlement::anchor(&self.ledger, &caller),
            Query::VerifyDisclosure { disclosure } => {
                return crate::anchoring::verify(&self.ledger, &caller, disclosure, clerk);
            }
            Query::Anchoring => return crate::anchoring::anchoring(&self.ledger, &caller, now),
            _ => {}
        }
        let state = self.ledger.state();
        let p = match resolve(state, &caller) {
            Ok(p) => p,
            Err(r) => return refused(r),
        };
        // Every query but the outbox's is `F-011`'s, over one consistent view (`REQ-QR-2`).
        let answer = || -> Result<QueryReply, Refusal> {
            let view = self.ledger.view(now)?;
            Ok(match query {
                Query::Balances(q) => QueryReply::Balances(q::balances(&view.balances(
                    &caller,
                    q::subject(q.account.as_deref(), q.identity.as_deref())?,
                    q::asset_id(q.asset.as_deref())?,
                )?)),
                Query::Movements(q) => QueryReply::Page(q::page(
                    &view.movements(&caller, &q::movements_request(q)?)?,
                )),
                Query::Statement(q) => QueryReply::Statement(q::statement(
                    &view.statement(&caller, &q::statement_request(q)?)?,
                )),
                Query::Operation { reference } => QueryReply::Operation(q::operation(
                    &view.operation(&caller, &q::reference(reference)?)?,
                )),
                Query::Assets => QueryReply::Assets {
                    items: view.assets(&caller)?.iter().map(q::asset).collect(),
                },
                Query::TrialBalance(a) => QueryReply::TrialBalance(q::trial(
                    &view.trial_balance(&caller, q::asset_id(a.asset.as_deref())?)?,
                )),
                Query::CanSpend { account, params } => {
                    let account = q::account(account)?;
                    let asset =
                        q::asset_id(params.asset.as_deref())?.ok_or(Refusal::QueryUnbounded)?;
                    let amount =
                        q::minor(params.amount.as_deref().ok_or(Refusal::QueryUnbounded)?)?;
                    view.can_spend(&caller, &account, &asset, amount)?;
                    QueryReply::CanSpend(q::CanSpendBody { ok: true })
                }
                Query::Undelivered
                | Query::Instructions
                | Query::Calls
                | Query::Evidence
                | Query::Anchor
                | Query::VerifyDisclosure { .. }
                | Query::Anchoring
                | Query::Signals
                | Query::IntegratingParties
                | Query::Alias { .. }
                | Query::Grants { .. }
                | Query::CodeAuthority { .. }
                | Query::Account { .. } => return Err(Refusal::NotAuthorised),
                Query::IdentityGrants { .. } => return Err(Refusal::NotAuthorised),
                Query::RecoveryTerms { identity } => {
                    let owner = command::identity_id(identity)?;
                    QueryReply::RecoveryMechanisms(crate::custody::mechanisms_body(
                        &self.ledger.recovery_terms(&caller, &owner)?,
                    ))
                }
                Query::RecoveryMechanisms { account } => {
                    let account = q::account(account)?;
                    QueryReply::RecoveryMechanisms(crate::custody::mechanisms_body(
                        &self.ledger.recovery_mechanisms(&caller, &account, now)?,
                    ))
                }
                Query::Custody { account } => {
                    let account = q::account(account)?;
                    QueryReply::Custody(crate::custody::view_body(
                        &self.ledger.custody_view(&caller, &account, now)?,
                    ))
                }
            })
        };
        if !matches!(query, Query::Undelivered) {
            return answer().unwrap_or_else(refused);
        }
        // The outbox's undelivered notifications, with their age: the operator's alone.
        let operator = state
            .roles()
            .is_some_and(|r| r.operator == p.identity && p.unscoped());
        if !operator {
            return refused(Refusal::NotAuthorised);
        }
        let anchors = state.anchoring();
        let items = state
            .operations()
            .outbox()
            .filter(|n| !self.delivered.contains_key(&n.id))
            .map(|n| UndeliveredBody {
                notification: notification_body(n, anchors),
                recipient: self.recipient(&n.op).map(|r| r.to_did()),
                age: now.saturating_sub(self.created_at(n)),
            })
            .collect();
        QueryReply::Undelivered { items }
    }
}

impl<S, V> Core for InProcessCore<S, V>
where
    S: LeafStore + Send + Sync + 'static,
    V: VaultBackend + Send + Sync + 'static,
{
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        let core = self.clone();
        let input = input.clone();
        tokio::task::spawn_blocking(move || core.apply_now(&input))
            .await
            .unwrap_or(Err(CoreError::Ambiguous))
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        let core = self.clone();
        let batch = inputs.to_vec();
        let n = inputs.len();
        tokio::task::spawn_blocking(move || core.apply_batch_now(&batch))
            .await
            .unwrap_or_else(|_| (0..n).map(|_| Err(CoreError::Ambiguous)).collect())
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        _now: Time,
    ) -> Result<OutboxPage, CoreError> {
        let cursor = cursor.map(str::to_owned);
        self.read_blocking(move |r| r.outbox(cursor.as_deref(), undelivered))
            .await
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        self.read_blocking(|r| r.ledger.state().operations().clock())
            .await
    }

    async fn next_due(&self) -> Result<Option<Time>, CoreError> {
        self.read_blocking(|r| r.ledger.next_due()).await
    }

    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        let caller = caller.clone();
        let query = query.clone();
        let mut reply = self
            .read_blocking(move |r| r.query(&caller, &query, now))
            .await?;
        if let (QueryReply::Signals(s), Some(a)) = (&mut reply, self.audit()) {
            s.audit = Some(a.stats.body(now));
        }
        Ok(reply)
    }

    async fn credential(
        &self,
        credential: &str,
    ) -> Result<Option<crate::boundary::CredentialBody>, CoreError> {
        let credential = credential.to_owned();
        self.read_blocking(move |r| r.credential(&credential)).await
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        let principal = principal.to_owned();
        self.read_blocking(move |r| r.party_of(&principal)).await
    }
}

/// The in-process core's side of shipping (`T-013-13`): what it committed, read through the gate;
/// an acknowledgement, under the core's lock. Gone once the core is dropped.
struct InProcessOutbox<S: LeafStore, V: VaultBackend>(Weak<Parts<S, V>>);

impl<S, V> AuditOutbox for InProcessOutbox<S, V>
where
    S: LeafStore + Send + Sync + 'static,
    V: VaultBackend + Send + Sync + 'static,
{
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
        let _ctl = lock(&parts.ctl);
        let mut slot = parts.running.write();
        let Some(running) = slot.as_mut() else {
            return Some(Ok(()));
        };
        let done = running.ledger.acknowledge_audit(head).map(|_| ());
        if let Some((a, _)) = parts.audit.get() {
            let (n, oldest) = running.audit_outbox();
            a.stats.observe(n, oldest);
        }
        Some(done)
    }
}

/// The operator's list of integrating parties, on the wire (`REQ-ID-7`).
pub fn parties_body(list: &[ledger_core::party::PartyView]) -> crate::wire::IntegratingPartiesBody {
    crate::wire::IntegratingPartiesBody {
        items: list
            .iter()
            .map(|v| crate::wire::IntegratingPartyBody {
                party: v.party.party.to_did(),
                registered_at: v.party.registered_at,
                principals: v
                    .principals
                    .iter()
                    .map(|t| crate::wire::PartyPrincipalBody {
                        principal: t.principal.to_did(),
                        tied_at: t.tied_at,
                    })
                    .collect(),
            })
            .collect(),
    }
}
