// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What crosses into the Agent and back (`T-005-03`): the envelope of one invocation, its
//! private inputs, and the reply.
//!
//! Three carriers, each chosen for what it holds (`F-005` §Design, "Witness carrier"):
//!
//! | Carrier | Holds | Limit | Kept by VOS |
//! |---|---|---|---|
//! | The invocation message | The [`Envelope`]: the input number, the library [`Call`]s, delivery records, the archive acknowledgement, and the names of the three blobs below | 16 KiB | In the invocation's work, which this binding's host never journals |
//! | A caller blob | The kernel witness ([`crate::witness::KernelWitness`]) | 192 KiB for all caller blobs | Staged for the one invocation only |
//! | A caller blob | The service witness ([`crate::service::ServiceWitness`]) | the same | The same |
//! | A caller blob | The [`Private`] inputs: the key custody's answers, the clerk's signatures, the touched openings (`F-035`: never a key) | the same | The same. Never written to Agent state (`REQ-RT-6`) |
//!
//! Every value is encoded with `postcard` (serde), which is deterministic: the same value gives
//! the same bytes on the host and in the guest.
//!
//! Trade-off, stated plainly: the Agent ABI at the pinned VOS has no private-input channel of its
//! own (`notes/MEASUREMENTS-F002.md` §11.2 (g)). A caller blob is the nearest thing: the runtime
//! stages it for one invocation, verifies it against its content address, and does not retain
//! it. But the blob travels in the work item, and a host that journals work items (the Local
//! host's journal) would keep it. This host does not; `T-005-05`'s test scans the Agent's state
//! for every private byte.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use ledger_core::account::Time;
use ledger_core::call::{Call, Summary};
use ledger_core::id::OperationId;
use ledger_core::ledger::Genesis;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// A caller-supplied blob, named by its content: VOS's `BlobRef` (hash and length).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobKey {
    /// The content hash, as VOS computes it.
    pub hash: [u8; 32],
    /// The length in bytes.
    pub len: u64,
}

/// What the driver compares after every input: the book's two roots and the audit chain's head.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roots {
    /// The kernel's composite root (`F-001`).
    pub kernel: [u8; 32],
    /// The service tree's root: the service store's collections and the deliveries recorded, as
    /// leaves (`crate::service`, `F-006` decision 5). Not an export's service root, which is the
    /// digest of its sections (`F-014`).
    pub service: [u8; 32],
    /// The audit chain's head: [`audit_digest`] of the records emitted and the last one's hash
    /// (`REQ-AT-1`). The audit trail is not in the service sections, so it is compared apart.
    pub audit: [u8; 32],
}

/// The digest of an audit chain's head: `blake2b-256(domain ‖ emitted ‖ head)`.
pub fn audit_digest(emitted: u64, head: &[u8; 32]) -> [u8; 32] {
    cipher_clerk::crypto::blake2b_256(
        b"aledgent/core-agent/audit-head/v1",
        &[&emitted.to_be_bytes(), head],
    )
}

/// One invocation's message.
#[derive(Clone, Serialize, Deserialize)]
pub enum Envelope {
    /// Found the book: bootstrap the kernel, run genesis, and fix the driver's principal.
    Genesis(Box<GenesisEnvelope>),
    /// Apply one input.
    Input(InputEnvelope),
    /// The driver archived the delta ring up to `through`: trim it.
    Archived {
        /// The last archived input number.
        through: u64,
        /// Its entry's log hash, as the Agent reported it.
        log_hash: [u8; 32],
    },
    /// Apply several inputs in one invocation, in order (`T-031-02`): answered by
    /// [`Reply::Batch`].
    Batch(BatchEnvelope),
    /// Found the book on a genesis **state** the host built natively from a chainspec (`F-036`,
    /// owner ruling `genesis-chainspec`): the Agent takes the roots and the hot state, and
    /// executes nothing. After `Batch`, so no earlier variant's tag moves.
    StateGenesis(Box<StateGenesisEnvelope>),
    /// A layout-3 invocation: the book in the Agent's committed maps (`F-031`, `crate::mapped`).
    /// Last, because it is a feature: postcard encodes a variant by its index, and a variant
    /// that only some builds have must not shift the others (the host always has `mapped`, the
    /// production guest does not; before this order the guest read every batch as malformed).
    #[cfg(feature = "mapped")]
    Mapped(crate::mapped::Mapped),
}

/// The most inputs one [`Envelope::Batch`] may carry; more is refused as
/// [`AgentRefusal::Malformed`]. The host's own bound is lower (the message, reply and blob limits
/// of one invocation bind first); this one bounds the Agent's work whatever the host sends.
pub const MAX_BATCH_INPUTS: usize = 256;

