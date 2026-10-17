// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The host-side driver (`T-005-08`, `INV-18`, `REQ-RT-2`, `REQ-CS-7`).
//!
//! For each input, as `F-005` §"One command, end to end" plans it:
//!
//! 1. The host keeps a **replica**: the library over `F-006`'s leaf store and openings store,
//!    wrapped so that it records every leaf it loads ([`RecStore`]), the value every vault row had
//!    before this input touched it ([`RecVault`]), and every library call it makes
//!    (`ledger_core::call`). Before each input the driver pins a read view of the leaf store.
//! 2. The input runs natively on the replica. That finds its exact read set, and its calls.
//! 3. The driver builds the witness of that read set against the pinned view (the Agent's roots),
//!    and the private inputs: the key-ring root and the openings of every account read. The
//!    library's service store records its own read set the same way (`ledger_core::leaves`), and
//!    the driver witnesses those leaves from its copy of the Agent's service tree
//!    ([`ServiceStore`]).
//! 4. It invokes the Agent through an [`AgentPort`]. The Agent verifies the witness and replays
//!    the calls over its own state, and commits.
//! 5. The driver checks the Agent's reply against the native run: every call's summary, both
//!    roots, the next posting number, and the delta-ring entry's log hash. Any difference is a
//!    [`DriveError::Diverged`]: the replica is not the Agent's book, and the host stops. Then it
//!    applies the input's service leaves to its service tree, whose root must be the Agent's.
//!
//! **Unknown outcomes.** A port that cannot say whether the Agent ran (an HTTP 503 or 504 from a
//! VOS ingress, `notes/FIT-VOS-CIPHERCLERK.md`) returns [`PortError::Ambiguous`]. The driver re-sends the same
//! envelope, under the same input number: an Agent that committed it answers
//! [`Reply::Duplicate`] with the entry's log hash, which the driver checks against its own; one
//! that did not runs it now. Either way the input changes the book exactly once (`INV-18`).
//!
//! **Batches** (`T-031-03`, `REQ-CS-7`). Steps 1 to 3 are [`Driver::stage`]: it builds one input's
//! envelope and blobs, applies its service leaves to the host's tree and pins the next view at
//! once, as if the Agent had committed it, and keeps it. Steps 4 and 5 are [`Driver::flush`]: every
//! staged input in one invocation ([`Envelope::Input`] for one, [`Envelope::Batch`] for more), and
//! each reply checked as above. The next input's witness is built against the state after the
//! staged ones, exactly as if each had been sent alone. `stage` flushes first when the new input
//! would take the invocation over a bound of [`BatchLimits`]; [`Driver::sync`] is a stage and a
//! flush, one input per invocation. A refused or ambiguous flush leaves the staged state ahead of
//! the Agent's, and the host rebuilds ([`Driver::recover`]), as it does after a refused input.

use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet};
use std::string::{String, ToString};
use std::sync::{Mutex, PoisonError};
use std::vec::Vec;
use std::{format, vec};

use cipher_clerk::AccountId;
use cipher_clerk::crypto::blake2b_256;
use leaf_store::engine::RedbSnapshot;
use leaf_store::engine::{Durability, RedbEngine};
use leaf_store::store::ReadView;
use leaf_store::trie::TreeId;
use leaf_store::witness::{batch_proof, keys_of};
use ledger_kernel::compact_smt::Version;

/// One kernel tree's multiproof of `keys` from `view`, in the build's root version
/// (`DEC-25`): opaque node entries, since a kernel tree never loses a leaf.
pub fn kernel_proof<S: leaf_store::engine::Snapshot>(
    view: &leaf_store::store::ReadView<S>,
    tree: TreeId,
    keys: &[[u8; 16]],
) -> Result<crate::witness::KernelProof, StoreError> {
    Ok(match Version::ACTIVE {
        Version::V1 => crate::witness::KernelProof::V1(batch_proof(&view.frontier(tree, keys)?)),
        Version::V2 => {
            crate::witness::KernelProof::V2((&view.multiproof(tree, keys, false)?).into())
        }
    })
}
use leaf_store::{Archive, ArchiveConfig, BookStore, OpeningsStore, StoreConfig};
use ledger_core::Ledger;
use ledger_core::account::Time;
use ledger_core::anchor::clerk::{ClerkSigner, InProcessClerk};
use ledger_core::call::{Call, Recorded, Summary};
use ledger_core::delta::{
    ServiceDelta, ServiceKind, library_id, library_key, library_leaf, library_parts,
};
use ledger_core::export::canon;
use ledger_core::id::OperationId;
use ledger_core::leaves::kind;
use ledger_core::ledger::{Founded, Genesis};
use ledger_core::lifecycle::keys::NotificationId;
use ledger_core::state::{LeafEntry, State};
use ledger_kernel::book::Book;
use ledger_kernel::keys::{Answers, Custody, KeyRequest, KeyRing, Recording, RootKeys};
use ledger_kernel::store::{BookDelta, KeySet, LeafStore, Loaded, LogRecord, StoreError, SubRoots};
use ledger_kernel::vault::{LayerSums, RowKey, Undo, VaultBackend, VaultError, VaultImage};
use std::sync::Arc;

use crate::envelope::{
    AgentRefusal, BatchEnvelope, BlobKey, ClerkAnswers, Committed, Envelope, FoundingCode,
    GenesisEnvelope, GenesisState, InputEnvelope, MAX_BATCH_INPUTS, MAX_BATCH_PACKS, Pack, Private,
    Reply, Roots, StateGenesisEnvelope, decode, encode,
};
use crate::hot::{Hot, Limits, RingEntry};
use crate::mapped::{Mapped, MappedGenesis, MappedInput, ServiceKeys};
use crate::service::{Node, ServiceWitness, proven_keys};
use crate::transition::{Invocation, MemRows};
use crate::vault::PrivateVault;
use crate::witness::{KernelWitness, root_of};

/// A leaf store that records every key loaded through it (step 1). Behind a mutex, not a cell, so
/// the replica can answer reads from several threads at once (`T-031-12`).
pub struct RecStore<L: LeafStore> {
    inner: L,
    seen: Mutex<KeySet>,
}

impl<L: LeafStore> RecStore<L> {
    /// Record loads from `inner`.
    pub fn new(inner: L) -> Self {
        RecStore {
            inner,
            seen: Mutex::new(KeySet::default()),
        }
    }

    /// The wrapped store.
    pub fn inner(&self) -> &L {
        &self.inner
    }

    /// Every key loaded since the last take.
    pub fn take_keys(&self) -> KeySet {
        core::mem::take(&mut *self.seen.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl<L: LeafStore> LeafStore for RecStore<L> {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        self.inner.subroots()
    }
    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.accounts.extend(keys.accounts.iter().copied());
        seen.transfers.extend(keys.transfers.iter().copied());
        seen.journals.extend(keys.journals.iter().copied());
        seen.external_ids.extend(keys.external_ids.iter().copied());
        seen.voided.extend(keys.voided.iter().copied());
        seen.pending.extend(keys.pending.iter().copied());
        drop(seen);
        self.inner.load(keys)
    }
    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        self.inner.commit(seq, delta)
    }
    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.inner.last_committed()
    }
    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        self.inner.next_posting_seq()
    }
    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        self.inner.log_record(seq)
    }
}

/// A vault backend that remembers the value every row had before its first write since the last
/// take (step 1): the openings the Agent needs are the ones before the input.
pub struct RecVault<V: VaultBackend> {
    inner: V,
    before: Mutex<BTreeMap<RowKey, Option<LayerSums>>>,
}

impl<V: VaultBackend> RecVault<V> {
    /// Record writes to `inner`.
    pub fn new(inner: V) -> Self {
        RecVault {
            inner,
            before: Mutex::new(BTreeMap::new()),
        }
    }