/// The most packs one [`Envelope::Batch`] may name: VOS hands an actor at most four caller blobs
/// per invocation (`vos/src/agent/execution.rs`, `MAX_EXECUTION_BLOBS` at the pinned VOS).
pub const MAX_BATCH_PACKS: usize = 4;

/// Several inputs in one invocation (`T-031-02`, `REQ-RT-1`, `INV-18`).
///
/// Each input is exactly what [`Envelope::Input`] would carry for it alone: its own calls, its own
/// three blobs and its own witness, built against the state after the input before it. The Agent
/// applies them in order, each over the writes of the ones before (`crate::transition`), with a
/// **prefix commit**: the inputs before the first one it refuses commit, that one answers its
/// refusal, and every later one answers [`AgentRefusal::Skipped`].
///
/// **Packs.** VOS gives an actor at most [`MAX_BATCH_PACKS`] caller blobs per invocation, and an
/// input brings three. So the blobs of a run of consecutive inputs travel together in one caller
/// blob, a [`Pack`]: `packs` names each pack and how many inputs it covers, in order. Each pack is
/// fetched only when its first input is applied, and split into its blobs before that input runs;
/// each blob leaves the heap with the last input that uses it, so the guest's heap holds one
/// pack at a time, and each blob once.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchEnvelope {
    /// The inputs, in input-number order.
    pub inputs: Vec<InputEnvelope>,
    /// Each pack's blob, and how many consecutive inputs its blobs are for.
    pub packs: Vec<(BlobKey, u32)>,
}

/// One pack's content: every blob of a run of inputs, each under the key its input names it by.
/// The pack is a caller blob itself, so its content is checked against its key by the runtime;
/// the keys inside it are the host's, carried as the message carries them, and the Agent matches
/// each input's blobs by key and length.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pack<'a> {
    /// The blobs, borrowed from the pack's bytes when decoded.
    #[serde(borrow)]
    pub blobs: Vec<(BlobKey, &'a [u8])>,
}

/// The code a writer is founded under (`T-037-02`, `REQ-RT-15`, `REQ-RT-18`): the founder's word,
/// as the driver is. The generation is 0 and the construction the build's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundingCode {
    /// The genesis code version *H₀*: the chainspec's, which the host checked its binaries against
    /// (`F-036`); zeros for a book whose code is not named (tests, stress books).
    pub code: [u8; 32],
    /// The digest of the initial upgrade-authority mode; zeros until modes exist (`T-037-05`).
    pub authority: [u8; 32],
}

impl FoundingCode {
    /// The code a host founds a book under: the genesis code its chainspec names, if the host has
    /// the chainspec, else none.
    pub fn of(genesis: &Genesis) -> FoundingCode {
        FoundingCode {
            code: genesis
                .spec
                .as_deref()
                .and_then(|s| s.header().code.as_ref())
                .map_or([0; 32], |c| c.version()),
            authority: [0; 32],
        }
    }
}

/// The founding invocation.
#[derive(Clone, Serialize, Deserialize)]
pub struct GenesisEnvelope {
    /// The reviewed genesis configuration (`REQ-AS-2`).
    pub genesis: Genesis,
    /// Genesis's operation reference.
    pub op: OperationId,
    /// Genesis's time.
    pub at: Time,
    /// The Agent's bounds: the budget `H` (`T-005-02`), the ring and the outbox (`T-005-07`).
    pub limits: crate::hot::Limits,
    /// The kernel witness: the book's journal key, absent, against the empty tree.
    pub witness: BlobKey,
    /// The private inputs (the key-ring root).
    pub private: BlobKey,
    /// The code it is founded under.
    pub code: FoundingCode,
}

/// The founding invocation of a chainspec book (`F-036`, owner ruling `genesis-chainspec`).
///
/// Like a Substrate chain's genesis, the book's first state is **defined**, not reached by
/// executing inputs: the host builds it natively from the chainspec, with the same library code
/// the Agent would run (`Ledger::genesis` with the chainspec, every chunk through
/// `apply_spec_chunk`), and hands the Agent what its hot row holds after it. The Agent takes it on
/// its founder's word, as it takes the driver: only the Agent's owner can found it. What makes
/// that safe is that the genesis is a pure function of the chainspec (and the key custody), which
/// genesis commits by digest: every host rebuilds it and checks the roots on every start
/// (`Driver::recover`), the deployment's manifest is checked against it before the Agent is
/// founded, and `verify-genesis` reaches the same roots inside the PVM, chunk by chunk, off the
/// critical path.
#[derive(Clone, Serialize, Deserialize)]
pub struct StateGenesisEnvelope {
    /// The chainspec, by its digest and chunk count, as genesis commits it.
    pub chainspec: ledger_core::chainspec::SpecCommitment,
    /// The deployment's lifecycle configuration: the Agent's configuration row.
    pub lifecycle: ledger_core::lifecycle::LifecycleConfig,
    /// The Agent's bounds.
    pub limits: crate::hot::Limits,
    /// The genesis state's roots and counters.
    pub state: GenesisState,
    /// The code it is founded under.
    pub code: FoundingCode,
}

/// What the Agent's hot row holds after a genesis state (`StateGenesisEnvelope`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenesisState {
    /// The kernel's six sub-roots.
    pub subroots: [[u8; 32]; 6],
    /// The kernel store's last commit.
    pub last_commit: Option<u64>,
    /// The next posting sequence number.
    pub next_posting: u64,
    /// The service tree's root.
    pub service_root: [u8; 32],
    /// The audit chain: records emitted.
    pub audit_emitted: u64,
    /// The audit chain: the last record's hash.
    pub audit_head: [u8; 32],
    /// The audit chain: the last record the audit store acknowledged.
    pub audit_shipped: Option<(u64, [u8; 32])>,
    /// Notifications the outbox has taken.
    pub notified: u64,
}

impl GenesisState {
    /// The roots it stands at.
    pub fn roots(&self) -> Roots {
        Roots {
            kernel: crate::witness::root_of(&self.subroots),
            service: self.service_root,
            audit: audit_digest(self.audit_emitted, &self.audit_head),
        }
    }
}

/// One core input: the calls a native run of it made, in order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputEnvelope {
    /// Its input number: the Agent's last plus one.
    pub seq: u64,
    /// The library calls.
    pub calls: Vec<Call>,
    /// Deliveries acknowledged by their recipients: notification identity and time
    /// (`REQ-RT-4`, `REQ-EV-6`).
    pub deliveries: Vec<([u8; 32], Time)>,
    /// The delta ring's archive acknowledgement, piggybacked: `(through, log hash)`.
    pub archived: Option<(u64, [u8; 32])>,
    /// The kernel witness blob.
    pub witness: BlobKey,
    /// The service witness blob.
    pub service: BlobKey,
    /// The private-input blob.
    pub private: BlobKey,
}

/// The private inputs of one invocation (`REQ-RT-6`). Zeroised on drop; its `Debug` shows
/// nothing.
///
/// **No key material** (`F-035`, guardrail 13, `NFR-8`). The Agent once received the key-ring
/// root and the clerk key's secret here. It now receives only what its host asked the key
/// custody (the signer service) while it ran the input natively: public keys, signatures and
/// entry blindings ([`ledger_kernel::keys::Answers`]), and the clerk's signatures
/// ([`ClerkAnswers`]). The root and the clerk key stay in the signer's process.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Private {
    /// The custody's answers this input needs: encoded [`ledger_kernel::keys::KeyRequest`] to
    /// encoded [`ledger_kernel::keys::KeyAnswer`], in request order.
    pub keys: Vec<(Vec<u8>, Vec<u8>)>,
    /// The clerk's public key and the signatures this input needs, when it issues a disclosure
    /// (`F-017`).
    pub clerk: Option<ClerkAnswers>,
    /// The openings of every kernel account the input touches: vault row key (17 bytes) and
    /// its sums (96 bytes), as `ledger-kernel`'s vault encodes them.
    pub openings: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Private {
    /// The key ring the Agent runs the input over: the recorded answers, nothing else.
    pub fn key_ring(&self) -> ledger_kernel::keys::KeyRing {
        ledger_kernel::keys::KeyRing::with_custody(Box::new(ledger_kernel::keys::Answers::new(
            self.keys.iter().cloned(),
        )))
    }
}

/// The clerk's answers for one invocation (`F-035`): its public key, and each message it signed
/// with the signature, as the host's native run obtained them from the signer.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClerkAnswers {
    /// The clerk's public key.
    pub public: [u8; 32],
    /// Message, then its 64-byte signature, in message order.
    pub signatures: Vec<(Vec<u8>, Vec<u8>)>,
}

impl core::fmt::Debug for ClerkAnswers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClerkAnswers")
            .field("signatures", &self.signatures.len())
            .finish_non_exhaustive()
    }
}

impl ledger_core::anchor::clerk::ClerkSigner for ClerkAnswers {
    fn public_key(&self) -> [u8; 32] {
        self.public
    }

    /// The recorded signature over `msg`. A message the host did not have signed panics, which
    /// aborts the invocation: the Agent cannot sign, and never makes a signature up.
    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signatures
            .binary_search_by(|(m, _)| m.as_slice().cmp(msg))
            .ok()
            .and_then(|i| self.signatures[i].1.as_slice().try_into().ok())
            .unwrap_or_else(|| panic!("no clerk signature for this message"))
    }
}