    /// Each row written since the last take, with its value before.
    pub fn take_before(&self) -> BTreeMap<RowKey, Option<LayerSums>> {
        core::mem::take(&mut *self.before.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl<V: VaultBackend> VaultBackend for RecVault<V> {
    fn read(&self) -> Result<VaultImage, VaultError> {
        self.inner.read()
    }
    fn write(&mut self, rows: &[(RowKey, LayerSums)], undo: &Undo) -> Result<(), VaultError> {
        let mut before = self.before.lock().unwrap_or_else(PoisonError::into_inner);
        for (k, v) in &undo.rows {
            before.entry(*k).or_insert(*v);
        }
        drop(before);
        self.inner.write(rows, undo)
    }
    fn revert(&mut self, undo: &Undo) -> Result<(), VaultError> {
        self.inner.revert(undo)
    }
}

/// Where the host keeps the replica's stores (`T-031-14`, finding F6 of `F-032`): the book's
/// leaves with their history and archive, the openings, and the service tree. Each is a
/// projection of what the Agent committed, rebuilt from the host's log on every start and after
/// every refused input ([`Driver::recover`]), so none needs to survive a crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplicaStores {
    /// In memory: they grow with the book (about 10 KB a command in the stress suite's soak).
    Memory,
    /// In files under `dir`, each with at most `cache_bytes` cached in memory. Every build wipes
    /// `dir` first; nothing else may use it. Commits are not synced: the host's log is durable.
    Disk {
        /// The directory, the host's own.
        dir: std::path::PathBuf,
        /// The page cache of each of the three files, bytes.
        cache_bytes: usize,
    },
}

/// The page cache of each replica file by default (`ReplicaStores::Disk`): three files, so at most
/// three times this in memory, whatever the size of the book. Small: the core host waits on the
/// Agent's invocations, not on reads, and what the cache misses the operating system's page cache
/// usually holds. At 32 MiB a file, an hour of the stress suite's soak (0.1 × the reference hour)
/// never filled it, so the host still grew with the book for hours.
pub const REPLICA_CACHE_BYTES: usize = 8 * 1024 * 1024;

impl ReplicaStores {
    fn fresh(dir: &std::path::Path) -> Result<(), DriveError> {
        let io = |e: std::io::Error| DriveError::Replica(format!("{}: {e}", dir.display()));
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        std::fs::create_dir_all(dir).map_err(io)
    }

    /// Fresh, empty stores for the book and the openings.
    pub fn book(&self) -> Result<(BookStore, OpeningsStore), DriveError> {
        match self {
            ReplicaStores::Memory => Ok((
                BookStore::in_memory().map_err(rep)?,
                OpeningsStore::in_memory().map_err(rep)?,
            )),
            ReplicaStores::Disk { dir, cache_bytes } => {
                let book = dir.join("book");
                Self::fresh(&book)?;
                let archive = Archive::open(
                    book.join("archive"),
                    ArchiveConfig {
                        fsync: false,
                        ..ArchiveConfig::default()
                    },
                )
                .map_err(rep)?;
                let store = BookStore::with_engine(
                    RedbEngine::open_cached(book.join("leaves.redb"), *cache_bytes).map_err(rep)?,
                    Some(archive),
                    StoreConfig {
                        durability: Durability::Visible,
                        verify_reads: true,
                    },
                )
                .map_err(rep)?;
                let vault = OpeningsStore::open_cached(book.join("openings.redb"), *cache_bytes)
                    .map_err(rep)?;
                Ok((store, vault))
            }
        }
    }

    /// A fresh, empty store for the service tree: a projection with no archive.
    pub fn service(&self) -> Result<BookStore, DriveError> {
        match self {
            ReplicaStores::Memory => BookStore::projection_in_memory().map_err(rep),
            ReplicaStores::Disk { dir, cache_bytes } => {
                let service = dir.join("service");
                Self::fresh(&service)?;
                BookStore::with_engine(
                    RedbEngine::open_cached(service.join("leaves.redb"), *cache_bytes)
                        .map_err(rep)?,
                    None,
                    StoreConfig {
                        durability: Durability::Visible,
                        verify_reads: true,
                    },
                )
                .map_err(rep)
            }
        }
    }
}

/// The host's leaf store, recording.
pub type HostStore = RecStore<BookStore>;
/// The host's openings store, recording.
pub type HostVault = RecVault<OpeningsStore>;
/// The replica: the library natively, over `F-006`'s stores (step 1).
pub type Replica = Ledger<HostStore, HostVault>;

/// Why a port could not answer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PortError {
    /// The Agent certainly did not run the invocation.
    Unavailable(String),
    /// The Agent may have run it (module documentation).
    Ambiguous(String),
    /// The runtime refused or aborted the invocation (the actor panicked or ran out of gas):
    /// nothing committed.
    Aborted(String),
}

/// How the driver reaches the Agent: VOS's runtime ABI (`agents/core-host`), the same transition
/// natively ([`NativePort`]), or a remote host.
pub trait AgentPort {
    /// Run one invocation: `message`, with `blobs` as caller availability.
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError>;
    /// The content address the port's runtime gives `bytes`.
    fn blob_key(&self, bytes: &[u8]) -> BlobKey;
    /// The Agent's hot row, read without a transition (`None` before genesis). A restarted host
    /// reads it to learn where the Agent stands (`T-005-08`, `T-005-09`).
    fn hot(&mut self) -> Result<Option<Hot>, PortError>;
    /// One delta-ring entry, read without a transition (`None` once trimmed).
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError>;
    /// The runtime's Authority decisions (receipts) issued for invocations not yet acknowledged
    /// (`NFR-11`, `F-018`'s `AledgentVosAuthorityBacklog`); `None` where the port has no runtime
    /// that counts them.
    fn unacknowledged(&self) -> Option<u64> {
        None
    }
}

/// Why the driver stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveError {
    /// The Agent committed nothing and said why. The replica ran the input: it must be rebuilt
    /// from the committed inputs before the next one.
    Refused(AgentRefusal),
    /// The port failed and a retry did not settle it.
    Port(PortError),
    /// The Agent's transition is not the replica's: an incident. The host must stop.
    Diverged(String),
    /// The input made a call that cannot run inside the Agent.
    Unsupported(String),
    /// The replica itself failed.
    Replica(String),
}

/// How many times the driver re-sends an envelope after an ambiguous failure.
pub const RETRIES: usize = 3;

/// The bounds of one invocation the driver builds (`T-031-03`): how many inputs it batches, and
/// the sizes VOS allows one invocation, less a margin. [`Driver::stage`] flushes before an input
/// that would take the staged batch over any of them; an input alone is always sent, whatever its
/// size, as before batching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchLimits {
    /// The most inputs in one invocation. 1 is one input per invocation (before `T-031-03`).
    pub max_inputs: usize,
    /// The encoded envelope: VOS's 16 KiB message limit (`MAX_INVOCATION_MESSAGE_BYTES`), less
    /// 1 KiB for the runtime's own framing of the message.
    pub max_message_bytes: usize,
    /// The reply, estimated from the summaries each input will commit with: VOS keeps an
    /// application actor's reply to 8 KiB (`MAX_EXECUTION_REPLY_BYTES`, under the protocol's
    /// 16 KiB `MAX_INVOCATION_REPLY_BYTES`), less 1 KiB for a refusal's text and the value's
    /// framing.
    pub max_reply_bytes: usize,
    /// Every pack together: VOS's 192 KiB of caller blobs per invocation
    /// (`MAX_RUNTIME_CALLER_AVAILABILITY_BYTES`), less 8 KiB.
    pub max_blob_bytes: usize,
    /// One pack ([`crate::envelope::Pack`]): the guest holds one at a time in its 256 KiB heap,
    /// beside the input it is running, the message and the batch's buffered writes; each input's
    /// blobs once, not the pack's bytes as well (`crate::transition`). A quarter of
    /// `max_blob_bytes`, so that [`MAX_BATCH_PACKS`] packs use all of it. At a full-size book's
    /// depth a seed chunk's witness fills a pack, and four of them took about 195 KB of the
    /// guest's arena at worst (`tests/guest_heap.rs`, the guest allocator's placement modelled).
    pub max_pack_bytes: usize,
    /// The most packs one invocation may carry: [`MAX_BATCH_PACKS`], all of VOS's caller blobs,
    /// unless the host keeps one for itself (`F-033`: a composite invocation's own blob).
    pub max_packs: usize,
    /// The actor gas one invocation may use. In VOS it is `MAX_EXECUTION_GAS`
    /// (`vos/src/agent/execution.rs`; 1 000 M at the pinned VOS, [`PINNED_VOS_ACTOR_GAS`]): past
    /// it the invocation runs out of gas and nothing commits. The defaults set no bound, as a
    /// native port has none; the host that links VOS sets the constant it links
    /// (`agents/core-host`, [`BatchLimits::with_actor_gas`]).
    pub max_actor_gas: u64,
    /// The actor gas one input is assumed to take: the batch holds at most
    /// `max_actor_gas / input_gas` inputs. 330 M is above every ordinary command's p99
    /// (`notes/MEASUREMENTS-F005.md` §8.2: 291 M to 329 M); a heavier input can still run a batch
    /// out of gas, and then nothing of it commits and the host re-drives its inputs one at a time
    /// (`crate::core`).
    pub input_gas: u64,
}

impl BatchLimits {
    /// The defaults: up to 32 inputs, within the bounds above, with no gas bound. With about
    /// 9 KB of blobs per ordinary input (`notes/MEASUREMENTS-F005.md` §8.2) the packs bind first,
    /// at about twenty. In VOS at the pinned version the actor gas binds before them, at three
    /// ordinary inputs (1 000 M over 330 M).
    pub const DEFAULT: BatchLimits = BatchLimits {
        max_inputs: 32,
        max_message_bytes: 15 * 1024,
        max_reply_bytes: 7 * 1024,
        max_blob_bytes: 184 * 1024,
        max_pack_bytes: 46 * 1024,
        max_packs: MAX_BATCH_PACKS,
        max_actor_gas: u64::MAX,
        input_gas: 330_000_000,
    };

    /// One input per invocation.
    pub const SINGLE: BatchLimits = BatchLimits {
        max_inputs: 1,
        ..BatchLimits::DEFAULT
    };

    /// The defaults, with at most `n` inputs (at least one).
    pub const fn with_inputs(n: usize) -> BatchLimits {
        BatchLimits {
            max_inputs: if n == 0 { 1 } else { n },
            ..BatchLimits::DEFAULT
        }
    }

    /// These bounds, with the runtime's actor gas per invocation.
    pub const fn with_actor_gas(self, gas: u64) -> BatchLimits {
        BatchLimits {
            max_actor_gas: gas,
            ..self
        }
    }
}

/// VOS's actor gas per invocation at the pinned version (`MAX_EXECUTION_GAS`): what
/// [`BatchLimits::max_actor_gas`] is in the VOS host unless VOS is changed.
pub const PINNED_VOS_ACTOR_GAS: u64 = 1_000_000_000;

impl Default for BatchLimits {
    fn default() -> Self {
        BatchLimits::DEFAULT
    }
}

/// One staged input: its envelope and blobs, and what its reply must say.
struct Staged {
    input: InputEnvelope,
    /// The kernel witness, the service witness and the private inputs, in that order.
    blobs: [Vec<u8>; 3],
    summaries: Vec<Summary>,
    entry: RingEntry,
    log_hash: [u8; 32],
    /// The envelope's encoded size.
    message_bytes: usize,
    /// The largest its reply can be.
    reply_bytes: usize,
}

impl Staged {
    fn blob_bytes(&self) -> usize {
        self.blobs.iter().map(Vec::len).sum()
    }

    /// Its blobs' size in a pack: each with its key (32 B), its key's length and its own
    /// length (at most ten bytes each, as varints).
    fn packed_bytes(&self) -> usize {
        self.blob_bytes() + 3 * (32 + 10 + 10)
    }
}

/// How staged inputs fall into packs: consecutive runs, each within `max_pack_bytes` unless one
/// input alone is larger. Returns each run's length.
fn pack_runs<'a>(
    staged: impl IntoIterator<Item = &'a Staged>,
    max_pack_bytes: usize,
) -> Vec<usize> {
    let mut runs: Vec<usize> = Vec::new();
    let mut bytes = 0usize;
    for s in staged {
        let b = s.packed_bytes();
        match runs.last_mut() {
            Some(n) if bytes + b <= max_pack_bytes => {
                *n += 1;
                bytes += b;
            }
            _ => {
                runs.push(1);
                bytes = 16 + b;
            }
        }
    }
    runs
}

/// The varint length of `n` in postcard.
fn varint_len(n: usize) -> usize {
    let mut n = n as u64;
    let mut len = 1;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }
    len
}

/// The driver (module documentation).
pub struct Driver<P: AgentPort> {
    port: P,
    keys: HostKeys,
    /// What the replica asked the custody and the clerk since the last input was staged.
    recorder: Recorder,
    view: Option<ReadView<RedbSnapshot>>,
    service: ServiceStore,
    seq: u64,
    log_head: [u8; 32],
    archived: Option<(u64, [u8; 32])>,
    /// The Agent's ring: no batch is larger.
    ring: u64,
    limits: BatchLimits,
    staged: Vec<Staged>,
    /// Inputs flushed by [`Driver::stage`] and not yet returned by [`Driver::flush`].
    done: Vec<Committed>,
    /// The entries of inputs the Agent confirmed since the host last archived.
    entries: Vec<RingEntry>,
    /// The delta-ring entry of the last committed input, as the replica computed it and the Agent
    /// confirmed it: what the host archives before it acknowledges (`T-005-07`).
    pub last_entry: Option<RingEntry>,
    /// Whether the Agent keeps the book in its committed maps (layout 3, `F-031` lever 2): its
    /// inputs carry the service keys they touch, and no witness.
    mapped: bool,
    /// The roots of the book's genesis, as this host's replica computed them: from the chainspec
    /// for a chainspec book (`F-036`), which the core checks against the deployment's manifest.
    genesis: Roots,
    /// The code version that executes the Agent's next input, as its hot row holds it
    /// (`T-037-02`): each ring entry the replica computes names it.
    code: [u8; 32],
    /// The digest of the Agent's upgrade-authority mode, as its hot row holds it: with
    /// [`Driver::code`], what the replica's checkpoints bind (`T-037-03`).
    authority: [u8; 32],
}

/// A clerk shared between threads: what the executor signs disclosures with.
pub type SharedClerk = Arc<dyn ClerkSigner + Send + Sync>;

/// The keys the host runs the book over (`F-035`, guardrail 13, `NFR-8`): a key custody that
/// answers for the key-ring root, and the clerk, if disclosures are issued. In production both
/// are the signer service's clients (`services/signer`), so no key is ever in this process;
/// [`HostKeys::local`] holds them here, for tests and development.
#[derive(Clone)]
pub struct HostKeys {
    /// Answers for the key-ring root.
    pub custody: Arc<dyn Custody>,
    /// Signs disclosures.
    pub clerk: Option<SharedClerk>,
}

impl core::fmt::Debug for HostKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostKeys")
            .field("clerk", &self.clerk.as_ref().map(|c| c.public_key()))
            .finish_non_exhaustive()
    }
}

impl HostKeys {
    /// Keys answered by `custody` and `clerk`.
    pub fn new(custody: Arc<dyn Custody>, clerk: Option<SharedClerk>) -> Self {
        HostKeys { custody, clerk }
    }

    /// The root and the clerk key held in this process: tests and development only. The book's
    /// production process never calls it (`ledger_kernel::keys::local_roots_loaded`).
    pub fn local(root: [u8; 32], clerk: Option<[u8; 32]>) -> Self {
        HostKeys {
            custody: Arc::new(RootKeys::new(root)),
            clerk: clerk
                .and_then(InProcessClerk::from_secret)
                .map(|c| Arc::new(c) as SharedClerk),
        }
    }
}

/// A clerk that keeps every signature it gives, for the Agent's private input.
struct RecordingClerk {
    inner: SharedClerk,
    log: Mutex<BTreeMap<Vec<u8>, [u8; 64]>>,
}

impl ClerkSigner for RecordingClerk {
    fn public_key(&self) -> [u8; 32] {
        self.inner.public_key()
    }

    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let sig = self.inner.sign(msg);
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(msg.to_vec(), sig);
        sig
    }
}

/// The custody's answers for one input, encoded (`Private::keys`).
type KeyAnswers = Vec<(Vec<u8>, Vec<u8>)>;

/// What the replica asked the custody and the clerk, taken with each input (`F-035`).
struct Recorder {
    keys: Arc<Mutex<Answers>>,
    clerk: Option<Arc<RecordingClerk>>,
    /// The registrar's key: the Agent opens its book with it on every invocation, and the
    /// replica asks for it only once.
    registrar: (KeyRequest, ledger_kernel::keys::KeyAnswer),
}

impl Recorder {
    /// A recorder over `keys`, and the key ring the replica runs over.
    fn new(keys: &HostKeys) -> (Recorder, KeyRing) {
        let (rec, log) = Recording::new(Arc::clone(&keys.custody));
        let registrar = (
            KeyRequest::RegistrarKey,
            keys.custody.answer(&KeyRequest::RegistrarKey),
        );
        let clerk = keys.clerk.as_ref().map(|c| {
            Arc::new(RecordingClerk {
                inner: Arc::clone(c),
                log: Mutex::new(BTreeMap::new()),
            })
        });
        (
            Recorder {
                keys: log,
                clerk,
                registrar,
            },
            KeyRing::with_custody(Box::new(rec)),
        )
    }

    /// The clerk the executor signs with.
    fn clerk(&self) -> Option<SharedClerk> {
        self.clerk.as_ref().map(|c| Arc::clone(c) as SharedClerk)
    }

    /// Forget what was recorded (after a replay, which asks for every past input).
    fn clear(&self) {
        let _ = self.take();
    }

    /// What was recorded since the last take, as the Agent's private keys and clerk.
    fn take(&self) -> (KeyAnswers, Option<ClerkAnswers>) {
        let mut answers =
            core::mem::take(&mut *self.keys.lock().unwrap_or_else(PoisonError::into_inner));
        answers.insert(&self.registrar.0, &self.registrar.1);
        let clerk = self.clerk.as_ref().map(|c| {
            let signatures =
                core::mem::take(&mut *c.log.lock().unwrap_or_else(PoisonError::into_inner));
            ClerkAnswers {
                public: c.public_key(),
                signatures: signatures
                    .into_iter()
                    .map(|(m, s)| (m, s.to_vec()))
                    .collect(),
            }
        });
        (answers.into_entries(), clerk)
    }
}
/// The native run of one input, compared with the Agent's: the replica's kernel and audit chain,
/// and the host's service tree once the input's service leaves are applied.
fn replica_roots(replica: &Replica, service: &ServiceStore) -> Result<(Roots, u64), DriveError> {
    let store = replica.book().store();
    let sub = store
        .subroots()
        .map_err(|e| DriveError::Replica(format!("{e:?}")))?;
    let next = store
        .next_posting_seq()
        .map_err(|e| DriveError::Replica(format!("{e:?}")))?;
    let audit = replica.state().chain_head();
    Ok((
        Roots {
            kernel: root_of(&sub),
            service: service.root(),
            audit: crate::envelope::audit_digest(audit.emitted, &audit.head),
        },
        next,
    ))
}

/// What the Agent's hot row holds for the replica's state as it stands (`F-036`).
fn genesis_state_of(replica: &Replica, service: &ServiceStore) -> Result<GenesisState, DriveError> {
    let store = replica.book().store();
    let audit = replica.state().chain_head();
    Ok(GenesisState {
        subroots: store.subroots().map_err(rep)?,
        last_commit: store.last_committed().map_err(rep)?,
        next_posting: store.next_posting_seq().map_err(rep)?,
        service_root: service.root(),
        audit_emitted: audit.emitted,
        audit_head: audit.head,
        audit_shipped: audit.shipped,
        notified: replica.state().operations().outbox().count() as u64,
    })
}

/// Apply chunks `0..upto` of `spec` to `replica` natively, as the operator, through the same
/// `apply_spec_chunk` and with the same context as the native genesis
/// (`Ledger::genesis` with `Genesis::spec`): the start of a segment ([`Driver::found_at_chunk`]).
fn apply_spec_prefix(
    replica: &mut Replica,
    founded: &Founded,
    spec: &ledger_core::chainspec::ChainSpec,
    upto: u32,
) -> Result<(), DriveError> {
    use ledger_core::ledger::Ctx;
    use ledger_core::principal::Caller;
    let ctx = Ctx {
        caller: Caller::new(founded.operator, founded.operator_credential),
        op: spec.header().genesis_op,
        now: spec.header().at,
    };
    // `try_for_each_chunk` streams the chunks; the first one past the prefix stops it.
    let r = spec.try_for_each_chunk(|i, bytes, path| {
        if i >= upto {
            return Err(None);
        }
        replica
            .apply_spec_chunk(&ctx, bytes, path)
            .map(|_| ())
            .map_err(Some)
    });
    match r {
        Ok(()) | Err(None) => Ok(()),
        Err(Some(refusal)) => Err(DriveError::Replica(format!(
            "the segment's prefix: a chunk is refused natively: {refusal:?}"
        ))),
    }
}

/// A chainspec book's genesis state, built natively (`F-036`, owner ruling `genesis-chainspec`):
/// what `aledgent-core-host build-spec` writes, and what every core host rebuilds on start.
pub struct BuiltGenesis {
    /// The replica at genesis.
    pub replica: Replica,
    /// Its service tree.
    pub service: ServiceStore,
    /// The roots.
    pub roots: Roots,
    /// The Agent's hot state at genesis.
    pub state: GenesisState,
}

/// Build the genesis state of `genesis` (which carries the chainspec in `Genesis::spec`) in
/// `stores`, with `keys`: the same native genesis every core host runs before its first input.
pub fn build_genesis(
    stores: &ReplicaStores,
    keys: &HostKeys,
    genesis: Genesis,
    op: OperationId,
    at: Time,
) -> Result<BuiltGenesis, DriveError> {
    if genesis.spec.is_none() {
        return Err(DriveError::Replica("no chainspec to build".into()));
    }
    let (replica, service, _, roots) = replay_with(stores, keys, genesis, op, at, &[], None, None)?;
    let state = genesis_state_of(&replica, &service)?;
    Ok(BuiltGenesis {
        replica,
        service,
        roots,
        state,
    })
}

/// How many leaves one commit of [`ServiceStore::of_in`] writes.
const SERVICE_SLICE: usize = 65_536;

/// The host's copy of the Agent's service tree (`F-005`, `F-006` decision 5): `F-006`'s leaf
/// store, holding the service tree alone. Every collection of the replica's service store is
/// leaves of it (`ledger_core::leaves`), and so is every delivery the Agent recorded. It changes
/// once per committed input, by the leaves that input changed; the witness of the next input is
/// read from it, so it always stands where the Agent does.
pub struct ServiceStore {
    store: BookStore,
}

fn rep(e: impl core::fmt::Debug) -> DriveError {
    DriveError::Replica(format!("{e:?}"))
}

impl ServiceStore {
    /// An empty tree, in memory.
    pub fn new() -> Result<Self, DriveError> {
        ServiceStore::new_in(&ReplicaStores::Memory)
    }

    /// An empty tree, where `stores` says.
    pub fn new_in(stores: &ReplicaStores) -> Result<Self, DriveError> {
        Ok(ServiceStore {
            store: stores.service()?,
        })
    }

    /// The tree of a whole store: every leaf of `state`.
    pub fn of(state: &State) -> Result<Self, DriveError> {
        ServiceStore::of_in(&ReplicaStores::Memory, state)
    }