impl core::fmt::Debug for Private {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Private(<redacted>)")
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        for (k, v) in self.keys.iter_mut().chain(self.openings.iter_mut()) {
            k.zeroize();
            v.zeroize();
        }
    }
}

/// What the Agent answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    /// The input committed, in one transition (`REQ-RT-1`).
    Committed(Committed),
    /// The input was applied before (`INV-18`): its entry is still in the delta ring.
    Duplicate {
        /// Its input number.
        seq: u64,
        /// Its log hash.
        log_hash: [u8; 32],
    },
    /// The ring was trimmed.
    Archived {
        /// Everything up to here is gone from the ring.
        through: u64,
    },
    /// Nothing committed.
    Refused(AgentRefusal),
    /// A batch's replies, one per input, in order (`T-031-02`): a committed (or duplicate) prefix,
    /// then at most one refusal, then [`AgentRefusal::Skipped`] for every input after it.
    Batch(Vec<Reply>),
}

/// A committed input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Committed {
    /// Its input number.
    pub seq: u64,
    /// What each call came to, in order: the named `ERR-*` for a refusal (`REQ-CS-4`).
    pub summaries: Vec<Summary>,
    /// The roots after.
    pub roots: Roots,
    /// The next posting sequence number.
    pub next_posting: u64,
    /// The delta-ring entry's log hash: the hash chain over every committed input.
    pub log_hash: [u8; 32],
    /// The Agent's state after, in bytes, against its budget.
    pub state_bytes: u64,
}

/// Why the Agent committed nothing. None of these is a library refusal: those commit (a refused
/// command is recorded, `F-007`) and come back in [`Committed::summaries`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRefusal {
    /// The invocation is not from the driver's principal (`T-005-03`).
    NotDriver,
    /// No genesis yet.
    NotFounded,
    /// Genesis already ran.
    AlreadyFounded,
    /// The input number is not the next one.
    OutOfOrder {
        /// The one the Agent expects.
        expected: u64,
    },
    /// The delta ring is full of entries the driver has not archived (`T-005-07`).
    RingFull,
    /// The outbox holds too many undelivered notifications (`T-005-07`, `DEC-7`).
    OutboxFull,
    /// The transition would take the Agent's state over its budget `H` (`T-005-02`).
    OverBudget {
        /// The state it would have.
        bytes: u64,
        /// The budget.
        budget: u64,
    },
    /// The witness does not prove the Agent's roots, or a write outside it (`T-005-04`).
    Witness(String),
    /// The library read a leaf the witness does not prove (`T-005-04`): the run is not the
    /// host's, and nothing commits.
    UnprovenRead,
    /// A call that cannot run inside the Agent.
    Unsupported(String),
    /// The message or a blob does not decode, or a blob is missing.
    Malformed(String),
    /// The library faulted (a binding fault, not a refusal).
    Fault(String),
    /// An earlier input of the batch was refused: nothing of this one ran (`T-031-02`).
    Skipped,
    /// The fence (`T-037-15`, `REQ-RT-20`, [`crate::fence`]): the code that received the input is
    /// not the code the writer's state names for it. Nothing is written and the input's number is
    /// not taken: the execution node installs the code the code map names, and sends the same
    /// input again. Appended last, so no earlier refusal's tag moves: a host built before it reads
    /// it as a reply that does not decode, never as another refusal. The continuation's scoped
    /// refusal shares it (`T-037-17`).
    Fenced(Fenced),
}

/// Why the fence refused an input (`T-037-15`, [`AgentRefusal::Fenced`]): what the refusing code
/// is, and what the writer's state names. The execution node audits it (its log and the audit
/// store), never the book.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fenced {
    /// The input refused: its number is not taken.
    pub seq: u64,
    /// The code generation compiled into the code that refused it.
    pub code_generation: u64,
    /// The code generation in force in the writer's state.
    pub generation: u64,
    /// The writer's scheduled upgrade, if any: the code version and the input it applies from.
    pub upgrade: Option<([u8; 32], u64)>,
    /// The named refusal of a scoped refusal of this kind, if any: none for the fence; the
    /// continuation's refusal of a way out (`T-037-17`, `ERR-InvalidTransition`) names its own.
    pub refusal: Option<String>,
}

/// Encode a value.
pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_allocvec(v).expect("postcard encodes every value of these types")
}

/// Decode a value; `Err` names what failed.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, String> {
    postcard::from_bytes(bytes).map_err(|e| alloc::format!("{e}"))
}