    /// [`ServiceStore::of`], where `stores` says.
    pub fn of_in(stores: &ReplicaStores, state: &State) -> Result<Self, DriveError> {
        let mut s = ServiceStore::new_in(stores)?;
        // Streamed, in commits of at most `SERVICE_SLICE` leaves (`F-036`): a chainspec book's
        // genesis has millions, and neither their encodings nor one write transaction of all of
        // them is held at once.
        let mut slice: Vec<LeafEntry> = Vec::with_capacity(SERVICE_SLICE);
        let mut failed = None;
        state.for_each_leaf(&mut |k, key, value| {
            if failed.is_some() {
                return;
            }
            slice.push((k, key, Some(value)));
            if slice.len() == SERVICE_SLICE {
                if let Err(e) = s.apply(&slice) {
                    failed = Some(e);
                }
                slice.clear();
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        s.apply(&slice)?;
        Ok(s)
    }

    /// The service root.
    pub fn root(&self) -> [u8; 32] {
        self.store.roots().service
    }

    /// A read view.
    pub fn view(&self) -> Result<ReadView<RedbSnapshot>, DriveError> {
        self.store.view().map_err(rep)
    }

    /// The value of every leaf of `touched` as it stands, and of every leaf of each kind touched
    /// whole; and the kinds touched whole.
    fn leaves(&self, touched: &Touches) -> Result<(Vec<LeafEntry>, Vec<u8>), DriveError> {
        let view = self.view()?;
        let mut leaves = Vec::new();
        let mut whole = Vec::new();
        for (kind, (keys, all)) in &touched.0 {
            let mut keys = keys.clone();
            if *all {
                whole.push(*kind);
                let kind_of = ServiceKind::from_code(*kind).ok_or_else(|| rep("kind 0"))?;
                for leaf in view.service_leaves_of(kind_of).map_err(rep)? {
                    keys.insert(library_parts(&leaf).map_err(rep)?.0);
                }
            }
            for key in keys {
                let value = match view
                    .service_leaf_at(&library_key(*kind, &key))
                    .map_err(rep)?
                {
                    Some(leaf) => Some(library_parts(&leaf).map_err(rep)?.1),
                    None => None,
                };
                leaves.push((*kind, key, value));
            }
        }
        Ok((leaves, whole))
    }

    /// The witness of `touched` for the Agent (`crate::service`).
    fn witness(&self, touched: &Touches) -> Result<ServiceWitness, DriveError> {
        let (leaves, whole) = self.leaves(touched)?;
        let keys = proven_keys(&leaves, &whole);
        let (frontier, compact) = if Version::ACTIVE == Version::V2 {
            // The service tree removes leaves: node entries opened (`crate::service`).
            let proof = self
                .view()?
                .multiproof(TreeId::SERVICE, &keys, true)
                .map_err(rep)?;
            (Vec::new(), Some((&proof).into()))
        } else {
            let frontier = self
                .view()?
                .frontier(TreeId::SERVICE, &keys)
                .map_err(rep)?
                .into_iter()
                .map(|f| Node {
                    level: f.level,
                    prefix: f.prefix,
                    hash: f.hash,
                })
                .collect();
            (frontier, None)
        };
        Ok(ServiceWitness {
            leaves,
            whole,
            frontier,
            compact,
        })
    }

    /// Apply changed leaves: a value puts the leaf, none removes it.
    fn apply(&mut self, changes: &[LeafEntry]) -> Result<(), DriveError> {
        let mut delta = ServiceDelta::new();
        for (k, key, value) in changes {
            match value {
                Some(v) => delta.put(library_leaf(*k, key, v)),
                None => delta.remove(
                    ServiceKind::from_code(*k).ok_or_else(|| rep("kind 0"))?,
                    library_id(*k, key),
                ),
            }
            .map_err(rep)?;
        }
        if !delta.is_empty() {
            self.store.apply_service(&delta).map_err(rep)?;
        }
        Ok(())
    }
}

/// What one input touched of the service tree, by kind: the keys, and whether the kind was read
/// whole.
#[derive(Default)]
struct Touches(BTreeMap<u8, (BTreeSet<Vec<u8>>, bool)>);

impl Touches {
    /// What the replica recorded since the last take, and the leaves `deliveries` touch: each
    /// notification, and its delivery.
    fn take(replica: &Replica, deliveries: &[([u8; 32], Time)]) -> Touches {
        let mut t = Touches::default();
        for r in replica.state().take_touched() {
            let e = t.0.entry(r.kind).or_default();
            e.0.extend(r.keys);
            e.1 |= r.whole;
        }
        for (id, _) in deliveries {
            t.0.entry(kind::NOTIFICATION)
                .or_default()
                .0
                .insert(canon::to_bytes(&NotificationId(*id)));
            t.0.entry(kind::DELIVERED)
                .or_default()
                .0
                .insert(canon::to_bytes(id));
        }
        t
    }
}

/// The leaves of `before` whose value the input changed, with their value after: the replica's
/// service store, and each delivery recorded, as the Agent records it (a committed notification,
/// not recorded before).
fn changed(
    replica: &Replica,
    before: &[LeafEntry],
    deliveries: &[([u8; 32], Time)],
) -> Result<Vec<LeafEntry>, DriveError> {
    let mut delivered: BTreeMap<Vec<u8>, Option<Vec<u8>>> = before
        .iter()
        .filter(|(k, _, _)| *k == kind::DELIVERED)
        .map(|(_, key, v)| (key.clone(), v.clone()))
        .collect();
    for (id, at) in deliveries {
        let known = replica
            .state()
            .operations()
            .notification(&NotificationId(*id))
            .is_some();
        if let Some(slot) = delivered.get_mut(&canon::to_bytes(id))
            && known
            && slot.is_none()
        {
            *slot = Some(canon::to_bytes(at));
        }
    }
    let mut out = Vec::new();
    for (k, key, old) in before {
        let now = if *k == kind::DELIVERED {
            delivered.get(key).cloned().flatten()
        } else {
            replica.state().leaf_value(*k, key).map_err(rep)?
        };
        if now != *old {
            out.push((*k, key.clone(), now));
        }
    }
    Ok(out)
}

impl<P: AgentPort> Driver<P> {
    /// The clerk the replica's executor signs disclosures with: the host's, recorded for the
    /// Agent's private input (`F-035`).
    pub fn clerk(&self) -> Option<SharedClerk> {
        self.recorder.clerk()
    }

    /// The keys this driver runs over.
    pub fn keys(&self) -> &HostKeys {
        &self.keys
    }

    /// Found the book (module documentation): genesis on a fresh replica over in-memory stores,
    /// then on the Agent, and compare. `root` is the key-ring root (a secret: the caller wipes
    /// its copy); `clerk` the clerk key's secret, if disclosures are issued.
    #[allow(clippy::too_many_arguments)]
    pub fn found(
        port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        limits: Limits,
    ) -> Result<(Self, Replica, Founded), DriveError> {
        Self::found_in(&ReplicaStores::Memory, port, keys, genesis, op, at, limits)
    }

    /// [`Driver::found`], with the replica's stores where `stores` says (`T-031-14`).
    #[allow(clippy::too_many_arguments)]
    pub fn found_in(
        stores: &ReplicaStores,
        port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        limits: Limits,
    ) -> Result<(Self, Replica, Founded), DriveError> {
        let (store, vault) = stores.book()?;
        Self::found_with(
            stores, port, keys, store, vault, genesis, op, at, limits, false, None,
        )
    }

    /// Found a **segment** of the chunk-by-chunk path (`verify-genesis --segments`, `F-036`): a
    /// book founded on the chainspec's commitment alone (`genesis.spec` is `None`, as
    /// `verify-genesis` founds it), whose first `start` chunks the host applies natively, through
    /// the same `apply_spec_chunk` as the native genesis, before the Agent is founded on that
    /// state in one invocation (`StateGenesis`). The Agent then takes chunks `start..` as inputs,
    /// as an Agent founded at chunk 0 would.
    ///
    /// The state the Agent is founded on is the host's word, as every `StateGenesis` is: a
    /// segmented verification is only as strong as the single one because the verifier checks
    /// that this state is the one the segment before it reached **inside the PVM** (the hot row's
    /// genesis fields, [`Hot::genesis_state`]). `start == 0` is [`Driver::found_in`].
    #[allow(clippy::too_many_arguments)]
    pub fn found_at_chunk(
        stores: &ReplicaStores,
        port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        limits: Limits,
        spec: &ledger_core::chainspec::ChainSpec,
        start: u32,
    ) -> Result<(Self, Replica, Founded), DriveError> {
        if genesis.spec.is_some() || genesis.chainspec != Some(spec.commitment()) {
            return Err(DriveError::Replica(
                "a segment is founded on the chainspec's commitment alone".into(),
            ));
        }
        if start >= spec.chunk_count() {
            return Err(DriveError::Replica(format!(
                "a segment starting at chunk {start} of {}",
                spec.chunk_count()
            )));
        }
        let (store, vault) = stores.book()?;
        let prefix = (start > 0).then_some((spec, start));
        Self::found_with(
            stores, port, keys, store, vault, genesis, op, at, limits, false, prefix,
        )
    }

    /// [`Driver::found`], for an Agent that keeps the book in its committed maps (layout 3,
    /// `F-031` lever 2).
    #[allow(clippy::too_many_arguments)]
    pub fn found_mapped(
        port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        limits: Limits,
    ) -> Result<(Self, Replica, Founded), DriveError> {
        let store = BookStore::in_memory().map_err(|e| DriveError::Replica(format!("{e:?}")))?;
        let vault =
            OpeningsStore::in_memory().map_err(|e| DriveError::Replica(format!("{e:?}")))?;
        Self::found_with(
            &ReplicaStores::Memory,
            port,
            keys,
            store,
            vault,
            genesis,
            op,
            at,
            limits,
            true,
            None,
        )
    }

    /// [`Driver::found`] over the given (empty) stores.
    #[allow(clippy::too_many_arguments)]
    pub fn found_on(
        port: P,
        keys: HostKeys,
        store: BookStore,
        vault: OpeningsStore,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        limits: Limits,
    ) -> Result<(Self, Replica, Founded), DriveError> {
        Self::found_with(
            &ReplicaStores::Memory,
            port,
            keys,
            store,
            vault,
            genesis,
            op,
            at,
            limits,
            false,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn found_with(
        stores: &ReplicaStores,
        port: P,
        keys: HostKeys,
        store: BookStore,
        vault: OpeningsStore,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        limits: Limits,
        mapped: bool,
        prefix: Option<(&ledger_core::chainspec::ChainSpec, u32)>,
    ) -> Result<(Self, Replica, Founded), DriveError> {
        let rep = |e: &dyn core::fmt::Debug| DriveError::Replica(format!("{e:?}"));
        let code = FoundingCode::of(&genesis);
        let view = store.view().map_err(|e| rep(&e))?;
        let (recorder, ring) = Recorder::new(&keys);
        let book = Book::bootstrap(RecStore::new(store), RecVault::new(vault), ring)
            .map_err(|e| rep(&e))?;
        let (mut replica, founded) =
            Ledger::genesis(book, genesis.clone(), op, at).map_err(|e| rep(&e))?;
        if let Some((spec, start)) = prefix {
            apply_spec_prefix(&mut replica, &founded, spec, start)?;
        }
        replica.set_code(code.code, code.authority);
        replica.record_calls(true);
        let service = ServiceStore::of_in(stores, replica.state())?;
        let mut driver = Driver {
            port,
            keys,
            recorder,
            view: Some(view),
            service,
            seq: 0,
            log_head: [0; 32],
            archived: None,
            ring: limits.ring,
            limits: BatchLimits::DEFAULT,
            staged: Vec::new(),
            done: Vec::new(),
            entries: Vec::new(),
            last_entry: None,
            mapped,
            genesis: Roots::default(),
            code: code.code,
            authority: code.authority,
        };
        // Bootstrap writes the book's one journal without loading it: witness it absent.
        let mut keys = replica.book().store().take_keys();
        keys.journals.insert(replica.book().journal());
        replica.book().vault_backend_take();
        let (answers, _) = driver.recorder.take();
        let private = encode(&Private {
            keys: answers,
            clerk: None,
            openings: Vec::new(),
        });
        let state_genesis = genesis.spec.is_some() || prefix.is_some();
        if state_genesis {
            // The genesis state is built (`F-036`): what the Agent needs is its roots.
            let _ = replica.state().take_touched();
        }
        driver.genesis = replica_roots(&replica, &driver.service)?.0;
        let reply = if state_genesis && !mapped {
            let commitment = genesis
                .chainspec
                .ok_or_else(|| DriveError::Replica("a chainspec without its commitment".into()))?;
            let envelope = Envelope::StateGenesis(Box::new(StateGenesisEnvelope {
                chainspec: commitment,
                lifecycle: genesis.lifecycle.clone(),
                limits,
                state: genesis_state_of(&replica, &driver.service)?,
                code,
            }));
            driver.send(&envelope, Vec::new())?
        } else if mapped {
            let envelope = Envelope::Mapped(Mapped::Genesis(Box::new(MappedGenesis {
                genesis,
                op,
                at,
                limits,
                private: driver.port.blob_key(&private),
                code,
            })));
            driver.send(&envelope, vec![private])?
        } else {
            let witness = driver.witness(&keys)?;
            let envelope = Envelope::Genesis(Box::new(GenesisEnvelope {
                genesis,
                op,
                at,
                limits,
                witness: driver.port.blob_key(&witness),
                private: driver.port.blob_key(&private),
                code,
            }));
            driver.send(&envelope, vec![witness, private])?
        };
        let (roots, next) = replica_roots(&replica, &driver.service)?;
        match reply {
            Reply::Committed(c) if c.roots == roots && c.next_posting == next => {}
            Reply::Committed(c) => {
                return Err(DriveError::Diverged(format!(
                    "genesis: the Agent's roots {:?} are not the replica's {roots:?}",
                    c.roots
                )));
            }
            Reply::Refused(r) => return Err(DriveError::Refused(r)),
            other => return Err(DriveError::Diverged(format!("genesis: {other:?}"))),
        }
        driver.pin(&replica)?;
        Ok((driver, replica, founded))
    }

    /// The roots of the book's genesis as this host's replica computed them (`F-036`): for a
    /// chainspec book, from the chainspec.
    pub fn genesis_roots(&self) -> Roots {
        self.genesis
    }

    /// Give the port back: a host rebuilding its replica hands it to [`Driver::recover`].
    pub fn into_port(self) -> P {
        self.port
    }

    /// The port.
    pub fn port(&self) -> &P {
        &self.port
    }

    /// The port, mutably.
    pub fn port_mut(&mut self) -> &mut P {
        &mut self.port
    }

    /// The code version that executes the Agent's next input (`T-037-02`).
    pub fn code(&self) -> [u8; 32] {
        self.code
    }

    /// The last input number the Agent committed.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The log head: the last committed entry's log hash.
    pub fn log_head(&self) -> [u8; 32] {
        self.log_head
    }

    /// Say that the host archived everything up to the last committed input: the next envelope
    /// carries the acknowledgement and the Agent trims its ring (`T-005-07`).
    pub fn archived(&mut self) {
        if self.seq > 0 {
            self.archived = Some((self.seq, self.log_head));
        }
        self.entries.clear();
    }

    /// The batch bounds.
    pub fn batch_limits(&self) -> BatchLimits {
        self.limits
    }

    /// Set the batch bounds (`T-031-03`); [`BatchLimits::SINGLE`] is one input per invocation.
    pub fn set_batch_limits(&mut self, limits: BatchLimits) {
        self.limits = limits;
    }

    /// How many inputs are staged and not yet sent.
    pub fn staged(&self) -> usize {
        self.staged.len()
    }

    /// The entries of every input the Agent confirmed since the host last archived
    /// ([`Driver::archived`]), in order: what the host archives before it acknowledges.
    pub fn take_entries(&mut self) -> Vec<RingEntry> {
        core::mem::take(&mut self.entries)
    }

    /// Pin the replica's leaf store as the pre-state of the next input.
    fn pin(&mut self, replica: &Replica) -> Result<(), DriveError> {
        self.view = Some(
            replica
                .book()
                .store()
                .inner()
                .view()
                .map_err(|e| DriveError::Replica(format!("{e:?}")))?,
        );
        Ok(())
    }

    /// The witness of `keys` against the pinned view.
    fn witness(&self, keys: &KeySet) -> Result<Vec<u8>, DriveError> {
        let view = self
            .view
            .as_ref()
            .ok_or_else(|| DriveError::Replica("no pinned view".into()))?;
        let rep = |e: StoreError| DriveError::Replica(format!("{e:?}"));
        let pre = view.load(keys).map_err(rep)?;
        let tree_keys = keys_of(keys);
        let mut proofs = Vec::with_capacity(6);
        for (i, k) in tree_keys.iter().enumerate() {
            proofs.push(kernel_proof(view, TreeId(i as u8), k).map_err(rep)?);
        }
        Ok(KernelWitness::new(&pre, proofs).to_bytes())
    }

    /// Send an envelope; re-send it after an ambiguous failure.
    fn send(&mut self, envelope: &Envelope, blobs: Vec<Vec<u8>>) -> Result<Reply, DriveError> {
        let message = encode(envelope);
        let mut last = None;
        for _ in 0..=RETRIES {
            match self.port.invoke(message.clone(), blobs.clone()) {
                Ok(bytes) => {
                    return decode::<Reply>(&bytes).map_err(|e| {
                        DriveError::Diverged(format!("the reply does not decode: {e}"))
                    });
                }
                Err(PortError::Ambiguous(e)) => last = Some(PortError::Ambiguous(e)),
                Err(e) => return Err(DriveError::Port(e)),
            }
        }
        Err(DriveError::Port(
            last.unwrap_or(PortError::Ambiguous("no attempt".into())),
        ))
    }

    /// Send the replica's recorded calls since the last sync, and `deliveries` (already checked
    /// by the caller to be new and known), as one input (module documentation): a
    /// [`Driver::stage`] and a [`Driver::flush`]. `Ok(None)` when there is nothing to send.
    pub fn sync(
        &mut self,
        replica: &mut Replica,
        deliveries: Vec<([u8; 32], Time)>,
    ) -> Result<Option<Committed>, DriveError> {
        let staged = self.stage(replica, deliveries)?;
        let done = self.flush()?;
        Ok(staged.and_then(|seq| done.into_iter().find(|c| c.seq == seq)))
    }

    /// The input number and log hash the next staged input follows.
    fn tip(&self) -> (u64, [u8; 32]) {
        self.staged
            .last()
            .map_or((self.seq, self.log_head), |s| (s.entry.seq, s.log_hash))
    }

    /// Stage the replica's recorded calls since the last stage, and `deliveries`, as one input
    /// (module documentation, steps 1 to 3): its envelope and blobs are built against the state
    /// after every input staged before it, its service leaves are applied to the host's tree and
    /// the next view is pinned at once, as if the Agent had committed it. If adding it would take
    /// the staged batch over a bound of [`BatchLimits`], the staged inputs are flushed first; their
    /// results come with the next [`Driver::flush`]. Returns the input number it will commit
    /// under; `Ok(None)` when there is nothing to send. On an error nothing of this input is
    /// staged.
    pub fn stage(
        &mut self,
        replica: &mut Replica,
        deliveries: Vec<([u8; 32], Time)>,
    ) -> Result<Option<u64>, DriveError> {
        let recorded: Vec<Recorded> = replica.take_calls();
        let keys = replica.book().store().take_keys();
        let before = replica.book().vault_backend_take();
        let touched = Touches::take(replica, &deliveries);
        if recorded.is_empty() && deliveries.is_empty() {
            return Ok(None);
        }
        if let Some(r) = recorded
            .iter()
            .find(|r| matches!(r.call, Call::Unsupported { .. }))
        {
            return Err(DriveError::Unsupported(r.call.method().to_string()));
        }
        let mut keys = keys;
        keys.journals.insert(replica.book().journal());
        let openings = openings_before(replica, &keys, &before);
        // F-035: what the native run asked the key custody and the clerk, never a key.
        let (answers, clerk) = self.recorder.take();
        let clerk = recorded
            .iter()
            .any(|r| matches!(r.call, Call::IssueDisclosure { .. }))
            .then_some(clerk)
            .flatten();
        let private = encode(&Private {
            keys: answers,
            clerk,
            openings: PrivateVault::encode_rows(&openings),
        });
        let calls: Vec<Call> = recorded.iter().map(|r| r.call.clone()).collect();
        let summaries: Vec<Summary> = recorded.iter().map(|r| r.summary.clone()).collect();
        if self.mapped {
            // Layout 3 (`crate::mapped`) is driven one input per invocation: it is sent now, after
            // anything staged, and its result comes with the next flush. The Agent reads its own
            // leaves; the input names the service keys.
            if !self.staged.is_empty() {
                self.flush_staged()?;
            }
            let seq = self.seq + 1;
            let (leaves, whole) = self.service.leaves(&touched)?;
            let changes = changed(replica, &leaves, &deliveries)?;
            let envelope = Envelope::Mapped(Mapped::Input(MappedInput {
                seq,
                calls: calls.clone(),
                deliveries: deliveries.clone(),
                archived: self.archived,
                service: ServiceKeys {
                    leaves: leaves.iter().map(|(k, key, _)| (*k, key.clone())).collect(),
                    whole,
                },
                private: self.port.blob_key(&private),
            }));
            let reply = self.send(&envelope, vec![private])?;
            // A refused input leaves the leaves unapplied; the host rebuilds from the Agent.
            if matches!(reply, Reply::Committed(_) | Reply::Duplicate { .. }) {
                self.service.apply(&changes)?;
            }
            let (roots, next_posting) = replica_roots(replica, &self.service)?;
            let entry = RingEntry {
                seq,
                code: self.code,
                calls,
                deliveries,
                roots,
                next_posting,
                prev: self.log_head,
            };
            let log_hash = entry.log_hash();
            let committed = check(&entry, log_hash, &summaries, reply)?;
            self.seq = seq;
            self.log_head = committed.log_hash;
            self.last_entry = Some(entry.clone());
            self.entries.push(entry);
            self.done.push(committed);
            self.pin(replica)?;
            return Ok(Some(seq));
        }
        let witness = self.witness(&keys)?;
        let service = self.service.witness(&touched)?;
        let changes = changed(replica, &service.leaves, &deliveries)?;
        let service = encode(&service);
        // As if committed: the next input's witnesses stand where this one leaves the book.
        self.service.apply(&changes)?;
        let (roots, next_posting) = replica_roots(replica, &self.service)?;
        let (tip, prev) = self.tip();
        let seq = tip + 1;
        let input = InputEnvelope {
            seq,
            calls: calls.clone(),
            deliveries: deliveries.clone(),
            archived: self.archived,
            witness: self.port.blob_key(&witness),
            service: self.port.blob_key(&service),
            private: self.port.blob_key(&private),
        };
        let entry = RingEntry {
            seq,
            code: self.code,
            calls,
            deliveries,
            roots,
            next_posting,
            prev,
        };
        let log_hash = entry.log_hash();
        // The largest reply it can have: committed, with every field at its widest.
        let reply_bytes = encode(&Reply::Committed(Committed {
            seq: u64::MAX,
            summaries: summaries.clone(),
            roots,
            next_posting: u64::MAX,
            log_hash,
            state_bytes: u64::MAX,
        }))
        .len();
        let staged = Staged {
            message_bytes: encode(&input).len(),
            input,
            blobs: [witness, service, private],
            summaries,
            entry,
            log_hash,
            reply_bytes,
        };
        self.pin(replica)?;
        if !self.staged.is_empty() && !self.fits(&staged) {
            self.flush_staged()?;
        }
        self.staged.push(staged);
        Ok(Some(seq))
    }

    /// The most inputs one invocation carries: the batch bound, the Agent's ring, the actor gas
    /// and [`MAX_BATCH_INPUTS`], whichever is least; at least one.
    pub fn batch_cap(&self) -> usize {
        let l = &self.limits;
        let by_gas = l.max_actor_gas.checked_div(l.input_gas).unwrap_or(u64::MAX);
        l.max_inputs
            .min(MAX_BATCH_INPUTS)
            .min(usize::try_from(self.ring).unwrap_or(usize::MAX))
            .min(usize::try_from(by_gas).unwrap_or(usize::MAX))
            .max(1)
    }

    /// Whether `next` fits in one invocation with the inputs staged.
    fn fits(&self, next: &Staged) -> bool {
        let l = &self.limits;
        let n = self.staged.len() + 1;
        if n > self.batch_cap() {
            return false;
        }
        let all = || self.staged.iter().chain(core::iter::once(next));
        // The batch's envelope: its tag, the inputs, and at most four packs (key and count).
        let message = 1
            + varint_len(n)
            + all().map(|s| s.message_bytes).sum::<usize>()
            + 1
            + MAX_BATCH_PACKS * (32 + 10 + 5);
        let reply = 1 + varint_len(n) + all().map(|s| s.reply_bytes).sum::<usize>();
        let blobs: usize = all().map(Staged::packed_bytes).sum::<usize>() + 16 * MAX_BATCH_PACKS;
        message <= l.max_message_bytes
            && reply <= l.max_reply_bytes
            && blobs <= l.max_blob_bytes
            && pack_runs(all(), l.max_pack_bytes).len() <= l.max_packs.min(MAX_BATCH_PACKS)
    }

    /// Send every staged input as one invocation, and check every reply (module documentation,
    /// steps 4 and 5). Returns every input committed since the last flush, in order: those
    /// [`Driver::stage`] flushed to make room, then these. An error leaves the inputs before the
    /// failing one confirmed ([`Driver::seq`]) and the rest not; the host rebuilds either way.
    pub fn flush(&mut self) -> Result<Vec<Committed>, DriveError> {
        if !self.staged.is_empty() {
            self.flush_staged()?;
        }
        Ok(core::mem::take(&mut self.done))
    }

    fn flush_staged(&mut self) -> Result<(), DriveError> {
        let staged = core::mem::take(&mut self.staged);
        let replies = if staged.len() == 1 {
            let s = &staged[0];
            vec![self.send(&Envelope::Input(s.input.clone()), s.blobs.to_vec())?]
        } else {
            let (envelope, packs) = self.batch_of(&staged);
            match self.send(&envelope, packs)? {
                Reply::Batch(r) if r.len() == staged.len() => r,
                Reply::Refused(r) => return Err(DriveError::Refused(r)),
                other => {
                    return Err(DriveError::Diverged(format!(
                        "a batch of {} answered {other:?}",
                        staged.len()
                    )));
                }
            }
        };
        for (s, reply) in staged.into_iter().zip(replies) {
            let committed = check(&s.entry, s.log_hash, &s.summaries, reply)?;
            self.seq = committed.seq;
            self.log_head = committed.log_hash;
            self.last_entry = Some(s.entry.clone());
            self.entries.push(s.entry);
            self.done.push(committed);
        }
        Ok(())
    }

    /// The envelope of a batch, and its packs.
    fn batch_of(&self, staged: &[Staged]) -> (Envelope, Vec<Vec<u8>>) {
        let mut packs = Vec::new();
        let mut names = Vec::new();
        let mut at = 0;
        for run in pack_runs(staged, self.limits.max_pack_bytes) {
            let mut blobs = Vec::with_capacity(3 * run);
            for s in &staged[at..at + run] {
                let keys = [s.input.witness, s.input.service, s.input.private];
                for (k, b) in keys.into_iter().zip(&s.blobs) {
                    blobs.push((k, b.as_slice()));
                }
            }
            let pack = encode(&Pack { blobs });
            names.push((self.port.blob_key(&pack), run as u32));
            packs.push(pack);
            at += run;
        }
        let envelope = Envelope::Batch(BatchEnvelope {
            inputs: staged.iter().map(|s| s.input.clone()).collect(),
            packs: names,
        });
        (envelope, packs)
    }

    /// A driver for an Agent already founded, after a host restart (`T-005-08`, `T-005-09`).
    ///
    /// `archive` is what the host archived: the committed inputs, in order, from the first. The
    /// Agent may have committed more (the host died after the Agent committed, before it archived):
    /// those are still in the Agent's delta ring, which is trimmed only up to what the host
    /// acknowledged, and they are read back and appended to `archive`. The replica is rebuilt by
    /// replaying the whole archive from genesis ([`replay`]) and must reach the Agent's roots.
    #[allow(clippy::too_many_arguments)]
    pub fn recover(
        port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        archive: &mut Vec<RingEntry>,
    ) -> Result<(Self, Replica), DriveError> {
        Self::recover_in(&ReplicaStores::Memory, port, keys, genesis, op, at, archive)
    }

    /// [`Driver::recover`], with the replica's stores where `stores` says (`T-031-14`).
    #[allow(clippy::too_many_arguments)]
    pub fn recover_in(
        stores: &ReplicaStores,
        port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        archive: &mut Vec<RingEntry>,
    ) -> Result<(Self, Replica), DriveError> {
        Self::recover_audited(stores, port, keys, genesis, op, at, archive, None)
    }

    /// [`Driver::recover_in`], dropping from the replica's audit outbox, as it replays, the
    /// records the audit store holds through `audit_floor` (`T-013-13`, [`replay_audited`]).
    #[allow(clippy::too_many_arguments)]
    pub fn recover_audited(
        stores: &ReplicaStores,
        port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        archive: &mut Vec<RingEntry>,
        audit_floor: Option<(u64, [u8; 32])>,
    ) -> Result<(Self, Replica), DriveError> {
        Self::recover_with(
            stores,
            port,
            keys,
            genesis,
            op,
            at,
            archive,
            audit_floor,
            None,
        )
    }

    /// [`Driver::recover_in`] for an Agent founded as a **segment** of `verify-genesis`
    /// ([`Driver::found_at_chunk`]): the replica is rebuilt as the segment was founded, chunks
    /// `0..start` of `spec` applied natively, then the segment's archive replayed over them. Without
    /// it a segment's core could not take back an invocation the Agent did not commit, so a single
    /// aborted invocation stopped the segment for good (`verify-genesis` of a full-size
    /// chainspec, 2026-10-11).
    #[allow(clippy::too_many_arguments)]
    pub fn recover_segment(
        stores: &ReplicaStores,
        port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        archive: &mut Vec<RingEntry>,
        spec: &ledger_core::chainspec::ChainSpec,
        start: u32,
    ) -> Result<(Self, Replica), DriveError> {
        if genesis.spec.is_some() || genesis.chainspec != Some(spec.commitment()) {
            return Err(DriveError::Replica(
                "a segment is founded on the chainspec's commitment alone".into(),
            ));
        }
        let prefix = (start > 0).then_some((spec, start));
        Self::recover_with(stores, port, keys, genesis, op, at, archive, None, prefix)
    }

    #[allow(clippy::too_many_arguments)]
    fn recover_with(
        stores: &ReplicaStores,
        mut port: P,
        keys: HostKeys,
        genesis: Genesis,
        op: OperationId,
        at: Time,
        archive: &mut Vec<RingEntry>,
        audit_floor: Option<(u64, [u8; 32])>,
        prefix: Option<(&ledger_core::chainspec::ChainSpec, u32)>,
    ) -> Result<(Self, Replica), DriveError> {
        let hot = port
            .hot()
            .map_err(DriveError::Port)?
            .ok_or(DriveError::Refused(AgentRefusal::NotFounded))?;
        if archive.len() as u64 > hot.inputs {
            return Err(DriveError::Diverged(format!(
                "the host archived {} inputs and the Agent committed {}",
                archive.len(),
                hot.inputs
            )));
        }
        for seq in archive.len() as u64 + 1..=hot.inputs {
            let entry = port.entry(seq).map_err(DriveError::Port)?.ok_or_else(|| {
                DriveError::Diverged(format!(
                    "input {seq} is neither archived nor in the Agent's ring"
                ))
            })?;
            archive.push(entry);
        }
        let (mut replica, service, recorder, genesis) =
            replay_with(stores, &keys, genesis, op, at, archive, audit_floor, prefix)?;
        recorder.clear();
        // From here the replica runs the Agent's next inputs, under its hot row's code and mode.
        replica.set_code(hot.code, hot.authority);
        let (roots, next_posting) = replica_roots(&replica, &service)?;
        if roots != hot.roots() || next_posting != hot.next_posting {
            return Err(DriveError::Diverged(format!(
                "the rebuilt replica does not stand where the Agent does (input {})",
                hot.inputs
            )));
        }
        let _ = replica.book().store().take_keys();
        replica.book().vault_backend_take();
        let _ = replica.state().take_touched();
        let mut driver = Driver {
            port,
            keys,
            recorder,
            view: None,
            service,
            seq: hot.inputs,
            log_head: hot.log_head,
            archived: (hot.inputs > 0).then_some((hot.inputs, hot.log_head)),
            ring: hot.limits.ring,
            limits: BatchLimits::DEFAULT,
            staged: Vec::new(),
            done: Vec::new(),
            entries: Vec::new(),
            last_entry: archive.last().cloned(),
            mapped: hot.layout == crate::mapped::LAYOUT_MAPPED,
            genesis,
            code: hot.code,
            authority: hot.authority,
        };
        driver.pin(&replica)?;
        Ok((driver, replica))
    }
}

impl<P: AgentPort> Driver<P> {
    /// Bring the Agent forward through archived inputs it does not hold (`T-005-16`, a restore
    /// from the off-node archive): each entry's calls are applied to the replica, as [`replay`]
    /// applies them, then staged and sent to the Agent exactly as a live input is, at most one
    /// invocation's inputs between two acknowledgements. Every input the driver stages must be
    /// the archived entry itself (its log hash covers the calls, the deliveries, the roots and
    /// the chain), and every reply is checked against it as a live reply is: any difference is a
    /// [`DriveError::Diverged`], and nothing is answered from a restore that diverged.
    ///
    /// The entries must continue the Agent's chain: the first is numbered [`Driver::seq`] + 1 and
    /// follows [`Driver::log_head`]. On success the Agent stands at the last entry; the host has
    /// them archived already (they came from its archive), so the next invocation acknowledges
    /// them.
    pub fn roll_forward(
        &mut self,
        replica: &mut Replica,
        entries: &[RingEntry],
    ) -> Result<(), DriveError> {
        // F-035: the recording clerk, so that the Agent receives what it signs.
        let clerk = self.recorder.clerk();
        let chunk = self.batch_cap();
        for part in entries.chunks(chunk) {
            for e in part {
                let (tip, prev) = self.tip();
                if e.seq != tip + 1 || e.prev != prev {
                    return Err(DriveError::Diverged(format!(
                        "roll forward: archived input {} does not continue the Agent at input {tip}",
                        e.seq
                    )));
                }
                // The code that executed it, as the Agent's ring names it (`T-037-02`), is what
                // its checkpoints bind (`T-037-03`).
                self.code = e.code;
                replica.set_code(self.code, self.authority);
                for c in &e.calls {
                    let clerk = clerk.as_deref().map(|c| c as &dyn ClerkSigner);
                    crate::calls::apply(replica, c, clerk);
                }
                let want = e.log_hash();
                let staged = self.stage(replica, e.deliveries.clone())?;
                let got = self
                    .staged
                    .last()
                    .filter(|s| Some(s.entry.seq) == staged)
                    .map(|s| s.log_hash)
                    .or_else(|| {
                        self.entries
                            .last()
                            .filter(|x| Some(x.seq) == staged)
                            .map(RingEntry::log_hash)
                    });
                if got != Some(want) {
                    return Err(DriveError::Diverged(format!(
                        "roll forward: input {} does not reproduce its archived entry",
                        e.seq
                    )));
                }
            }
            self.flush()?;
            // Archived already: the next invocation carries the acknowledgement.
            self.archived();
        }
        Ok(())
    }
}

/// Check one input's reply against its native run (module documentation, step 5).
fn check(
    entry: &RingEntry,
    log_hash: [u8; 32],
    summaries: &[Summary],
    reply: Reply,
) -> Result<Committed, DriveError> {
    let seq = entry.seq;
    match reply {
        Reply::Committed(c) => {
            if c.summaries != summaries {
                return Err(DriveError::Diverged(format!(
                    "input {seq}: the Agent's summaries {:?} are not the replica's {:?}",
                    c.summaries, summaries
                )));
            }
            if c.seq != seq
                || c.roots != entry.roots
                || c.next_posting != entry.next_posting
                || c.log_hash != log_hash
            {
                return Err(DriveError::Diverged(format!(
                    "input {seq}: the Agent's roots are not the replica's"
                )));
            }
            Ok(c)
        }
        // Committed before an ambiguous failure: the entry must be this one.
        Reply::Duplicate {
            seq: d,
            log_hash: got,
        } if d == seq && got == log_hash => Ok(Committed {
            seq,
            summaries: summaries.to_vec(),
            roots: entry.roots,
            next_posting: entry.next_posting,
            log_hash,
            state_bytes: 0,
        }),
        Reply::Duplicate { .. } => Err(DriveError::Diverged(format!(
            "input {seq}: the Agent holds another entry under this number"
        ))),
        Reply::Refused(r) => Err(DriveError::Refused(r)),
        Reply::Archived { .. } | Reply::Batch(_) => Err(DriveError::Diverged(format!(
            "input {seq}: answered as an archive or a batch"
        ))),
    }
}

/// Rebuild the replica and the service tree from genesis and the committed inputs, in order
/// (`REQ-RT-3`, `INV-6`): every call applied natively, every input's service leaves applied to
/// the tree, and every entry's roots checked on the way. The host's recovery after a restart,
/// and the replay test. The replica records calls, as a driver's does.
#[allow(clippy::too_many_arguments)]
pub fn replay(
    keys: &HostKeys,
    genesis: Genesis,
    op: OperationId,
    at: Time,
    entries: &[RingEntry],
) -> Result<(Replica, ServiceStore), DriveError> {
    replay_in(&ReplicaStores::Memory, keys, genesis, op, at, entries)
}

/// [`replay`], with the replica's stores where `stores` says (`T-031-14`).
#[allow(clippy::too_many_arguments)]
pub fn replay_in(
    stores: &ReplicaStores,
    keys: &HostKeys,
    genesis: Genesis,
    op: OperationId,
    at: Time,
    entries: &[RingEntry],
) -> Result<(Replica, ServiceStore), DriveError> {
    replay_audited(stores, keys, genesis, op, at, entries, None)
}

/// [`replay_in`], dropping from the replica's audit outbox after genesis and after every input
/// the records the audit store holds through `audit_floor` (`T-013-13`): a rebuild never holds the
/// trail in memory again, whatever the length of the archive. The record emitted at the floor's
/// number is checked against the store's hash; if it is another, nothing more is dropped, and
/// the shipper finds the store refusing it and reports it (`REQ-AT-2`).
#[allow(clippy::too_many_arguments)]
pub fn replay_audited(
    stores: &ReplicaStores,
    keys: &HostKeys,
    genesis: Genesis,
    op: OperationId,
    at: Time,
    entries: &[RingEntry],
    audit_floor: Option<(u64, [u8; 32])>,
) -> Result<(Replica, ServiceStore), DriveError> {
    replay_with(stores, keys, genesis, op, at, entries, audit_floor, None)
        .map(|(r, s, _, _)| (r, s))
}

/// `prefix`: the chunks of a chainspec applied natively after genesis, before the entries, as a
/// segment of `verify-genesis` is founded ([`Driver::found_at_chunk`]).
#[allow(clippy::too_many_arguments)]
fn replay_with(
    stores: &ReplicaStores,
    keys: &HostKeys,
    genesis: Genesis,
    op: OperationId,
    at: Time,
    entries: &[RingEntry],
    audit_floor: Option<(u64, [u8; 32])>,
    prefix: Option<(&ledger_core::chainspec::ChainSpec, u32)>,
) -> Result<(Replica, ServiceStore, Recorder, Roots), DriveError> {
    let mut floor = audit_floor;
    let mut drop_shipped = |replica: &mut Replica| {
        if let Some(f) = floor
            && replica.acknowledge_audit(f).is_err()
        {
            floor = None;
        }
    };
    let rep = |e: &dyn core::fmt::Debug| DriveError::Replica(format!("{e:?}"));
    // The mode a replay's checkpoints bind: the founding's, since no input changes it yet
    // (`T-037-05` gives the mode its inputs).
    let authority = FoundingCode::of(&genesis).authority;
    let (store, vault) = stores.book()?;
    let (recorder, ring) = Recorder::new(keys);
    let book =
        Book::bootstrap(RecStore::new(store), RecVault::new(vault), ring).map_err(|e| rep(&e))?;
    let (mut replica, founded) = Ledger::genesis(book, genesis, op, at).map_err(|e| rep(&e))?;
    if let Some((spec, start)) = prefix {
        apply_spec_prefix(&mut replica, &founded, spec, start)?;
        // What the prefix recorded is never read: let it go now, as the founding does
        // (`Driver::found_with`), not after the whole archive.
        let _ = replica.book().store().take_keys();
        replica.book().vault_backend_take();
        recorder.clear();
    }
    let _ = replica.state().take_touched();
    let mut service = ServiceStore::of_in(stores, replica.state())?;
    let genesis_roots = replica_roots(&replica, &service)?.0;
    drop_shipped(&mut replica);
    replica.record_calls(true);
    let clerk = recorder.clerk();
    let mut prev = [0u8; 32];
    for (i, e) in entries.iter().enumerate() {
        if e.seq != i as u64 + 1 || e.prev != prev {
            return Err(DriveError::Diverged(format!(
                "the archive is not a chain at input {}",
                e.seq
            )));
        }
        // Each input under the code its entry names (`T-037-02`): what its checkpoints bind.
        replica.set_code(e.code, authority);
        for c in &e.calls {
            let clerk = clerk.as_deref().map(|c| c as &dyn ClerkSigner);
            crate::calls::apply(&mut replica, c, clerk);
        }
        let _ = replica.take_calls();
        let touched = Touches::take(&replica, &e.deliveries);
        let (before, _) = service.leaves(&touched)?;
        let changes = changed(&replica, &before, &e.deliveries)?;
        service.apply(&changes)?;
        let (roots, next) = replica_roots(&replica, &service)?;
        if roots != e.roots || next != e.next_posting {
            return Err(DriveError::Diverged(format!(
                "replay: input {} does not reach its recorded roots",
                e.seq
            )));
        }
        prev = e.log_hash();
        drop_shipped(&mut replica);
    }
    Ok((replica, service, recorder, genesis_roots))
}

/// The openings, before the input, of every vault row of an account the input read.
fn openings_before(
    replica: &Replica,
    keys: &KeySet,
    before: &BTreeMap<RowKey, Option<LayerSums>>,
) -> BTreeMap<RowKey, LayerSums> {
    let accounts: BTreeSet<AccountId> = keys.accounts.iter().copied().collect();
    let mut out = BTreeMap::new();
    for (k, v) in replica.book().vault().rows() {
        if accounts.contains(&k.account) && !before.contains_key(k) {
            out.insert(*k, *v);
        }
    }
    for (k, v) in before {
        if accounts.contains(&k.account)
            && let Some(v) = v
        {
            out.insert(*k, *v);
        }
    }
    out
}

/// Access to the recording vault through the book (the book owns it).
trait VaultTake {
    fn vault_backend_take(&self) -> BTreeMap<RowKey, Option<LayerSums>>;
}

impl VaultTake for Book<HostStore, HostVault> {
    fn vault_backend_take(&self) -> BTreeMap<RowKey, Option<LayerSums>> {
        self.vault().backend().take_before()
    }
}

/// The Agent's transition, natively, over rows in memory: the same code the VOS actor runs,
/// without the runtime. The conformance suite's `through-agent/native` target and the tests run
/// on it; `agents/core-host` runs the real thing.
pub struct NativePort {
    /// The Agent's rows.
    pub rows: MemRows,
    /// The principal invocations come from.
    pub origin: [u8; 32],
    /// The observed slot.
    pub slot: u64,
    /// Fail the next invocation this way, for tests: `Some(true)` after running it (ambiguous),
    /// `Some(false)` before (unavailable).
    pub fail_next: Option<bool>,
}

/// The principal the native port and the tests invoke as.
pub const DRIVER: [u8; 32] = [0xD7; 32];

impl NativePort {
    /// An empty Agent, invoked as [`DRIVER`].
    pub fn new() -> Self {
        NativePort {
            rows: MemRows::default(),
            origin: DRIVER,
            slot: 0,
            fail_next: None,
        }
    }
}

impl Default for NativePort {
    fn default() -> Self {
        NativePort::new()
    }
}

impl AgentPort for NativePort {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        if self.fail_next == Some(false) {
            self.fail_next = None;
            return Err(PortError::Unavailable("injected".into()));
        }
        self.slot += 1;
        let by_key: BTreeMap<[u8; 32], Vec<u8>> = blobs
            .into_iter()
            .map(|b| (self.blob_key(&b).hash, b))
            .collect();
        let fetch = |k: &BlobKey| {
            by_key
                .get(&k.hash)
                .filter(|b| b.len() as u64 == k.len)
                .cloned()
        };
        let inv = Invocation {
            origin: Some(self.origin),
            slot: Some(self.slot),
            message: &message,
            blob: &fetch,
        };
        let reply = crate::transition::transition(&mut self.rows, &inv);
        if self.fail_next == Some(true) {
            self.fail_next = None;
            return Err(PortError::Ambiguous("injected after the run".into()));
        }
        Ok(reply)
    }

    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        BlobKey {
            hash: blake2b_256(b"aledgent/core-agent/native-blob/v1", &[bytes]),
            len: bytes.len() as u64,
        }
    }

    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        use crate::transition::Rows as _;
        self.rows
            .get(crate::hot::HOT)
            .map(|b| crate::hot::decode_hot(&b).map_err(PortError::Unavailable))
            .transpose()
    }

    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        use crate::transition::Rows as _;
        self.rows
            .get(&crate::hot::ring_key(seq))
            .map(|b| decode::<RingEntry>(&b).map_err(PortError::Unavailable))
            .transpose()
    }
}

/// Every row of `rows`, for scans of the Agent's state (`T-005-05`).
pub fn rows_bytes(rows: &MemRows) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in &rows.0 {
        out.extend_from_slice(k);
        out.extend_from_slice(v);
    }
    out
}

impl<T: AgentPort + ?Sized> AgentPort for Box<T> {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        (**self).invoke(message, blobs)
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        (**self).blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        (**self).hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        (**self).entry(seq)
    }
}

/// A request to an Agent host's raw port (`aledgent-core-host agent-server`): one Agent per
/// connection, created when the connection opens, gone when it closes. The conformance suite's
/// `through-agent/vos` target and the measurements use it; a deployment serves ingress's
/// boundary instead.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RawRequest {
    /// [`AgentPort::invoke`].
    Invoke {
        /// The message.
        message: Vec<u8>,
        /// The caller blobs.
        blobs: Vec<Vec<u8>>,
    },
    /// [`AgentPort::hot`].
    Hot,
    /// [`AgentPort::entry`].
    Entry(u64),
    /// The measurements of every invocation so far (`aledgent-core-host` fills it).
    Samples,
}

/// The raw port's response.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RawResponse {
    /// The Agent's reply bytes.
    Reply(Vec<u8>),
    /// The hot row, encoded, if any.
    Hot(Option<Vec<u8>>),
    /// A ring entry, encoded, if any.
    Entry(Option<Vec<u8>>),
    /// Measurements, as the host encodes them.
    Samples(Vec<u8>),
    /// The port failed.
    Failed(PortError),
}

/// Write one length-prefixed frame.
pub fn write_frame(w: &mut impl std::io::Write, bytes: &[u8]) -> std::io::Result<()> {
    let n = u32::try_from(bytes.len()).map_err(|_| std::io::Error::other("a frame too large"))?;
    w.write_all(&n.to_be_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

/// Read one length-prefixed frame (at most 64 MiB).
pub fn read_frame(r: &mut impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut n = [0u8; 4];
    r.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n) as usize;
    if n > 64 * 1024 * 1024 {
        return Err(std::io::Error::other("a frame over the limit"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// The client of an Agent host's raw port (see [`RawRequest`]).
pub struct RemotePort {
    stream: std::net::TcpStream,
}

impl RemotePort {
    /// Connect to `tcp:host:port`: a fresh Agent.
    pub fn connect(endpoint: &str) -> Result<Self, String> {
        let addr = endpoint
            .strip_prefix("tcp:")
            .ok_or_else(|| format!("{endpoint}: not tcp:host:port"))?;
        let stream = std::net::TcpStream::connect(addr).map_err(|e| e.to_string())?;
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        Ok(RemotePort { stream })
    }

    fn call(&mut self, request: &RawRequest) -> Result<RawResponse, PortError> {
        write_frame(&mut self.stream, &encode(request))
            .map_err(|e| PortError::Unavailable(e.to_string()))?;
        let frame =
            read_frame(&mut self.stream).map_err(|e| PortError::Ambiguous(e.to_string()))?;
        decode(&frame).map_err(PortError::Ambiguous)
    }

    /// The host's measurements of every invocation so far, as it encodes them.
    pub fn samples(&mut self) -> Result<Vec<u8>, PortError> {
        match self.call(&RawRequest::Samples)? {
            RawResponse::Samples(b) => Ok(b),
            RawResponse::Failed(e) => Err(e),
            other => Err(PortError::Ambiguous(format!("unexpected {other:?}"))),
        }
    }
}

impl AgentPort for RemotePort {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        match self.call(&RawRequest::Invoke { message, blobs })? {
            RawResponse::Reply(r) => Ok(r),
            RawResponse::Failed(e) => Err(e),
            other => Err(PortError::Ambiguous(format!("unexpected {other:?}"))),
        }
    }

    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        // VOS's BlobRef::of_bytes: blake2b-256 under "vos/blob" (vos-protocol), which the host's
        // runtime checks every staged blob against.
        BlobKey {
            hash: vos_blob_hash(bytes),
            len: bytes.len() as u64,
        }
    }

    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        match self.call(&RawRequest::Hot)? {
            RawResponse::Hot(h) => h
                .map(|b| crate::hot::decode_hot(&b).map_err(PortError::Ambiguous))
                .transpose(),
            RawResponse::Failed(e) => Err(e),
            other => Err(PortError::Ambiguous(format!("unexpected {other:?}"))),
        }
    }

    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        match self.call(&RawRequest::Entry(seq))? {
            RawResponse::Entry(e) => e
                .map(|b| decode::<RingEntry>(&b).map_err(PortError::Ambiguous))
                .transpose(),
            RawResponse::Failed(e) => Err(e),
            other => Err(PortError::Ambiguous(format!("unexpected {other:?}"))),
        }
    }
}

/// VOS's content address of a blob (`vos-protocol`, `BlobRef::of_bytes`, `Hash::digest`).
pub fn vos_blob_hash(bytes: &[u8]) -> [u8; 32] {
    blake2b_256(b"vos/blob", &[bytes])
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use ledger_kernel::keys::KeyAnswer;

    const CLERK: [u8; 32] = [7; 32];

    /// `F-035`: what the replica asked the custody and the clerk is what the Agent is given, and
    /// the Agent's key ring and clerk answer from it alone, as the host's did.
    #[test]
    fn nfr_8_the_agent_answers_from_what_its_host_recorded() {
        let keys = HostKeys::local([5; 32], Some(CLERK));
        let (recorder, ring) = Recorder::new(&keys);
        let account = AccountId([3; 16]);
        let sig = ring.sign_account(&account, b"a transfer");
        let clerk = recorder.clerk().expect("a clerk");
        let disclosure = clerk.sign(b"a disclosure");
        let (answers, recorded) = recorder.take();
        let private = Private {
            keys: answers,
            clerk: recorded,
            openings: Vec::new(),
        };
        let private: Private = decode(&encode(&private)).expect("round trip");
        let agent = private.key_ring();
        assert_eq!(agent.sign_account(&account, b"a transfer"), sig);
        assert_eq!(agent.registrar_key(), ring.registrar_key());
        let agent_clerk = private.clerk.as_ref().expect("the clerk's answers");
        assert_eq!(agent_clerk.sign(b"a disclosure"), disclosure);
        assert_eq!(agent_clerk.public_key(), clerk.public_key());
        // Taken: the next input starts empty but for the registrar's key.
        let (next, next_clerk) = recorder.take();
        assert_eq!(next.len(), 1);
        assert_eq!(
            KeyAnswer::decode(&next[0].1),
            Some(keys.custody.answer(&KeyRequest::RegistrarKey))
        );
        assert!(next_clerk.expect("a clerk").signatures.is_empty());
    }

    #[test]
    #[should_panic(expected = "no clerk signature")]
    fn a_message_the_host_did_not_have_signed_aborts_the_agent() {
        let answers = ClerkAnswers {
            public: [1; 32],
            signatures: Vec::new(),
        };
        let _ = answers.sign(b"anything");
    }
}
