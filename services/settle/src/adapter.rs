// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter itself: one [`Adapter::step`] after another.
//!
//! A step, in order:
//!
//! 1. **Lease.** Renew the single-active lease, or stop: a second instance on the same keys would
//!    be one nonce lane with two owners (Kreivo's runtime).
//! 2. **Status.** Re-run the start-up assertions (`REQ-SA-2`, `REQ-AS-2`) and declare the result
//!    to the core when it changed or half its lease has passed. While refusing, nothing else
//!    runs: a network that is not the declared one is not read, and nothing is signed for it.
//!    Among them (`F-027` T-027-07, T-027-12, `REQ-SA-9`): the runtime's **transaction surface**
//!    (its `transaction_version`, its transaction extensions and the digest of what a transaction
//!    carries, read from its metadata) selects the Kreivo transaction format every batch of the
//!    step is signed in, unless the operator names one for its `spec_version`; a surface with no
//!    format is refused, and so is a runtime on which the layer did not confirm the format. Its
//!    `spec_version` alone decides nothing. So a runtime upgrade the layer reports switches the
//!    format from the next batch on, and one that changes nothing a transaction carries keeps it.
//!    **The first transaction signed on a runtime not served before is validated by the runtime
//!    itself** (`TaggedTransactionQueue_validate_transaction`), at the block whose runtime it was
//!    signed for, before anything is broadcast (`Adapter::confirmed`); the runtime is recorded as
//!    served once it is valid, and nothing goes out on it before (`F-027` NOTES, decision 51).
//!    Then, while serving, **attest** each bound asset location (`DEC-20`): what the layer's
//!    metadata states about it at the finalized head, when it changed or half the status lease
//!    has passed. The core registers assets at those decimals and holds any asset whose location
//!    drifts; a held asset raises an alert here (`asset:<ASSET>`) until the core says it matches
//!    again.
//! 3. **Intake** (`REQ-SO-5`, `REQ-SO-6`, `INV-13`; `F-027` T-027-01). Read the core's call
//!    outbox: every external movement and anchor as an envelope — reference, target, call,
//!    expiry. Validate each withdrawal (its instruction's identity is recomputed from the envelope
//!    alone); store it; only then acknowledge it through ingress. The anchor in flight goes to
//!    step 8.
//! 4. **Track.** Decide each uncertain transaction at the finalized head, once for the whole
//!    batch it carries: included (its nonce is spent), its era ended with its nonce unspent, or
//!    still uncertain. A nonce stays reserved while its transaction is uncertain.
//! 5. **Observe** (`REQ-SA-7`, `REQ-IN-5`, `REQ-SI-1`–`SI-5`; amendment 0040). **Sync**: read the
//!    head of the contract's chain of entries at the finalized head; if it is the head last
//!    accepted, that is the only read. Otherwise fold the entries of the blocks since, rebuilt
//!    from their events, from the accepted digest; if they lead to the head, report each from the
//!    cursor with its canonical check and accept the head once the core took in all of them; if
//!    not, read once more, then raise `chain:<instance>:<position>` and report nothing past the
//!    first new position. Check what was reported provisionally; report the adapter's own
//!    extrinsics seen above finality as provisional, while one is in flight; report the finalised
//!    head **with the
//!    account nonce of every signing key, read at that same finalized block** (the owner's
//!    ruling of 2026-09-24: the core releases on non-inclusion only if the transaction's nonce
//!    agrees).
//! 6. **Submit** (`REQ-SA-6`, `REQ-SO-7`, `REQ-SL-4`, `REQ-SN-2`; `F-027` T-027-02). For each
//!    acknowledged or parked instruction: tombstone it if no era can end before its expiry; park
//!    it if the position or the fees do not cover it (one alert per position). The rest go out
//!    **batched**: each free lane takes up to 32 calls into one `execute` transaction of the
//!    settlement contract, **as many as its dry run says fit**, with the dry run's gas and half
//!    again (a call no good on its own is left unsubmitted for review, `F-027` T-027-09), signed
//!    as the deployment's community PASS account with an explicit nonce and an era **born at the
//!    finalized head** that ends before the earliest expiry among them (T-027-08). **Store the bytes (one
//!    row per operation, all or nothing), record every operation's key, nonce and era's last
//!    valid block with the core, then broadcast them**: nothing is broadcast that the core has
//!    not recorded in full, so an instruction the core holds as unsigned was never broadcast. A
//!    tombstone is recorded with the core too. Nothing new is signed in a step whose head the
//!    core did not take in: a nonce freed by a dead transaction is reused only after the core has
//!    seen the head that proves it dead. Broadcast every stored, uncertain transaction again:
//!    after a restart the same bytes go out, never a second transaction.
//! 7. (The periodic re-scan is gone, `T-043-05`: the sync shows completeness at every step.)
//! 8. **Anchor** (`F-017`, with an anchor key only). Decide the anchor key's transaction at the
//!    finalized head; take the checkpoint in flight from the call outbox; store it, then
//!    acknowledge it; unless the finalized deployment already holds it, sign the anchor call in a
//!    batch of its own on the anchor key, **store the bytes, then broadcast them**. An anchor moves
//!    no value and the contract takes a replay of its latest anchor as a no-op, so a transaction
//!    that provably did not store it (not included, failed, or the contract refused the call) is
//!    reported (`AnchorNotExecuted`, the core emits the anchor again) and a new one is signed.
//! 9. **Positions** (`F-017`, `REQ-PR-5`, with an anchor key only). Once the cursor has reached
//!    the finalized chain's head, report the instance's positions at the finalized head, when the
//!    core says a comparison is due or they changed.
//!
//! The adapter reports observations only. It never tells the core an outcome (`INV-11`), and
//! "unknown" produces no report at all (`REQ-SO-4`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use ingress::clock::SharedClock;
use ingress::settlement::{
    AcknowledgementBody, AnchorAcknowledgementBody, AnchorFactBody, AnchorFailureBody,
    AttestationBody, CallBody, CheckBody, EpochBody, EventKindBody, GapBody, GapReasonBody,
    HeadBody, KeyNonceBody, LayerAssetFactsBody, LayerAssetStatusBody, MovementBody,
    ObservationBody, ObservedFinalityBody, PositionBody, PositionsBody, PreviousAnchorBody,
    SettlementCallBody, SigningBody, SigningRecordBody, destination as parse_destination,
    venue_body,
};
use ingress::wire::{AdapterLegBody, CoordinateBody, OperationKindBody};
use ledger_core::account::Time;
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::id::OperationId;
use ledger_core::settlement::assets::LayerAssetStatus;
use ledger_core::settlement::envelope::{operation_of, operation_reference};
use ledger_core::settlement::{Destination, instruction_digest};
use serde::Serialize;

use crate::chain::kreivo;
use crate::chain::{
    AccountId32, AnchorCall, BatchCall, BatchFailure, BindCall, BlockEvents, BlockRef, Chain,
    ChainAsset, ChainError, ChainEvent, ChainHead, InboxEntry, InboxEvent, Mortality, RuntimeInfo,
    SignedExtrinsic, Validity, WithdrawCall, ZERO_DIGEST,
};
use crate::config::AdapterConfig;
use crate::link::{Answer, CoreLink, LinkError, Transport};
use crate::signer::Signer;
use crate::store::{
    AcceptedHead, AnchorExtrinsic, AnchorRow, BindRow, ExtrinsicRow, ExtrinsicStatus, GapRow,
    Inserted, InstructionRow, InstructionState, LaneRow, NotExecuted, ObservationRow, ParkReason,
    ReportRow, RuntimeRow, Store, StoreError,
};

/// The chain's operation reference of a book operation: the call envelope's reference, its 16
/// bytes then 16 zero bytes ([`operation_reference`]).
pub fn chain_ref(op: &OperationId) -> [u8; 32] {
    operation_reference(op)
}

/// The book operation a chain operation reference names, if it has the deployment's shape.
pub fn book_op(r: &[u8; 32]) -> Option<OperationId> {
    operation_of(r)
}

/// A point at which a test can make the adapter die (the restart matrix, `T-016-10`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CrashPoint {
    /// An instruction is stored; it was not acknowledged.
    AfterStore,
    /// An instruction is acknowledged to the core; its row does not say so yet.
    AfterAck,
    /// An extrinsic is signed; it was not stored.
    AfterSign,
    /// An extrinsic is stored; it was not broadcast.
    AfterPersist,
    /// An extrinsic is broadcast; its row does not say so yet.
    AfterBroadcast,
    /// An inbox entry is stored; it was not reported.
    AfterObserve,
    /// An observation was reported; its verdict was not stored.
    AfterReport,
    /// A verdict is stored; the cursor did not move.
    AfterVerdict,
}

/// Why a step stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterError {
    /// Another instance holds the lease.
    LeaseLost,
    /// The settlement layer did not answer.
    Chain(ChainError),
    /// Ingress did not answer.
    Link(LinkError),
    /// The store failed.
    Store(StoreError),
    /// A test's crash point was reached.
    Crashed(CrashPoint),
    /// An operator's request was refused, with why: nothing was written.
    Refused(String),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::LeaseLost => f.write_str("another instance holds the lease"),
            AdapterError::Chain(e) => e.fmt(f),
            AdapterError::Link(e) => e.fmt(f),
            AdapterError::Store(e) => e.fmt(f),
            AdapterError::Crashed(p) => write!(f, "crashed at {p:?}"),
            AdapterError::Refused(why) => write!(f, "refused: {why}"),
        }
    }
}

impl From<ChainError> for AdapterError {
    fn from(e: ChainError) -> Self {
        AdapterError::Chain(e)
    }
}

impl From<LinkError> for AdapterError {
    fn from(e: LinkError) -> Self {
        AdapterError::Link(e)
    }
}

impl From<StoreError> for AdapterError {
    fn from(e: StoreError) -> Self {
        AdapterError::Store(e)
    }
}

/// A step's result.
pub type StepResult<T> = Result<T, AdapterError>;

/// Why the adapter refuses to serve (`REQ-SA-2`, `REQ-AS-2`, `AC-E4.2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "assertion", rename_all = "snake_case")]
pub enum Refusing {
    /// The settlement layer could not be reached.
    Unreachable,
    /// The genesis hash is not the declared one.
    Genesis {
        /// What the network reports, hex.
        found: String,
    },
    /// The runtime's transaction surface is none the adapter has a Kreivo transaction format for,
    /// and the operator names no format for its `spec_version` (`T-027-12`, `REQ-SA-9`,
    /// `AC-E4.4`): nothing is signed for it rather than guessed.
    UnknownSurface {
        /// The runtime's `spec_version`.
        spec_version: u32,
        /// Its `transaction_version`.
        transaction_version: u32,
        /// The extensions its metadata names, in order.
        extensions: Vec<String>,
        /// The digest of the types of what a transaction carries, hex; empty when its metadata
        /// has no `Contracts::call` to read.
        types: String,
    },
    /// The layer did not confirm a transaction signed in the format chosen for the runtime: it
    /// found it invalid for what it carries (`T-027-12`, `REQ-SA-9`, `AC-E4.4`). Nothing more is
    /// signed for that runtime until the adapter restarts or the runtime changes.
    Unconfirmed {
        /// The runtime's `spec_version`.
        spec_version: u32,
        /// The format the transaction was signed in.
        format: String,
        /// The runtime's answer.
        error: String,
    },
    /// The settlement pallet is not in the metadata.
    NoPallet,
    /// The instance is not registered.
    NoInstance,
    /// A lane's key is not one of the instance's keys.
    NotAKey {
        /// The lane's account, hex.
        lane: String,
    },
}

/// What a step did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepReport {
    /// Whether the adapter serves.
    pub serving: bool,
    /// Whether it is held back by a reset or a start not taken (amendment 0041): it declares to
    /// the core that it does not serve, and goes on stepping.
    pub held: bool,
    /// Instructions acknowledged.
    pub acknowledged: usize,
    /// Extrinsics signed and stored.
    pub signed: usize,
    /// Extrinsics broadcast (first time or again).
    pub broadcast: usize,
    /// Instructions parked this step.
    pub parked: usize,
    /// Inbox entries reported.
    pub observed: usize,
    /// Gaps recorded.
    pub gaps: usize,
    /// The finalized head the step read (`NFR-15`: the step log places each phase of a
    /// withdrawal against the layer's heads). Zero when the step did not read one.
    pub finalized: u64,
    /// The best head the step read; zero when it did not read one.
    pub best: u64,
}

/// Test hooks. None of them is reachable from configuration.
#[derive(Clone, Debug, Default)]
pub struct Faults {
    crash: BTreeSet<CrashPoint>,
}

/// The adapter. See the module documentation.
pub struct Adapter<C: Chain, S: Store, T: Transport> {
    config: AdapterConfig,
    chain: C,
    store: S,
    link: CoreLink<T>,
    lanes: Vec<Arc<dyn Signer>>,
    clock: SharedClock,
    epoch: Option<u64>,
    serving: bool,
    /// Whether the last heartbeat found the adapter held ([`Adapter::held`]).
    held: bool,
    refusing: Vec<Refusing>,
    last_declared: Option<(Time, bool)>,
    steps: u64,
    faults: Faults,
    session: String,
    declarations: u64,
    anchor_signer: Option<Arc<dyn Signer>>,
    comparison_due: bool,
    last_cut: Option<(u64, Vec<PositionBody>)>,
    /// Until when dry runs are paused, and why: the client could not give one (decision 35).
    dry_run_pause: Option<(Time, String)>,
    /// The last attestation sent for each bound location, and when (`DEC-20`).
    attested: BTreeMap<String, (Time, AttestationBody)>,
    /// The mapped assets the core said it does not hold (a book-only asset registered later,
    /// F-029): left out of the support declared, which the core would refuse whole for them.
    unregistered: std::collections::BTreeSet<String>,
    /// The support last declared, so a change of it is declared at once.
    declared_support: Vec<AdapterLegBody>,
    /// What the last step read (`NFR-15`): the heads and the calls waiting for intake, against
    /// which [`Adapter::news`] says whether a step is due before the interval ends.
    last_seen: Option<Seen>,
    /// The runtimes on which the layer refused a transaction signed in the format chosen for
    /// them (`T-027-12`), by [`Chosen::key`]: refused until the process restarts.
    refused: BTreeMap<String, Refusing>,
    /// Consecutive "not now" answers to the first transaction on each runtime not yet confirmed,
    /// by [`Chosen::key`] (`F-027` NOTES, decision 52): at
    /// [`AdapterConfig::not_now_alert_after`] the runtime's alert is raised. Held in memory: a
    /// restart counts again; an alert already raised is in the store, and stays open until a
    /// runtime is confirmed.
    not_now: BTreeMap<String, u32>,
    /// The confirmed runtimes, by [`Chosen::key`], whose `runtime:<spec_version>` alert this
    /// process has made sure is cleared (decision 52): a confirmation stored before its alert was
    /// cleared (a store error, a crash in between) would otherwise leave the alert open, paging,
    /// since every later step finds the runtime confirmed and never reaches the clearing.
    alert_settled: BTreeSet<String>,
    /// Steps in a row that ended because a key's signer did not sign (`F-045`), and the last reason.
    signer_failures: (u32, String),
    /// Whether the `signer:engines` alert, as this process last wrote it while holding the lease, is
    /// open; `None` before it has written it.
    signer_alert: Option<bool>,
}

/// The alert of a chain that does not lead to its head (amendment 0040, `REQ-IN-5`):
/// `chain:<instance>:<position>`, of kind `chain`, which pages as
/// `AledgentSettlementChainMismatch` (`aledgent_observation_chain_mismatches`).
pub const CHAIN_ALERT: &str = "chain";

/// The alert of a new sequence the core refused to follow (amendment 0041, `REQ-SA-10` as
/// proposed): `epoch:<instance>:<epoch>`, of kind `epoch`, which pages as
/// `AledgentSettlementEpochRefused`. Nothing of the new chain is reported while it is open.
pub const EPOCH_ALERT: &str = "epoch";

/// The alert of a new sequence a core before amendment 0041's generation cannot take:
/// `epoch-unsupported:<instance>:<epoch>`, of kind `epoch-unsupported`, which does not page. The
/// adapter goes on as before (the core's head stays where it is, and its guard refuses
/// withdrawals once it has one); the start is reported again once the core is upgraded.
pub const EPOCH_UNSUPPORTED_ALERT: &str = "epoch-unsupported";

/// How long a refused start waits before it is reported again (amendment 0041): ten minutes.
pub const EPOCH_RETRY: Time = 600_000;

/// The core's answer to a new sequence (amendment 0041), as the adapter keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EpochAnswer {
    /// Started, or the same sequence recorded already: the core follows it.
    Confirmed,
    /// Refused by the core, with its reason (`other_book`, `in_flight`, `earlier_final`,
    /// `not_later`).
    Refused(String),
    /// Not taken: the core cannot take the input (a code before the amendment's generation), with
    /// its refusal code.
    Unsupported(String),
}

/// Why the adapter stopped at a reset (`reset:<instance>`), kept so the retry follows only what
/// the core may still lift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResetReason {
    /// Entries of the chain before it past the verified head were never read.
    Dropped,
    /// The chain before it could not be read up to the reset.
    Unread,
    /// Verified entries the core has not taken in yet.
    Unreported,
    /// The core did not take the start (refused, cannot take it, or not asked).
    NotFollowed,
    /// The reset names another book.
    OtherBook,
}

impl ResetReason {
    fn as_str(self) -> &'static str {
        match self {
            ResetReason::Dropped => "dropped",
            ResetReason::Unread => "unread",
            ResetReason::Unreported => "unreported",
            ResetReason::NotFollowed => "not_followed",
            ResetReason::OtherBook => "other_book",
        }
    }

    fn of(s: &str) -> Option<ResetReason> {
        Some(match s {
            "dropped" => ResetReason::Dropped,
            "unread" => ResetReason::Unread,
            "unreported" => ResetReason::Unreported,
            "not_followed" => ResetReason::NotFollowed,
            "other_book" => ResetReason::OtherBook,
            _ => return None,
        })
    }
}

fn reset_reason_key(instance: u32) -> String {
    format!("settle/reset/{instance}")
}

/// The one refusal code read as "the core cannot take the input yet" (amendment 0041).
const UNSUPPORTED_CODE: &str = "ERR-MalformedCommand";

impl EpochAnswer {
    /// The answer a stored verdict records ([`epoch_verdict`]).
    fn of(verdict: &str) -> EpochAnswer {
        match verdict {
            "started" | "recorded" => EpochAnswer::Confirmed,
            v => match v.strip_prefix("refused:") {
                // What a core before the amendment's generation, or ingress with the route closed,
                // answers: the input cannot be taken yet. Any other code (the adapter's authority
                // lost, a limit) is a refusal, and holds the new chain back.
                Some(UNSUPPORTED_CODE) => EpochAnswer::Unsupported(UNSUPPORTED_CODE.into()),
                Some(reason) => EpochAnswer::Refused(reason.into()),
                None => EpochAnswer::Unsupported(v.into()),
            },
        }
    }
}

/// The verdict stored for the core's answer to a new sequence: `started`, `recorded`,
/// `refused:<reason>` for the core's refusal, or `refused:<ERR-code>` for a command the core did
/// not take.
/// What the core's refusal names, for the alert: the operation in flight, or the position of the
/// earlier sequence's final record.
fn epoch_refusal_detail(a: &Answer) -> String {
    match a {
        Answer::Decided { result, .. } => match &result.epoch {
            Some(e) => match (&e.operation_reference, e.seq) {
                (Some(op), _) => format!(" (operation {op} is in flight: resolve it first)"),
                (None, Some(seq)) => format!(
                    " (the book holds a final record of the chain before at position {seq})"
                ),
                _ => String::new(),
            },
            None => String::new(),
        },
        Answer::Refused(_) => String::new(),
    }
}

fn epoch_verdict(a: &Answer) -> String {
    match a {
        Answer::Decided { result, .. } => match &result.epoch {
            Some(e) if e.outcome == "refused" => {
                format!("refused:{}", e.refusal.as_deref().unwrap_or("unknown"))
            }
            Some(e) => e.outcome.clone(),
            None => "refused:unanswered".into(),
        },
        Answer::Refused(code) => format!("refused:{code}"),
    }
}

/// The report row that keeps the core's answer to the latest sequence of `instance`.
/// The prefixes of the instance's epoch alerts: a refused start (`epoch:`) and one the core
/// cannot take (`epoch-unsupported:`).
fn epoch_alert_prefixes(instance: u32) -> [String; 2] {
    [
        format!("{EPOCH_ALERT}:{instance}:"),
        format!("{EPOCH_UNSUPPORTED_ALERT}:{instance}:"),
    ]
}

fn epoch_key(instance: u32) -> String {
    format!("settle/epoch/{instance}")
}

/// Where a walk back from the finalized head stops (amendment 0040).
#[derive(Clone, Copy, Debug)]
enum WalkTo {
    /// At the blocks after this one: the block the accepted head was read at.
    After(u64),
    /// At the chain's start: the block of the reset that began it, or of its entry at this
    /// position ([`ChainHead::start`]), or the genesis.
    Start(u64),
}

/// What a walk read, in the blocks' order.
#[derive(Debug, Default)]
struct Walked {
    /// The current chain's entries.
    entries: Vec<(u64, InboxEntry)>,
    /// The block of the last reset read, if any.
    reset: Option<u64>,
    /// How many entries a reset read cleared: entries of a chain before it, past the head the
    /// adapter verified, and so never reported (amendment 0041).
    dropped: usize,
    /// The positions of events in the earlier code's shape.
    legacy: Vec<u64>,
}

/// What a sync found.
enum Synced {
    /// The head is the one accepted.
    Idle,
    /// The entries since the accepted head, in order: they lead to the head.
    New {
        /// The entries.
        entries: Vec<(u64, InboxEntry)>,
    },
}

/// Why a sync stopped at the first new position.
enum Stop {
    /// A block's events could not be read, by the adapter's client or the history endpoint.
    Unreadable(String),
    /// The events do not lead to the head, read twice.
    Mismatch(String),
    /// The instance was reset since the accepted head, at this block, with this many entries of
    /// the chain before it past the verified head, and this many verified that the core has not
    /// taken in (amendment 0041: none of either, or the reset is not followed).
    Reset(u64, usize, u64),
    /// The anchor sequence moved past the verified head's, and the range since cannot be read: a
    /// reset at this block whose chain before could not be read up to it (the delta review's
    /// finding 1). Never followed by the adapter; the owner re-founds.
    ResetUnread(u64, String),
}

/// The format the adapter signs in on a runtime, and that runtime's identity (`T-027-12`).
#[derive(Clone, Debug)]
struct Chosen {
    /// What a transaction is signed for.
    signing: kreivo::Signing,
    /// The runtime's surface.
    surface: kreivo::Surface,
    /// The runtime, as its confirmation is recorded: `spec_version`, the surface's fingerprint
    /// and [`Chosen::format_name`].
    key: String,
    /// The format's name as the confirmation records it: the format's own for PASS transactions,
    /// [`kreivo::TxFormat::native_name`] for a native account's (`F-045`), so that a runtime confirmed
    /// for one form is not taken as confirmed for the other.
    format_name: String,
    /// The block the runtime was read at ([`RuntimeInfo::at`]): what is signed for it is
    /// validated there (`F-027` NOTES, decision 51).
    at: BlockRef,
}

/// What a step read that the next one would act on: the finalized and best heads' hashes, and the
/// references of the calls the outbox holds unacknowledged.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    finalized: [u8; 32],
    best: [u8; 32],
    waiting: BTreeSet<String>,
}

/// The references of the calls waiting for intake.
fn waiting(calls: &[CallBody]) -> BTreeSet<String> {
    calls
        .iter()
        .filter(|c| !c.acknowledged)
        .map(|c| c.reference.clone())
        .collect()
}

/// The alert raised while no dry run can be had, and so nothing is signed.
const DRY_RUN_ALERT: &str = "dry-run";

/// How long dry runs pause after the client could not give one, in milliseconds: longer than the
/// light client's longest ban of a peer (40 s).
const DRY_RUN_PAUSE: Time = 60_000;

/// The least a batch's gas limit exceeds its dry run by: 2 000 000 000 of ref time and 92 KiB of
/// proof size, what the contract's per-call isolation (#102) may use above what a dry run reports.
const BATCH_GAS_FLOOR: kreivo::Weight = kreivo::Weight {
    ref_time: 2_000_000_000,
    proof_size: 92 * 1024,
};

/// What [`Adapter::fit_batch`] made of a lane's candidates.
struct Fitted {
    /// The calls to sign, in order; empty for none.
    batch: Vec<InstructionRow>,
    /// Their limits, from their dry run ([`Adapter::batch_limits`]).
    limits: kreivo::CallLimits,
    /// What goes back, in order, for another lane or the next step.
    rest: Vec<InstructionRow>,
    /// What no batch can carry, with why.
    held: Vec<(InstructionRow, String)>,
}

/// A value that differs between two processes of the adapter: the declarations' client keys.
fn session() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static STARTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = STARTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut b = nanos.to_le_bytes().to_vec();
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&std::process::id().to_le_bytes());
    hex::encode(&crate::chain::blake2_256(&b)[..8])
}

/// A binding in the call outbox, as the executor reads it.
struct BindAsked {
    reference: String,
    asset: String,
    location: String,
    amount: u128,
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    hex::decode(s).ok().and_then(|v| v.try_into().ok())
}

fn kind_body(e: &InboxEvent) -> EventKindBody {
    match e {
        InboxEvent::Deposited { .. } => EventKindBody::Deposited,
        InboxEvent::Withdrawn { .. } => EventKindBody::Withdrawn,
        InboxEvent::WithdrawRefused { .. } => EventKindBody::WithdrawRefused,
        InboxEvent::Anchored { .. } => EventKindBody::Anchored,
        InboxEvent::AnchorRefused { .. } => EventKindBody::AnchorRefused,
        InboxEvent::Bound { .. } => EventKindBody::Bound,
        InboxEvent::BindRefused { .. } => EventKindBody::BindRefused,
    }
}

fn summary(e: &InboxEvent) -> String {
    match e {
        InboxEvent::Deposited { asset, amount, .. } => format!("deposited {amount} {}", asset.0),
        InboxEvent::Withdrawn { asset, amount, .. } => format!("withdrawn {amount} {}", asset.0),
        InboxEvent::WithdrawRefused {
            asset,
            amount,
            error,
            ..
        } => format!("withdraw of {amount} {} refused: {error}", asset.0),
        InboxEvent::Anchored { number, .. } => format!("anchor {number}"),
        InboxEvent::AnchorRefused { number, error, .. } => {
            format!("anchor {number} refused: {error}")
        }
        InboxEvent::Bound { asset, amount, .. } => format!("bound {} at {amount}", asset.0),
        InboxEvent::BindRefused {
            asset,
            amount,
            error,
            ..
        } => format!("bind of {} at {amount} refused: {error}", asset.0),
    }
}

/// The alert a settlement key that does not sign raises (`F-045`): kind `signer` in the signals.
pub const SIGNER_ALERT: &str = "signer:engines";

/// After how many steps in a row ended because a key did not sign [`SIGNER_ALERT`] is raised
/// (`F-045`): about 6 s of the deployment's 2 s steps, past a blip.
pub const SIGNER_FAILURES_ALERT_AFTER: u32 = 3;

/// Whether `r` ended because a key's signer did not sign (`F-045`).
fn signer_failed<T>(r: &StepResult<T>) -> bool {
    matches!(r, Err(AdapterError::Chain(ChainError::Signer(_))))
}

impl<C: Chain, S: Store, T: Transport> Adapter<C, S, T> {
    /// An adapter over `chain`, `store` and `transport`, signing with `lanes`. It does nothing
    /// until [`Adapter::step`].
    pub fn new(
        config: AdapterConfig,
        chain: C,
        store: S,
        transport: T,
        lanes: Vec<Arc<dyn Signer>>,
        clock: SharedClock,
    ) -> Adapter<C, S, T> {
        Adapter {
            config,
            chain,
            store,
            link: CoreLink::new(transport),
            lanes,
            clock,
            epoch: None,
            serving: false,
            held: false,
            refusing: Vec::new(),
            last_declared: None,
            steps: 0,
            faults: Faults::default(),
            session: session(),
            declarations: 0,
            anchor_signer: None,
            comparison_due: false,
            last_cut: None,
            dry_run_pause: None,
            attested: BTreeMap::new(),
            unregistered: std::collections::BTreeSet::new(),
            declared_support: Vec::new(),
            last_seen: None,
            refused: BTreeMap::new(),
            not_now: BTreeMap::new(),
            alert_settled: BTreeSet::new(),
            signer_failures: (0, String::new()),
            signer_alert: None,
        }
    }

    /// Publish anchors with `signer` (`F-017`): one of the instance's keys, used for nothing
    /// else, so its nonces are the anchors' alone.
    pub fn with_anchor_signer(mut self, signer: Arc<dyn Signer>) -> Self {
        self.anchor_signer = Some(signer);
        self
    }

    /// Its configuration.
    pub fn config(&self) -> &AdapterConfig {
        &self.config
    }

    /// Its store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Its settlement layer.
    pub fn chain(&self) -> &C {
        &self.chain
    }

    /// Its lanes' signers (public keys only: a signer never shows its secret).
    pub fn lanes(&self) -> &[Arc<dyn Signer>] {
        &self.lanes
    }

    /// Whether it serves, and why not.
    pub fn status(&self) -> (bool, &[Refusing]) {
        (self.serving, &self.refusing)
    }

    /// Whether a step is due before the interval ends (`NFR-15`): while serving, the finalized or
    /// the best head moved since the last step, or a call arrived in the outbox that the last step
    /// did not see. A step's own work is unchanged; this only says when there
    /// is something new for it, so a withdrawal is signed as soon as it is in the outbox and
    /// observed as soon as the layer's client has the block, not up to an interval later.
    ///
    /// Two head reads from the layer's client and one read of the outbox: no proof, no dry run.
    /// Anything that cannot be read is no news (the interval's step handles it).
    pub async fn news(&self) -> bool {
        if !self.serving {
            return false;
        }
        let Some(last) = &self.last_seen else {
            return true;
        };
        match (self.chain.finalized().await, self.chain.best().await) {
            (Ok(fin), Ok(best)) if fin.hash != last.finalized || best.hash != last.best => {
                return true;
            }
            (Ok(_), Ok(_)) => {}
            _ => return false,
        }
        match self.link.calls().await {
            // A call that arrived since: one the last step saw and could not take in waits for
            // the interval, as before.
            Ok(Ok(calls)) => waiting(&calls).difference(&last.waiting).next().is_some(),
            _ => false,
        }
    }

    /// Die at `point` the next time it is reached (tests).
    pub fn crash_at(&mut self, point: CrashPoint) {
        self.faults.crash.insert(point);
    }

    fn fault(&mut self, point: CrashPoint) -> StepResult<()> {
        if self.faults.crash.remove(&point) {
            return Err(AdapterError::Crashed(point));
        }
        Ok(())
    }

    fn now(&self) -> Time {
        self.clock.now()
    }

    fn lane_key(signer: &dyn Signer) -> String {
        hex::encode(signer.account())
    }

    // ------------------------------------------------------------------------------ lease

    /// Report the signers' health after a step (`F-045`): the alert [`SIGNER_ALERT`] is open while an
    /// engine is not ready (`engines`: ready, and why not), or once [`SIGNER_FAILURES_ALERT_AFTER`]
    /// steps in a row ended because a key did not sign (`stepped`: a Vault that answers but refuses
    /// `sign/` leaves the engine ready, and only this shows it); a step that ends otherwise resets the
    /// count. The alert is written only on a change, and **only while this process holds the lease**,
    /// so a standby never writes over the active instance's alerts. Returns whether it is open, or
    /// `None` when nothing was written.
    pub async fn report_signers(
        &mut self,
        engines: (bool, &str),
        stepped: &StepResult<StepReport>,
    ) -> StepResult<Option<bool>> {
        match stepped {
            Err(AdapterError::Chain(ChainError::Signer(e))) => {
                self.signer_failures = (self.signer_failures.0.saturating_add(1), e.clone());
            }
            Ok(_) => self.signer_failures = (0, String::new()),
            Err(_) => {}
        }
        if self.epoch.is_none() || matches!(stepped, Err(AdapterError::LeaseLost)) {
            // Another instance holds it: its alerts are its own. Written afresh once it is ours.
            self.signer_alert = None;
            return Ok(None);
        }
        let (n, why) = &self.signer_failures;
        let detail = if !engines.0 {
            Some(format!(
                "a signing key's engine is not ready: {}; nothing is signed until it is",
                engines.1
            ))
        } else if *n >= SIGNER_FAILURES_ALERT_AFTER {
            Some(format!("{n} steps in a row could not sign: {why}"))
        } else {
            None
        };
        let open = detail.is_some();
        if self.signer_alert == Some(open) {
            return Ok(None);
        }
        let now = self.now();
        match &detail {
            Some(d) => {
                self.store.raise(SIGNER_ALERT, d, now).await?;
                tracing::error!(detail = %d, "a settlement key does not sign");
            }
            None => {
                if self.store.clear(SIGNER_ALERT, now).await? {
                    tracing::info!("the settlement keys sign again");
                }
            }
        }
        self.signer_alert = Some(open);
        Ok(Some(open))
    }

    async fn hold_lease(&mut self) -> StepResult<u64> {
        let now = self.now();
        let ttl = self.config.lease_ttl;
        if let Some(epoch) = self.epoch {
            if self
                .store
                .renew(&self.config.holder, epoch, now, ttl)
                .await?
            {
                return Ok(epoch);
            }
            self.epoch = None;
            return Err(AdapterError::LeaseLost);
        }
        match self.store.acquire(&self.config.holder, now, ttl).await? {
            Some(l) => {
                self.epoch = Some(l.epoch);
                Ok(l.epoch)
            }
            None => Err(AdapterError::LeaseLost),
        }
    }

    // ------------------------------------------------------------------------------ status

    /// Run the start-up assertions (`REQ-SA-2`, `REQ-AS-2`): every one that fails.
    pub async fn assertions(&self) -> Vec<Refusing> {
        let mut out = Vec::new();
        // Which read failed, and how: the operator's first question about an unreachable layer.
        let unreachable = |what: &str, e: &ChainError| {
            tracing::warn!(read = what, error = %e, "the settlement layer did not answer");
            vec![Refusing::Unreachable]
        };
        let runtime = match self.chain.runtime().await {
            Ok(r) => r,
            Err(e) => return unreachable("runtime", &e),
        };
        if Some(runtime.genesis) != self.config.genesis_hash() {
            out.push(Refusing::Genesis {
                found: hex::encode(runtime.genesis),
            });
            // Nothing else on a network that is not the declared one is worth reading.
            return out;
        }
        // T-027-12 (REQ-SA-9): the transaction format follows the runtime's surface, checked on
        // every step, so a runtime upgrade the layer reports switches the format of the next batch
        // signed, and one that changes nothing a transaction carries keeps it. The `spec_version`
        // gates nothing.
        if let Err(r) = self.chosen(&runtime) {
            out.push(r);
        }
        if !runtime.pallet {
            out.push(Refusing::NoPallet);
            return out;
        }
        let fin = match self.chain.finalized().await {
            Ok(fin) => fin,
            Err(e) => return unreachable("finalized head", &e),
        };
        match self.chain.instance(&fin, self.config.instance).await {
            Err(e) => return unreachable("instance", &e),
            Ok(None) => out.push(Refusing::NoInstance),
            Ok(Some(i)) => {
                for l in self.lanes.iter().chain(self.anchor_signer.iter()) {
                    if !i.keys.contains(&l.account()) {
                        out.push(Refusing::NotAKey {
                            lane: Self::lane_key(l.as_ref()),
                        });
                    }
                }
            }
        }
        // DEC-20: an asset's decimals are no longer asserted here against a precision copied into
        // this configuration. The adapter attests each location's metadata to the core
        // (`Adapter::attest_assets`), which checks it against the registry, asset by asset.
        out
    }

    /// The format the adapter signs in on `runtime` (`T-027-12`, `REQ-SA-9`): the one the
    /// operator names for its `spec_version` (`formats`), else the one whose surface is the
    /// runtime's ([`kreivo::TxFormat::by_surface`]). Refused if there is none, or if the layer
    /// already refused a transaction signed in it on this runtime.
    fn chosen(&self, runtime: &RuntimeInfo) -> Result<Chosen, Refusing> {
        let named = self
            .config
            .formats
            .get(&runtime.spec_version)
            .and_then(|name| kreivo::TxFormat::named(name));
        let Some(format) = named.or_else(|| kreivo::TxFormat::by_surface(&runtime.surface)) else {
            return Err(Refusing::UnknownSurface {
                spec_version: runtime.spec_version,
                transaction_version: runtime.surface.transaction_version,
                extensions: runtime.surface.extensions.clone(),
                types: runtime.surface.types.map(hex::encode).unwrap_or_default(),
            });
        };
        let format_name = if self.native() {
            format.native_name()
        } else {
            format.name.to_owned()
        };
        let key = format!(
            "{}:{}:{}",
            runtime.spec_version,
            hex::encode(runtime.surface.fingerprint()),
            format_name
        );
        if let Some(r) = self.refused.get(&key) {
            return Err(r.clone());
        }
        Ok(Chosen {
            signing: kreivo::Signing {
                spec_version: runtime.spec_version,
                transaction_version: runtime.surface.transaction_version,
                format,
            },
            surface: runtime.surface.clone(),
            key,
            format_name,
            at: runtime.at,
        })
    }

    /// Whether the adapter signs as native accounts (`F-045`): its keys are no PASS account's
    /// devices. The configuration gives every lane and the anchor key one form, so the first lane
    /// says it for all.
    fn native(&self) -> bool {
        self.lanes
            .iter()
            .chain(self.anchor_signer.iter())
            .next()
            .is_some_and(|l| l.pass_user().is_none())
    }

    /// Whether `ext`, just signed for `chosen`, may be broadcast (`T-027-12`, `REQ-SA-9`): **the
    /// layer's confirmation before the first transaction signed under rules not served before**.
    ///
    /// On a runtime the store records as served, yes. Otherwise the runtime validates `ext`, as a
    /// transaction from the network, without including it, **at the block whose runtime `ext` was
    /// signed for** (`chosen.at`, `F-027` NOTES decision 51): never at a head read before or after
    /// that runtime was, which an upgrade between the two reads would make another runtime.
    ///
    /// - **valid**: the runtime is recorded as served (it survives a restart) and `ext` goes out;
    /// - **invalid for what it carries** (a proof or signer that does not verify, an origin no
    ///   extension authorised, bytes it cannot decode): nothing is broadcast and the adapter
    ///   refuses to serve on that runtime ([`Refusing::Unconfirmed`], an alert
    ///   `runtime:<spec_version>`), once it has checked that the runtime at `chosen.at` is the one
    ///   `ext` was signed for. If it is not, the refusal is of a transaction signed for another
    ///   runtime and says nothing of the format: nothing is broadcast, nothing refused, and the
    ///   next step signs again;
    /// - **anything else** (invalid for now: a nonce, fees, the era, the block's resources; or an
    ///   answer it cannot class): neither a confirmation nor a refusal, so nothing is broadcast
    ///   (`REQ-SA-9`: no service until the layer confirms). Nothing is stored and no nonce moves;
    ///   the calls stay acknowledged and unsigned, and the next step rebuilds the transaction and
    ///   asks again. After [`AdapterConfig::not_now_alert_after`] such answers in a row on the
    ///   runtime, its `runtime:<spec_version>` alert is raised, naming the last answer (`F-027`
    ///   NOTES, decision 52); the runtime's confirmation clears it.
    ///
    /// `false` means `ext` is dropped: the caller stores nothing and puts its calls back.
    async fn confirmed(&mut self, chosen: &Chosen, ext: &SignedExtrinsic) -> StepResult<bool> {
        if self
            .store
            .runtimes()
            .await?
            .iter()
            .any(|r| r.key == chosen.key)
        {
            // Once a process: an alert left open by a confirmation that was stored before its
            // alert was cleared is cleared now (unless this process refused that runtime).
            if !self.alert_settled.contains(&chosen.key) {
                let alert = format!("runtime:{}", chosen.signing.spec_version);
                let refused = self.refused.values().any(|r| {
                    matches!(r, Refusing::Unconfirmed { spec_version, .. }
                        if *spec_version == chosen.signing.spec_version)
                });
                if !refused
                    && self
                        .store
                        .alerts()
                        .await?
                        .iter()
                        .any(|a| a.key == alert && a.cleared_at.is_none())
                {
                    self.store.clear(&alert, self.now()).await?;
                }
                self.alert_settled.insert(chosen.key.clone());
            }
            return Ok(true);
        }
        let now = self.now();
        let spec_version = chosen.signing.spec_version;
        let format = chosen.format_name.as_str();
        let alert = format!("runtime:{spec_version}");
        match self.chain.validate(&chosen.at, ext).await? {
            Validity::Valid => {
                self.store
                    .confirm_runtime(&RuntimeRow {
                        key: chosen.key.clone(),
                        spec_version,
                        transaction_version: chosen.surface.transaction_version,
                        extensions: chosen.surface.extensions.clone(),
                        types: chosen.surface.types.map(hex::encode).unwrap_or_default(),
                        format: format.to_owned(),
                        extrinsic: hex::encode(ext.hash),
                        confirmed_at: now,
                    })
                    .await?;
                // Decision 52: the runtime's alert clears, and so does any other runtime's this
                // process has not refused (one a "not now" held, upgraded away from since, maybe
                // before a restart): nothing waits on it any more.
                let refused: BTreeSet<String> = self
                    .refused
                    .values()
                    .filter_map(|r| match r {
                        Refusing::Unconfirmed { spec_version, .. } => {
                            Some(format!("runtime:{spec_version}"))
                        }
                        _ => None,
                    })
                    .collect();
                for open in self.store.alerts().await? {
                    if open.cleared_at.is_none()
                        && open.key.starts_with("runtime:")
                        && (open.key == alert || !refused.contains(&open.key))
                    {
                        self.store.clear(&open.key, now).await?;
                    }
                }
                self.not_now.clear();
                self.alert_settled.insert(chosen.key.clone());
                tracing::info!(
                    spec_version,
                    format,
                    hash = %hex::encode(ext.hash),
                    "the settlement layer confirmed the transaction format on its runtime"
                );
                Ok(true)
            }
            Validity::NotNow(why) => {
                let answers = self.not_now.entry(chosen.key.clone()).or_insert(0);
                *answers = answers.saturating_add(1);
                let answers = *answers;
                tracing::warn!(
                    spec_version,
                    format,
                    why = %why,
                    answers,
                    "the settlement layer's runtime neither confirmed nor refused the format; \
                     nothing is broadcast, and the next step signs and asks again"
                );
                // Decision 52: a "not now" that persists holds every withdrawal on the runtime
                // until it ends as never signed, long before the oldest-operation page could
                // fire. From the threshold on, the runtime's alert is open, and it pages.
                if answers >= self.config.not_now_alert_after.max(1)
                    && self
                        .store
                        .raise(
                            &alert,
                            &format!(
                                "the settlement layer answered \"not now\" {answers} times in a \
                                 row to a transaction signed in format {format} on spec_version \
                                 {spec_version}, last: {why}; nothing is broadcast on it until it \
                                 confirms"
                            ),
                            now,
                        )
                        .await?
                {
                    tracing::error!(
                        spec_version,
                        format,
                        why = %why,
                        answers,
                        "the settlement layer has not confirmed the runtime after repeated \
                         \"not now\" answers; withdrawals on it wait unsigned"
                    );
                }
                Ok(false)
            }
            Validity::Refused(why) => {
                // Decision 51: a refusal is of the format only if the runtime that validated
                // `ext` is the one it was signed for.
                let validated = self.chain.version(&chosen.at).await?;
                if (validated.spec_version, validated.transaction_version)
                    != (spec_version, chosen.signing.transaction_version)
                {
                    tracing::warn!(
                        spec_version,
                        format,
                        validated_spec_version = validated.spec_version,
                        at = chosen.at.number,
                        why = %why,
                        "a transaction was validated by another runtime than the one it was \
                         signed for, and refused; nothing is broadcast or refused, and the next \
                         step signs again"
                    );
                    return Ok(false);
                }
                let refusing = Refusing::Unconfirmed {
                    spec_version,
                    format: format.to_owned(),
                    error: why.clone(),
                };
                // An alert the "not now" answers raised would keep their detail: the refusal's
                // replaces it.
                if self.not_now.remove(&chosen.key).is_some() {
                    self.store.clear(&alert, now).await?;
                }
                self.store
                    .raise(
                        &alert,
                        &format!(
                            "the settlement layer refused a transaction signed in format {format} \
                             on spec_version {spec_version}: {why}; nothing is signed for it"
                        ),
                        now,
                    )
                    .await?;
                tracing::error!(
                    spec_version,
                    format,
                    why = %why,
                    "the settlement layer did not confirm the transaction format on its runtime; \
                     refusing to serve"
                );
                self.refused.insert(chosen.key.clone(), refusing.clone());
                self.serving = false;
                self.refusing.push(refusing);
                Ok(false)
            }
        }
    }

    fn support(&self) -> Vec<AdapterLegBody> {
        let mut legs = Vec::new();
        for b in self
            .config
            .assets
            .iter()
            .filter(|b| !self.unregistered.contains(&b.asset))
        {
            for kind in [OperationKindBody::Redeem, OperationKindBody::Issue] {
                legs.push(AdapterLegBody {
                    kind,
                    asset: b.asset.clone(),
                });
            }
        }
        legs
    }

    /// Re-run the assertions and declare the status when it changed or half its lease passed.
    async fn heartbeat(&mut self) -> StepResult<()> {
        let refusing = self.assertions().await;
        let serving = refusing.is_empty();
        let now = self.now();
        // Amendment 0041: while a reset or a start not taken holds the adapter back, it keeps
        // running but declares that it does not serve, so the core refuses withdrawals rather than
        // build one from a head the adapter stopped sending.
        let held = self.held().await?;
        let declared = serving && !held;
        let due = match self.last_declared {
            None => true,
            Some((at, was)) => {
                was != declared || now.saturating_sub(at) >= self.config.status_lease / 2
            }
        };
        if !serving {
            let detail = serde_json::to_string(&refusing).unwrap_or_default();
            if self.store.raise("refusing", &detail, now).await? {
                tracing::error!(detail = %detail, "the adapter refuses to serve");
            }
        } else {
            self.store.clear("refusing", now).await?;
        }
        self.serving = serving;
        self.refusing = refusing;
        self.held = held;
        if serving {
            self.attest_assets().await?;
        }
        let support_changed = declared && self.support() != self.declared_support;
        if !due && !support_changed {
            return Ok(());
        }
        // Every declaration is a new request: a repeat under an old key would be answered with
        // the old declaration and never reach the core.
        self.declarations += 1;
        let key = format!(
            "settle/declare/{}/{now}/{}/{declared}",
            self.session, self.declarations
        );
        let support = declared.then(|| self.support());
        let body = serde_json::json!({
            "serving": declared, "lease": self.config.status_lease, "support": support
        });
        self.report_start(&key, "declare", body).await?;
        let a = self
            .link
            .declare(&key, declared, self.config.status_lease, support.clone())
            .await?;
        self.report_end(&key, &a).await?;
        if let Answer::Refused(code) = &a {
            self.store
                .raise(
                    "declare",
                    &format!("the core refused the declaration: {code}"),
                    now,
                )
                .await?;
        } else {
            self.store.clear("declare", now).await?;
            self.last_declared = Some((now, declared));
            if let Some(s) = &support {
                self.declared_support = s.clone();
            }
        }
        Ok(())
    }

    /// Attest each asset location the configuration maps to the core (`DEC-20`, `F-029`): what
    /// the layer states about it at the finalized head (its symbol and decimals fields, and its
    /// status), read through the same client as every read that decides an outcome. Sent when the
    /// facts changed, or half the status lease has passed since the last; held for that lease. A
    /// binding reads it (`REQ-AS-8`); after the binding the core ignores it (`REQ-AS-9`). The
    /// core's verdict is informative: a book-only asset that could not be bound there now keeps
    /// an `asset:<ASSET>` notice open, so an operator sees why before asking.
    async fn attest_assets(&mut self) -> StepResult<()> {
        let fin = match self.chain.finalized().await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "no finalized head to attest assets at");
                return Ok(());
            }
        };
        let now = self.now();
        for b in self.config.assets.clone() {
            let facts = match self.chain.asset_metadata(&fin, &b.chain).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(asset = %b.asset, location = %b.chain.0, error = %e, "the asset's metadata could not be read");
                    continue;
                }
            };
            let body = AttestationBody {
                location: b.chain.0.clone(),
                asset: b.asset.clone(),
                height: fin.number,
                facts: facts.as_ref().map(|f| LayerAssetFactsBody {
                    symbol: String::from_utf8_lossy(&f.symbol).into_owned(),
                    decimals: f.decimals,
                    layer_status: match f.status {
                        LayerAssetStatus::Live => LayerAssetStatusBody::Live,
                        LayerAssetStatus::Frozen => LayerAssetStatusBody::Frozen,
                        LayerAssetStatus::Destroying => LayerAssetStatusBody::Destroying,
                    },
                }),
                lease: self.config.status_lease,
            };
            // A mapped asset the core did not hold yet is attested every step, so that its
            // registration reaches the support declared at once.
            let due = match self.attested.get(&b.chain.0) {
                _ if self.unregistered.contains(&b.asset) => true,
                None => true,
                Some((at, last)) => {
                    last.facts != body.facts
                        || last.asset != body.asset
                        || now.saturating_sub(*at) >= self.config.status_lease / 2
                }
            };
            if !due {
                continue;
            }
            self.declarations += 1;
            let key = format!(
                "settle/attest/{}/{now}/{}/{}",
                self.session, self.declarations, b.chain.0
            );
            self.report_start(
                &key,
                "attest",
                serde_json::to_value(&body).unwrap_or_default(),
            )
            .await?;
            let a = self.link.attest(&key, &body).await?;
            self.report_end(&key, &a).await?;
            let alert = format!("asset:{}", b.asset);
            match &a {
                Answer::Refused(code) => {
                    self.store
                        .raise(
                            &alert,
                            &format!("the core refused the attestation of {}: {code}", b.chain.0),
                            now,
                        )
                        .await?;
                    continue;
                }
                Answer::Decided { result, .. } => {
                    if result
                        .attestation
                        .as_ref()
                        .is_some_and(|v| v.verdict == "unregistered")
                    {
                        self.unregistered.insert(b.asset.clone());
                    } else {
                        self.unregistered.remove(&b.asset);
                    }
                    match &result.attestation {
                        Some(v) if v.verdict == "book_only" && v.mismatch.is_some() => {
                            let why = format!(
                                "{} is book-only and could not be bound at {} now: {} (finalized #{})",
                                b.asset,
                                b.chain.0,
                                v.mismatch.as_deref().unwrap_or("?"),
                                fin.number,
                            );
                            if self.store.raise(&alert, &why, now).await? {
                                tracing::warn!(asset = %b.asset, location = %b.chain.0, detail = %why, "a book-only asset does not match its location");
                            }
                        }
                        _ => {
                            self.store.clear(&alert, now).await?;
                        }
                    }
                }
            }
            self.attested.insert(b.chain.0.clone(), (now, body));
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------ reports

    async fn report_start(&self, key: &str, kind: &str, body: serde_json::Value) -> StepResult<()> {
        if self.store.report(key).await?.is_none() {
            self.store
                .put_report(&ReportRow {
                    key: key.to_owned(),
                    kind: kind.to_owned(),
                    body,
                    verdict: None,
                    sent_at: self.now(),
                    answered_at: None,
                })
                .await?;
        }
        Ok(())
    }

    async fn report_end(&self, key: &str, a: &Answer) -> StepResult<()> {
        tracing::debug!(report = %key, verdict = %a.verdict(), "the core answered");
        if let Some(mut r) = self.store.report(key).await? {
            r.verdict = Some(a.verdict());
            r.answered_at = Some(self.now());
            self.store.put_report(&r).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------ step

    /// One step. See the module documentation.
    pub async fn step(&mut self) -> StepResult<StepReport> {
        let stepped = self.step_once().await;
        // The core cannot be reached (`REQ-PR-9`): the chain is still verified and kept, so that an
        // outage longer than the node keeps events for leaves nothing unreadable.
        if matches!(stepped, Err(AdapterError::Link(_))) && self.serving {
            if let Ok(fin) = self.chain.finalized().await {
                if let Err(e) = self.verify(&fin).await {
                    tracing::warn!(error = %e, "the chain could not be verified either");
                }
            }
        }
        stepped
    }

    async fn step_once(&mut self) -> StepResult<StepReport> {
        let epoch = self.hold_lease().await?;
        self.heartbeat().await?;
        let mut report = StepReport {
            serving: self.serving,
            held: self.held,
            ..StepReport::default()
        };
        if !self.serving {
            return Ok(report);
        }
        self.steps += 1;
        let fin = self.chain.finalized().await?;
        let best = self.chain.best().await?;
        report.finalized = fin.number;
        report.best = best.number;
        // The call outbox (`F-027` T-027-01): every external movement and anchor, as envelopes.
        let now = self.now();
        let calls = match self.link.calls().await? {
            Ok(calls) => {
                self.store.clear("outbox", now).await?;
                calls
            }
            Err(code) => {
                self.store
                    .raise(
                        "outbox",
                        &format!("the call outbox was refused: {code}"),
                        now,
                    )
                    .await?;
                Vec::new()
            }
        };
        self.last_seen = Some(Seen {
            finalized: fin.hash,
            best: best.hash,
            waiting: waiting(&calls),
        });
        report.acknowledged = self.intake(&calls).await?;
        self.track(epoch, &fin).await?;
        let (observed, gaps, head_taken) = self.observe(&fin, &best).await?;
        self.declare_hold().await?;
        report.held = self.held;
        report.observed = observed;
        report.gaps = gaps;
        let (signed, parked, broadcast) = self.submit(epoch, &fin, &best, head_taken).await?;
        report.signed = signed;
        report.parked = parked;
        report.broadcast = broadcast;
        // A reset under the adapter: nothing more goes to the book it serves (`refound`).
        if self.anchor_signer.is_some() && !self.refounding().await? {
            let anchor = calls.iter().find_map(|c| match &c.call {
                SettlementCallBody::Anchor { number, commitment } => {
                    Some((*number, commitment.clone(), c.acknowledged))
                }
                SettlementCallBody::Withdraw { .. } | SettlementCallBody::Bind { .. } => None,
            });
            // F-045: the anchor key's signer not signing (Vault Transit down, say) ends the step, but
            // only after the bindings and the position reports, which it must not starve: the
            // positions sign nothing.
            let anchored = self.anchor(epoch, &fin, &best, anchor).await;
            if !signer_failed(&anchored) {
                anchored.clone()?;
            }
            // The bindings in flight (amendment 0011, `REQ-AS-8`), on the anchor key's lane.
            let binds: Vec<BindAsked> = calls
                .iter()
                .filter_map(|c| match &c.call {
                    SettlementCallBody::Bind {
                        asset,
                        location,
                        amount,
                    } => Some(BindAsked {
                        reference: c.reference.clone(),
                        asset: asset.clone(),
                        location: location.clone(),
                        amount: amount.parse().ok()?,
                    }),
                    _ => None,
                })
                .collect();
            let bound = self.binds(epoch, &fin, &best, &binds).await;
            if !signer_failed(&bound) {
                bound.clone()?;
            }
            self.positions(&fin).await?;
            anchored?;
            bound?;
        }
        Ok(report)
    }

    /// [`Adapter::observe`]'s step 3: the verified entries `kept` the core has not taken in, in
    /// order from the cursor; the verified head `v` is accepted once the core has taken in every
    /// one. How many were reported.
    async fn report_kept(
        &mut self,
        fin: &BlockRef,
        v: AcceptedHead,
        accepted: Option<AcceptedHead>,
        kept: &BTreeMap<u64, Vec<u8>>,
    ) -> StepResult<usize> {
        let instance = self.config.instance;
        let now = self.now();
        let mut reported = 0;
        let cursor = self.store.cursor(instance).await?;
        let mut all = true;
        for (seq, raw) in kept.range(cursor..) {
            let seq = *seq;
            let e = self.chain.entry(raw.clone())?;
            self.store
                .insert_observation(&self.observation_row(seq, &e, true))
                .await?;
            self.fault(CrashPoint::AfterObserve)?;
            let body = self.observation_body(seq, &e, true);
            let a = self.report_observation(&body).await?;
            if !a.ingested() {
                // The core did not take it in: do not pass it.
                self.store
                    .raise(
                        &format!("report:{instance}:{seq}"),
                        &format!("the core refused an observation: {}", a.verdict()),
                        now,
                    )
                    .await?;
                all = false;
                break;
            }
            self.store
                .clear(&format!("report:{instance}:{seq}"), now)
                .await?;
            self.report_check(&body.coordinate, fin.number, true)
                .await?;
            reported += 1;
            self.note_record(&e.event).await?;
            self.fault(CrashPoint::AfterVerdict)?;
            self.store.advance_cursor(instance, seq + 1).await?;
        }
        let moved = accepted.is_none_or(|a| (a.position, a.digest) != (v.position, v.digest));
        if all && moved && self.store.cursor(instance).await? >= v.position {
            self.store.accept_head(instance, &v).await?;
        }
        Ok(reported)
    }

    // ------------------------------------------------------------------------------ anchor

    /// Step 8 (`F-017`, `F-027`). See the module documentation. `outbox` is the anchor call the
    /// call outbox holds, if any: its number, its commitment (hex) and whether it is acknowledged.
    async fn anchor(
        &mut self,
        epoch: u64,
        fin: &BlockRef,
        best: &BlockRef,
        outbox: Option<(u64, String, bool)>,
    ) -> StepResult<()> {
        let Some(signer) = self.anchor_signer.clone() else {
            return Ok(());
        };
        let now = self.now();
        let instance = self.config.instance;
        // The anchor read says whether a settlement comparison is due; the checkpoint in flight
        // comes from the call outbox.
        let read = match self.link.anchor().await? {
            Ok(r) => r,
            Err(code) => {
                self.store
                    .raise(
                        "anchor",
                        &format!("the anchor read was refused: {code}"),
                        now,
                    )
                    .await?;
                return Ok(());
            }
        };
        self.store.clear("anchor", now).await?;
        self.comparison_due = read.comparison_due;
        let Some(view) = self.chain.instance(fin, instance).await? else {
            return Ok(());
        };
        let key = Self::lane_key(signer.as_ref());
        let finalized_nonce = self.chain.account_nonce(fin, &signer.account()).await?;
        // 1. Decide every live anchor extrinsic at the finalized head.
        for mut row in self.store.anchors().await? {
            let Some(mut ext) = row.extrinsic.clone() else {
                continue;
            };
            if !matches!(
                ext.status,
                ExtrinsicStatus::Signed | ExtrinsicStatus::Broadcast { .. }
            ) {
                continue;
            }
            let proof = if finalized_nonce > ext.nonce {
                match self
                    .chain
                    .find_extrinsic(fin, ext.era.birth, &ext.extrinsic.hash)
                    .await?
                {
                    Some(included) => {
                        let failed = if !included.success {
                            Some(NotExecuted::DispatchFailed {
                                block: included.block,
                                error: included.error.clone().unwrap_or_default(),
                            })
                        } else if view.last_anchor < row.number {
                            // The batch executed in a finalized block, and the anchor is not
                            // stored at the finalized head: the contract refused the call
                            // (`F-027`), and the inbox records why.
                            Some(NotExecuted::CallRefused {
                                block: included.block,
                            })
                        } else {
                            None
                        };
                        ext.status = ExtrinsicStatus::Included(included);
                        failed
                    }
                    None => {
                        self.store
                            .raise(
                                &format!("lane:{key}"),
                                &format!(
                                    "nonce {} of the anchor key was spent by an extrinsic the adapter did not sign",
                                    ext.nonce
                                ),
                                now,
                            )
                            .await?;
                        Some(NotExecuted::NonceSpent {
                            finalized: fin.number,
                        })
                    }
                }
            } else if fin.number >= ext.era.last_valid() {
                Some(NotExecuted::EraEnded {
                    finalized: fin.number,
                })
            } else {
                continue;
            };
            row.updated_at = now;
            match proof {
                None => row.extrinsic = Some(ext),
                Some(proof) => {
                    let already = view.last_anchor >= row.number;
                    ext.status = ExtrinsicStatus::Dead(proof);
                    let hash = hex::encode(ext.extrinsic.hash);
                    row.previous.push(ext);
                    row.extrinsic = None;
                    if !already {
                        // Provably not executed: the core emits the same anchor again.
                        let body = AnchorFailureBody { number: row.number };
                        let k = format!("settle/anchor-failed/{}/{hash}", row.number);
                        self.report_start(
                            &k,
                            "anchor_not_executed",
                            serde_json::to_value(&body).unwrap_or_default(),
                        )
                        .await?;
                        let a = self.link.anchor_not_executed(&k, &body).await?;
                        self.report_end(&k, &a).await?;
                        row.acknowledged = false;
                    }
                }
            }
            self.store.put_anchor(epoch, &row).await?;
        }
        // 2. The checkpoint in flight, from the call outbox: stored, then acknowledged.
        let Some((number, commitment_hex, acknowledged)) = outbox else {
            return Ok(());
        };
        struct InFlight {
            number: u64,
            commitment: String,
            acknowledged: bool,
        }
        let ins = InFlight {
            number,
            commitment: commitment_hex,
            acknowledged,
        };
        let Some(commitment) = hex32(&ins.commitment) else {
            self.store
                .raise(
                    &format!("anchor:{}", ins.number),
                    "the anchor's commitment is not 32 bytes",
                    now,
                )
                .await?;
            return Ok(());
        };
        let stored = self
            .store
            .anchors()
            .await?
            .into_iter()
            .find(|r| r.number == ins.number);
        let mut row = match stored {
            Some(r) if r.commitment == ins.commitment => r,
            Some(r) => {
                let why = format!(
                    "the core handed anchor {} with commitment {} after {}",
                    ins.number, ins.commitment, r.commitment
                );
                self.store
                    .raise(&format!("anchor:{}", ins.number), &why, now)
                    .await?;
                return Ok(());
            }
            None => {
                let r = AnchorRow {
                    number: ins.number,
                    commitment: ins.commitment.clone(),
                    acknowledged: false,
                    extrinsic: None,
                    previous: Vec::new(),
                    received_at: now,
                    updated_at: now,
                };
                self.store.put_anchor(epoch, &r).await?;
                r
            }
        };
        if !ins.acknowledged {
            let body = AnchorAcknowledgementBody {
                number: ins.number,
                commitment: ins.commitment.clone(),
            };
            // A re-emission is a new request: the core moved the anchor back to pending.
            let k = format!("settle/anchor-ack/{}/{}", ins.number, row.previous.len());
            self.report_start(
                &k,
                "acknowledge_anchor",
                serde_json::to_value(&body).unwrap_or_default(),
            )
            .await?;
            let a = self.link.acknowledge_anchor(&k, &body).await?;
            self.report_end(&k, &a).await?;
            if let Answer::Decided { .. } = a {
                row.acknowledged = true;
                row.updated_at = now;
                self.store.put_anchor(epoch, &row).await?;
            }
        }
        // 3. Stored at the finalized head already: the inbox read reports its record.
        if view.last_anchor >= ins.number {
            return Ok(());
        }
        // A live extrinsic: the same bytes again, while its era lasts.
        if let Some(ext) = &row.extrinsic {
            if best.number < ext.era.death() {
                self.chain.submit(&ext.extrinsic).await?;
                let mut e = ext.clone();
                e.status = ExtrinsicStatus::Broadcast {
                    attempts: match ext.status {
                        ExtrinsicStatus::Broadcast { attempts } => attempts + 1,
                        _ => 1,
                    },
                };
                row.extrinsic = Some(e);
                self.store.put_anchor(epoch, &row).await?;
            }
            return Ok(());
        }
        // The contract takes numbers in strict order.
        if view.last_anchor.checked_add(1) != Some(ins.number) {
            self.store
                .raise(
                    &format!("anchor-order:{instance}"),
                    &format!(
                        "the instance's last anchor is {}, the core's next is {}",
                        view.last_anchor, ins.number
                    ),
                    now,
                )
                .await?;
            return Ok(());
        }
        self.store
            .clear(&format!("anchor-order:{instance}"), now)
            .await?;
        if self.chain.native_balance(fin, &signer.account()).await? < self.config.fee_floor {
            self.store
                .raise(
                    &format!("fees:{key}"),
                    "the anchor key's native balance is below the fee floor",
                    now,
                )
                .await?;
            return Ok(());
        }
        self.store.clear(&format!("fees:{key}"), now).await?;
        let runtime = self.chain.runtime().await?;
        // T-027-12: the runtime the step's assertions passed, unless it changed since.
        let Ok(chosen) = self.chosen(&runtime) else {
            return Ok(());
        };
        let max_era = self.config.max_era.min(runtime.block_hash_count).max(4);
        // An anchor has no expiry; its era, born at the finalized head, still ends within the PASS
        // challenge's window.
        let Some(era) = Mortality::born_at_finalized(
            fin.number,
            best.number,
            fin.number.saturating_add(self.config.challenge_window),
            max_era,
        ) else {
            return Ok(());
        };
        // One extrinsic of the anchor key in flight at a time: a binding's goes first.
        if self.bind_in_flight().await? {
            return Ok(());
        }
        let call = BatchCall::anchor(&AnchorCall {
            instance,
            number: ins.number,
            commitment,
        });
        // Its limits from a dry run at the finalized head, as every batch's; none, nothing signed.
        if !self.dry_runs_due().await? {
            return Ok(());
        }
        let Some(cap) = self.batch_cap(fin).await? else {
            return Ok(());
        };
        let dry = match self
            .chain
            .dry_run(
                fin,
                instance,
                std::slice::from_ref(&call),
                &signer.account(),
            )
            .await
        {
            Ok(d) => d,
            Err(ChainError::Unavailable(e)) => {
                self.pause_dry_runs(&e).await?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let alert = format!("anchor-dry-run:{instance}");
        let limits = match &dry.failure {
            None => self.batch_limits(&dry, true, cap),
            Some(_) => None,
        };
        let Some(limits) = limits else {
            let why = match dry.failure {
                Some(f) => format!("the anchor's dry run fails: {f}; nothing is signed"),
                None => format!(
                    "the anchor needs {:?}, more than a batch is given; nothing is signed",
                    dry.gas_required
                ),
            };
            self.store.raise(&alert, &why, now).await?;
            return Ok(());
        };
        self.store.clear(&alert, now).await?;
        let signed = self
            .chain
            .sign_batch(
                instance,
                std::slice::from_ref(&call),
                signer.as_ref(),
                finalized_nonce,
                &era,
                fin,
                &limits,
                &chosen.signing,
            )
            .await?;
        // T-027-12: the layer's confirmation before the first broadcast on a runtime.
        if !self.confirmed(&chosen, &signed).await? {
            return Ok(());
        }
        // REQ-SA-6: stored before it is broadcast.
        row.extrinsic = Some(AnchorExtrinsic {
            lane: key,
            nonce: finalized_nonce,
            era,
            extrinsic: signed.clone(),
            status: ExtrinsicStatus::Signed,
            signed_at: now,
        });
        row.updated_at = now;
        self.store.put_anchor(epoch, &row).await?;
        tracing::info!(
            number = ins.number,
            nonce = finalized_nonce,
            hash = %hex::encode(signed.hash),
            "anchor signed and stored"
        );
        self.chain.submit(&signed).await?;
        if let Some(e) = row.extrinsic.as_mut() {
            e.status = ExtrinsicStatus::Broadcast { attempts: 1 };
        }
        self.store.put_anchor(epoch, &row).await?;
        Ok(())
    }

    /// Whether an extrinsic of the anchor key carrying a binding may still execute.
    async fn bind_in_flight(&self) -> StepResult<bool> {
        Ok(self.store.binds().await?.iter().any(|r| {
            r.extrinsic.as_ref().is_some_and(|e| {
                matches!(
                    e.status,
                    ExtrinsicStatus::Signed | ExtrinsicStatus::Broadcast { .. }
                )
            })
        }))
    }

    /// Whether an extrinsic of the anchor key carrying an anchor may still execute.
    async fn anchor_in_flight(&self) -> StepResult<bool> {
        Ok(self.store.anchors().await?.iter().any(|r| {
            r.extrinsic.as_ref().is_some_and(|e| {
                matches!(
                    e.status,
                    ExtrinsicStatus::Signed | ExtrinsicStatus::Broadcast { .. }
                )
            })
        }))
    }

    /// The bindings of book-only assets in the call outbox (amendment 0011, `REQ-AS-8`,
    /// `F-029`), on the anchor key's lane, one extrinsic of that key in flight at a time.
    ///
    /// 1. Every live bind extrinsic is decided at the finalized head, as an anchor's is: once the
    ///    key's nonce passed it, it was included (executed: its outcome is in the inbox, which
    ///    the observation step reports, and it is never signed again) or not (dead); an era that
    ///    ended is dead. A dead one is signed again: a bind moves no value, and the contract
    ///    refuses a second one of the same asset.
    /// 2. A binding not yet stored is stored; its location must be the one this adapter's
    ///    configuration maps the asset to, or it is never signed (an alert says why).
    /// 3. With nothing of the anchor key in flight, the oldest binding with no live extrinsic is
    ///    dry-run, signed at the finalized nonce, stored, then broadcast (`REQ-SA-6`).
    async fn binds(
        &mut self,
        epoch: u64,
        fin: &BlockRef,
        best: &BlockRef,
        asked: &[BindAsked],
    ) -> StepResult<()> {
        let Some(signer) = self.anchor_signer.clone() else {
            return Ok(());
        };
        let now = self.now();
        let instance = self.config.instance;
        let key = Self::lane_key(signer.as_ref());
        let finalized_nonce = self.chain.account_nonce(fin, &signer.account()).await?;
        // 1. Decide every live bind extrinsic.
        for mut row in self.store.binds().await? {
            let Some(mut ext) = row.extrinsic.clone() else {
                continue;
            };
            if !matches!(
                ext.status,
                ExtrinsicStatus::Signed | ExtrinsicStatus::Broadcast { .. }
            ) {
                continue;
            }
            let dead = if finalized_nonce > ext.nonce {
                match self
                    .chain
                    .find_extrinsic(fin, ext.era.birth, &ext.extrinsic.hash)
                    .await?
                {
                    Some(included) if included.success => {
                        ext.status = ExtrinsicStatus::Included(included);
                        row.executed = true;
                        None
                    }
                    Some(included) => Some(NotExecuted::DispatchFailed {
                        block: included.block,
                        error: included.error.clone().unwrap_or_default(),
                    }),
                    None => Some(NotExecuted::NonceSpent {
                        finalized: fin.number,
                    }),
                }
            } else if fin.number >= ext.era.last_valid() {
                Some(NotExecuted::EraEnded {
                    finalized: fin.number,
                })
            } else {
                // Still live: the same bytes again, while its era lasts.
                if best.number < ext.era.death() {
                    self.chain.submit(&ext.extrinsic).await?;
                    ext.status = ExtrinsicStatus::Broadcast {
                        attempts: match ext.status {
                            ExtrinsicStatus::Broadcast { attempts } => attempts + 1,
                            _ => 1,
                        },
                    };
                    row.extrinsic = Some(ext);
                    row.updated_at = now;
                    self.store.put_bind(epoch, &row).await?;
                }
                continue;
            };
            row.updated_at = now;
            match dead {
                None => row.extrinsic = Some(ext),
                Some(proof) => {
                    tracing::warn!(reference = %row.reference, ?proof, "a bind extrinsic provably did not execute; it is signed again");
                    ext.status = ExtrinsicStatus::Dead(proof);
                    row.previous.push(ext);
                    row.extrinsic = None;
                }
            }
            self.store.put_bind(epoch, &row).await?;
        }
        // 2. Store each binding asked for.
        let stored = self.store.binds().await?;
        for b in asked {
            let alert = format!("bind:{}", b.asset);
            if self
                .config
                .binding_of_chain(&ChainAsset(b.location.clone()))
                .is_none_or(|m| m.asset != b.asset)
            {
                self.store
                    .raise(
                        &alert,
                        &format!(
                            "the core asks to bind {} at {}, which this adapter does not map to it; nothing is signed",
                            b.asset, b.location
                        ),
                        now,
                    )
                    .await?;
                continue;
            }
            self.store.clear(&alert, now).await?;
            if !stored.iter().any(|r| r.reference == b.reference) {
                let row = BindRow {
                    reference: b.reference.clone(),
                    asset: b.asset.clone(),
                    location: b.location.clone(),
                    amount: b.amount,
                    extrinsic: None,
                    previous: Vec::new(),
                    executed: false,
                    received_at: now,
                    updated_at: now,
                };
                self.store.put_bind(epoch, &row).await?;
            }
        }
        // 3. Sign the oldest binding still asked for with no live extrinsic.
        if self.bind_in_flight().await? || self.anchor_in_flight().await? {
            return Ok(());
        }
        let mut rows: Vec<BindRow> = self
            .store
            .binds()
            .await?
            .into_iter()
            .filter(|r| !r.executed && r.extrinsic.is_none())
            .filter(|r| asked.iter().any(|b| b.reference == r.reference))
            .collect();
        rows.sort_by_key(|r| r.received_at);
        let Some(mut row) = rows.into_iter().next() else {
            return Ok(());
        };
        let Some(reference) = hex32(&row.reference) else {
            return Ok(());
        };
        if self.chain.native_balance(fin, &signer.account()).await? < self.config.fee_floor {
            self.store
                .raise(
                    &format!("fees:{key}"),
                    "the anchor key's native balance is below the fee floor",
                    now,
                )
                .await?;
            return Ok(());
        }
        let runtime = self.chain.runtime().await?;
        // T-027-12: the runtime the step's assertions passed, unless it changed since.
        let Ok(chosen) = self.chosen(&runtime) else {
            return Ok(());
        };
        let max_era = self.config.max_era.min(runtime.block_hash_count).max(4);
        let Some(era) = Mortality::born_at_finalized(
            fin.number,
            best.number,
            fin.number.saturating_add(self.config.challenge_window),
            max_era,
        ) else {
            return Ok(());
        };
        let call = BatchCall::bind(&BindCall {
            reference,
            asset: ChainAsset(row.location.clone()),
            amount: row.amount,
        });
        if !self.dry_runs_due().await? {
            return Ok(());
        }
        let Some(cap) = self.batch_cap(fin).await? else {
            return Ok(());
        };
        let dry = match self
            .chain
            .dry_run(
                fin,
                instance,
                std::slice::from_ref(&call),
                &signer.account(),
            )
            .await
        {
            Ok(d) => d,
            Err(ChainError::Unavailable(e)) => {
                self.pause_dry_runs(&e).await?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let alert = format!("bind-dry-run:{}", row.asset);
        let limits = match &dry.failure {
            None => self.batch_limits(&dry, true, cap),
            Some(_) => None,
        };
        let Some(limits) = limits else {
            let why = match dry.failure {
                Some(f) => format!("the bind's dry run fails: {f}; nothing is signed"),
                None => format!(
                    "the bind needs {:?}, more than a batch is given; nothing is signed",
                    dry.gas_required
                ),
            };
            self.store.raise(&alert, &why, now).await?;
            return Ok(());
        };
        self.store.clear(&alert, now).await?;
        let signed = self
            .chain
            .sign_batch(
                instance,
                std::slice::from_ref(&call),
                signer.as_ref(),
                finalized_nonce,
                &era,
                fin,
                &limits,
                &chosen.signing,
            )
            .await?;
        // T-027-12: the layer's confirmation before the first broadcast on a runtime.
        if !self.confirmed(&chosen, &signed).await? {
            return Ok(());
        }
        // REQ-SA-6: stored before it is broadcast.
        row.extrinsic = Some(AnchorExtrinsic {
            lane: key,
            nonce: finalized_nonce,
            era,
            extrinsic: signed.clone(),
            status: ExtrinsicStatus::Signed,
            signed_at: now,
        });
        row.updated_at = now;
        self.store.put_bind(epoch, &row).await?;
        tracing::info!(
            asset = %row.asset,
            location = %row.location,
            amount = %row.amount,
            nonce = finalized_nonce,
            hash = %hex::encode(signed.hash),
            "bind signed and stored"
        );
        self.chain.submit(&signed).await?;
        if let Some(e) = row.extrinsic.as_mut() {
            e.status = ExtrinsicStatus::Broadcast { attempts: 1 };
        }
        self.store.put_bind(epoch, &row).await?;
        Ok(())
    }

    /// Step 9 (`F-017`, `REQ-PR-5`). See the module documentation.
    async fn positions(&mut self, fin: &BlockRef) -> StepResult<()> {
        let instance = self.config.instance;
        let now = self.now();
        let Some(view) = self.chain.instance(fin, instance).await? else {
            return Ok(());
        };
        // The core compares only a cut whose every position it holds as final.
        if self.store.cursor(instance).await? < view.next_seq {
            return Ok(());
        }
        let mut positions = Vec::new();
        for b in &self.config.assets {
            let p = self.chain.position(fin, instance, &b.chain).await?;
            let Ok(amount) = u64::try_from(p) else {
                self.store
                    .raise(
                        &format!("position:{instance}:{}", b.chain.0),
                        "the position does not fit the book's minor units",
                        now,
                    )
                    .await?;
                return Ok(());
            };
            positions.push(PositionBody {
                asset: b.asset.clone(),
                amount: amount.to_string(),
            });
        }
        let cut = (view.next_seq, positions.clone());
        if !self.comparison_due && self.last_cut.as_ref() == Some(&cut) {
            return Ok(());
        }
        let body = PositionsBody {
            next_seq: view.next_seq,
            height: fin.number,
            positions,
        };
        let k = format!(
            "settle/positions/{instance}/{}/{}",
            fin.number,
            crate::chain::blake2_256(&serde_json::to_vec(&body).unwrap_or_default())
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        self.report_start(
            &k,
            "positions",
            serde_json::to_value(&body).unwrap_or_default(),
        )
        .await?;
        let a = self.link.positions(&k, &body).await?;
        self.report_end(&k, &a).await?;
        if let Answer::Decided { result, .. } = &a {
            self.last_cut = Some(cut);
            self.comparison_due = false;
            if result.comparison.as_ref().is_some_and(|c| c.differs) {
                tracing::warn!(
                    cut = view.next_seq,
                    "the book and the settlement layer differ"
                );
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------ intake

    /// The operation a withdrawal call names, for an alert's key: its `did:`, or its reference.
    fn call_name(c: &CallBody) -> String {
        hex32(&c.reference)
            .and_then(|r| operation_of(&r))
            .map_or_else(|| c.reference.clone(), |op| op.to_did())
    }

    /// Validate one withdrawal of the call outbox into the row the adapter stores, or say why it
    /// is refused (`F-027` T-027-01: the envelope is the instruction).
    fn validate(&self, c: &CallBody) -> Result<InstructionRow, String> {
        let reference = hex32(&c.reference).ok_or("the reference is not 32 bytes")?;
        let op = operation_of(&reference).ok_or("the reference names no operation")?;
        let SettlementCallBody::Withdraw {
            asset,
            amount,
            destination,
        } = &c.call
        else {
            return Err("not a withdrawal".into());
        };
        // INV-13: an instruction without a reservation is never acknowledged.
        if c.reserved != Some(true) {
            return Err("the instruction carries no reservation (INV-13)".into());
        }
        if c.target != self.config.instance {
            return Err("the call acts for another deployment".into());
        }
        let expiry = c.expiry.ok_or("a withdrawal without an expiry")?;
        let asset = AssetId::parse(asset).map_err(|_| "the asset does not parse".to_owned())?;
        let amount: u64 = amount
            .parse()
            .map_err(|_| "the amount is not minor units".to_owned())?;
        let nz = NonZeroMinor::new(Minor::new(amount)).map_err(|_| "a zero amount".to_owned())?;
        let to = parse_destination(destination)
            .map_err(|_| "the destination is not an address of this network".to_owned())?;
        let digest = c
            .instruction
            .as_deref()
            .and_then(hex32)
            .ok_or("the instruction's identity is not 32 bytes")?;
        // Recomputed from the envelope alone: the call and the identity it is acknowledged under
        // cannot disagree.
        if instruction_digest(&op, &asset, nz, to, expiry) != digest {
            return Err("the digest does not match the instruction's fields".into());
        }
        let binding = self
            .config
            .binding(asset.as_str())
            .ok_or("the asset is not one the adapter supports")?;
        if let Destination::Instance { instance, .. } = to {
            if instance == self.config.instance {
                return Err("the destination is the deployment's own instance".into());
            }
        }
        let now = self.now();
        Ok(InstructionRow {
            operation: op.to_did(),
            digest: hex::encode(digest),
            asset: asset.as_str().to_owned(),
            amount,
            call: WithdrawCall {
                instance: self.config.instance,
                operation_ref: reference,
                to,
                asset: binding.chain.clone(),
                amount: u128::from(amount),
            },
            expiry,
            acknowledged: false,
            state: InstructionState::Received,
            received_at: now,
            updated_at: now,
        })
    }

    /// Store every valid withdrawal of the call outbox, then acknowledge it (`REQ-SO-6`).
    async fn intake(&mut self, calls: &[CallBody]) -> StepResult<usize> {
        let now = self.now();
        for c in calls {
            if matches!(c.call, SettlementCallBody::Anchor { .. }) {
                continue;
            }
            let row = match self.validate(c) {
                Ok(r) => r,
                Err(why) => {
                    let key = format!("instruction:{}", Self::call_name(c));
                    self.store.raise(&key, &why, now).await?;
                    continue;
                }
            };
            match self.store.insert_instruction(&row).await? {
                Inserted::New | Inserted::Same(_) => {}
                Inserted::Conflict(existing) => {
                    // The same reference with another digest: an alarm, and nothing else.
                    let why = format!(
                        "the core handed a second instruction for {}: digest {} after {}",
                        row.operation, row.digest, existing.digest
                    );
                    self.store
                        .raise(&format!("digest:{}", row.operation), &why, now)
                        .await?;
                    continue;
                }
            }
            self.fault(CrashPoint::AfterStore)?;
        }
        // Acknowledge every stored row the core does not yet have: new ones, and any whose
        // acknowledgement's answer was lost (the core no longer lists those).
        let mut acked = 0;
        for mut row in self.store.instructions().await? {
            if row.acknowledged {
                continue;
            }
            let body = AcknowledgementBody {
                operation_reference: row.operation.clone(),
                instruction: row.digest.clone(),
            };
            let key = format!("settle/ack/{}", row.operation);
            self.report_start(
                &key,
                "acknowledge",
                serde_json::to_value(&body).unwrap_or_default(),
            )
            .await?;
            let a = self.link.acknowledge(&key, &body).await?;
            self.fault(CrashPoint::AfterAck)?;
            self.report_end(&key, &a).await?;
            match a {
                Answer::Decided { .. } => {
                    row.acknowledged = true;
                    if row.state == InstructionState::Received {
                        row.state = InstructionState::Acknowledged;
                    }
                    row.updated_at = self.now();
                    self.store.put_instruction(&row).await?;
                    acked += 1;
                }
                Answer::Refused(code) => {
                    self.store
                        .raise(
                            &format!("instruction:{}", row.operation),
                            &format!("the core refused the acknowledgement: {code}"),
                            now,
                        )
                        .await?;
                }
            }
        }
        Ok(acked)
    }

    // ------------------------------------------------------------------------------ track

    fn signer_of(&self, lane: &str) -> Option<Arc<dyn Signer>> {
        self.lanes
            .iter()
            .find(|s| Self::lane_key(s.as_ref()) == lane)
            .cloned()
    }

    /// Decide every uncertain transaction at the finalized head, once for the whole batch it
    /// carries, and free its lane when it is decided.
    async fn track(&mut self, epoch: u64, fin: &BlockRef) -> StepResult<()> {
        let now = self.now();
        // The rows of one transaction share its hash: one batch.
        let mut batches: BTreeMap<[u8; 32], Vec<ExtrinsicRow>> = BTreeMap::new();
        for ext in self.store.extrinsics().await? {
            if matches!(
                ext.status,
                ExtrinsicStatus::Signed | ExtrinsicStatus::Broadcast { .. }
            ) {
                batches.entry(ext.extrinsic.hash).or_default().push(ext);
            }
        }
        for rows in batches.into_values() {
            let first = &rows[0];
            let Some(signer) = self.signer_of(&first.lane) else {
                continue;
            };
            let nonce = self.chain.account_nonce(fin, &signer.account()).await?;
            let mut lane = self.store.lane(&first.lane).await?.unwrap_or(LaneRow {
                lane: first.lane.clone(),
                next_nonce: 0,
                in_flight: None,
            });
            // What the finalized head proves, and the proof of non-execution if there is one.
            let (status, proof): (ExtrinsicStatus, Option<NotExecuted>) = if nonce > first.nonce {
                // The nonce is spent: by this transaction, or by another of the key's.
                lane.next_nonce = lane.next_nonce.max(first.nonce + 1);
                match self
                    .chain
                    .find_extrinsic(fin, first.era.birth, &first.extrinsic.hash)
                    .await?
                {
                    Some(included) => {
                        // A batch that failed as a whole wrote nothing, and its nonce is spent:
                        // every call in it is `DispatchFailed` (`F-026` decision 12).
                        let failed = (!included.success).then(|| NotExecuted::DispatchFailed {
                            block: included.block,
                            error: included.error.clone().unwrap_or_default(),
                        });
                        (ExtrinsicStatus::Included(included), failed)
                    }
                    None => {
                        self.store
                            .raise(
                                &format!("lane:{}", first.lane),
                                &format!(
                                    "nonce {} was spent by an extrinsic the adapter did not sign",
                                    first.nonce
                                ),
                                now,
                            )
                            .await?;
                        let proof = NotExecuted::NonceSpent {
                            finalized: fin.number,
                        };
                        (ExtrinsicStatus::Dead(proof.clone()), Some(proof))
                    }
                }
            } else if fin.number >= first.era.last_valid() {
                // The era ended in a finalized block and the nonce is unspent: it can never be
                // included, and the lane uses the nonce again.
                lane.next_nonce = first.nonce;
                let proof = NotExecuted::EraEnded {
                    finalized: fin.number,
                };
                (ExtrinsicStatus::Dead(proof.clone()), Some(proof))
            } else {
                // Uncertain: the nonce stays reserved.
                continue;
            };
            let instructions = self.store.instructions().await?;
            for mut ext in rows {
                ext.status = status.clone();
                self.store.put_extrinsic(&ext).await?;
                let Some(proof) = proof.clone() else {
                    continue;
                };
                if let Some(mut row) = instructions
                    .iter()
                    .find(|r| r.operation == ext.operation)
                    .cloned()
                {
                    if matches!(
                        row.state,
                        InstructionState::Signed | InstructionState::Broadcast
                    ) {
                        row.state = InstructionState::NotExecuted { proof };
                        row.updated_at = now;
                        self.store.put_instruction(&row).await?;
                    }
                }
            }
            lane.in_flight = None;
            self.store.put_lane(epoch, &lane).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------ observe

    fn observation_body(&self, seq: u64, e: &InboxEntry, finalized: bool) -> ObservationBody {
        let (operation_ref, account_ref, movement) = match &e.event {
            InboxEvent::Deposited {
                account_ref,
                asset,
                amount,
                ..
            } => (None, Some(*account_ref), Some((asset, *amount))),
            InboxEvent::Withdrawn {
                operation_ref,
                asset,
                amount,
                ..
            }
            | InboxEvent::WithdrawRefused {
                operation_ref,
                asset,
                amount,
                ..
            }
            // A binding's reference is the `bind_asset` operation's (amendment 0011).
            | InboxEvent::Bound {
                operation_ref,
                asset,
                amount,
            }
            | InboxEvent::BindRefused {
                operation_ref,
                asset,
                amount,
                ..
            } => (Some(*operation_ref), None, Some((asset, *amount))),
            // An anchor's reference names no operation of the book.
            InboxEvent::Anchored { .. } | InboxEvent::AnchorRefused { .. } => (None, None, None),
        };
        let anchor = match &e.event {
            // Where it was read: the venue the checkpoint records (`F-027` T-027-04).
            InboxEvent::Anchored { number, commitment } => Some(AnchorFactBody {
                number: *number,
                commitment: hex::encode(commitment),
                venue: Some(venue_body(&self.chain.venue(self.config.instance))),
            }),
            _ => None,
        };
        // The book's asset and units: 1:1. A minor unit of the book is the layer's own smallest
        // unit, which a binding checked against the registry's precision (`REQ-AS-8`). An asset the adapter does not map, or an amount the book cannot hold,
        // is reported without a movement: the core records it and raises it.
        let movement = movement.and_then(|(asset, amount)| {
            let b = self.config.binding_of_chain(asset)?;
            let amount = u64::try_from(amount).ok()?;
            Some(MovementBody {
                asset: b.asset.clone(),
                amount: amount.to_string(),
            })
        });
        let payload = match &e.event {
            // The chain's own error text (`REQ-SA-3`): the contract's reason, or Kreivo's.
            InboxEvent::WithdrawRefused { error, .. }
            | InboxEvent::AnchorRefused { error, .. }
            | InboxEvent::BindRefused { error, .. } => hex::encode(error.as_bytes()),
            _ => hex::encode(&e.raw),
        };
        ObservationBody {
            coordinate: CoordinateBody {
                instance: self.config.instance,
                seq,
                digest: hex::encode(e.digest()),
            },
            at: e.at,
            kind: kind_body(&e.event),
            payload,
            observed_finality: if finalized {
                ObservedFinalityBody::Final
            } else {
                ObservedFinalityBody::Provisional
            },
            operation_reference: operation_ref.and_then(|r| book_op(&r)).map(|o| o.to_did()),
            account_ref: account_ref.map(hex::encode),
            movement,
            anchor,
        }
    }

    fn observation_row(&self, seq: u64, e: &InboxEntry, finalized: bool) -> ObservationRow {
        let operation = match &e.event {
            InboxEvent::Withdrawn { operation_ref, .. }
            | InboxEvent::WithdrawRefused { operation_ref, .. } => {
                book_op(operation_ref).map(|o| o.to_did())
            }
            _ => None,
        };
        ObservationRow {
            instance: self.config.instance,
            seq,
            digest: hex::encode(e.digest()),
            at: e.at,
            raw: hex::encode(&e.raw),
            finalized,
            operation,
            summary: summary(&e.event),
            read_at: self.now(),
        }
    }

    /// Send one observation, with its verdict stored. Returns the answer.
    async fn report_observation(&mut self, body: &ObservationBody) -> StepResult<Answer> {
        let c = &body.coordinate;
        // The finality as observed is part of the report: a provisional report and the final
        // one of the same coordinate are two requests (the second gets "already applied").
        let seen = match body.observed_finality {
            ObservedFinalityBody::Final => "final",
            ObservedFinalityBody::Provisional => "provisional",
        };
        let key = format!("settle/obs/{}/{}/{}/{seen}", c.instance, c.seq, c.digest);
        self.report_start(
            &key,
            "observe",
            serde_json::to_value(body).unwrap_or_default(),
        )
        .await?;
        let a = self.link.observe(&key, body).await?;
        self.fault(CrashPoint::AfterReport)?;
        self.report_end(&key, &a).await?;
        // `NFR-15`: when each entry reached the core, and at which block it was written.
        tracing::info!(
            seq = c.seq,
            at = body.at,
            finality = seen,
            operation = body.operation_reference.as_deref().unwrap_or("-"),
            verdict = %a.verdict(),
            "observation reported"
        );
        Ok(a)
    }

    async fn report_check(
        &mut self,
        coordinate: &CoordinateBody,
        finalised_through: u64,
        still_matches: bool,
    ) -> StepResult<Answer> {
        let body = CheckBody {
            coordinate: coordinate.clone(),
            finalised_through,
            still_matches,
        };
        let key = format!(
            "settle/check/{}/{}/{}/{finalised_through}/{still_matches}",
            coordinate.instance, coordinate.seq, coordinate.digest
        );
        self.report_start(
            &key,
            "check",
            serde_json::to_value(&body).unwrap_or_default(),
        )
        .await?;
        let a = self.link.check(&key, &body).await?;
        self.report_end(&key, &a).await?;
        Ok(a)
    }

    async fn record_gap(
        &mut self,
        from: u64,
        to: u64,
        reason: GapReasonBody,
        sequence: Option<u32>,
    ) -> StepResult<()> {
        tracing::warn!(
            from,
            to,
            "a range of positions could not be read: recorded as a gap"
        );
        let now = self.now();
        let reason_text = match reason {
            GapReasonBody::Pruned => "pruned",
            GapReasonBody::Unreadable => "unreadable",
        };
        self.store
            .raise(
                &format!("gap:{}:{from}", self.config.instance),
                &format!("positions [{from}, {to}) are a gap: {reason_text}"),
                now,
            )
            .await?;
        let mut body = GapBody {
            from,
            to,
            reason,
            sequence,
        };
        let mut key = format!("settle/gap/{}/{from}/{to}", self.config.instance);
        self.report_start(&key, "gap", serde_json::to_value(&body).unwrap_or_default())
            .await?;
        let mut a = self.link.gap(&key, &body).await?;
        self.report_end(&key, &a).await?;
        // Amendment 0041: a gap that names its sequence is refused `ERR-MalformedCommand` by a core
        // before the amendment's generation (or an ingress before it): sent again without it, as
        // the gap of the sequence the core holds, which the next start clears (the cautious side).
        if body.sequence.is_some()
            && matches!(&a, Answer::Refused(code) if code.as_str() == UNSUPPORTED_CODE)
        {
            body.sequence = None;
            key.push_str("/untagged");
            self.report_start(&key, "gap", serde_json::to_value(&body).unwrap_or_default())
                .await?;
            a = self.link.gap(&key, &body).await?;
            self.report_end(&key, &a).await?;
        }
        // A gap the core did not record is not passed: `gap:` stays open, and the cursor where it
        // was (`REQ-SA-7`: a skipped range is recorded, never silently).
        if let Answer::Refused(code) = &a {
            return Err(AdapterError::Refused(format!(
                "the core refused the gap [{from}, {to}): {code}; nothing was recorded"
            )));
        }
        self.store
            .insert_gap(&GapRow {
                instance: self.config.instance,
                from,
                to,
                reason: reason_text.to_owned(),
                at: now,
            })
            .await?;
        self.store.advance_cursor(self.config.instance, to).await?;
        Ok(())
    }

    /// The adapter's disposition of an instruction, from a finalized record of it.
    async fn note_record(&self, e: &InboxEvent) -> StepResult<()> {
        let (r, state) = match e {
            InboxEvent::Withdrawn { operation_ref, .. } => {
                (operation_ref, InstructionState::Executed)
            }
            InboxEvent::WithdrawRefused {
                operation_ref,
                error,
                ..
            } => (
                operation_ref,
                InstructionState::Refused {
                    error: error.clone(),
                },
            ),
            _ => return Ok(()),
        };
        let Some(op) = book_op(r) else { return Ok(()) };
        let did = op.to_did();
        if let Some(mut row) = self
            .store
            .instructions()
            .await?
            .into_iter()
            .find(|x| x.operation == did)
        {
            row.state = state;
            row.updated_at = self.now();
            self.store.put_instruction(&row).await?;
        }
        Ok(())
    }

    /// The events of the blocks back from `fin` that a sync needs (amendment 0040), walking back
    /// by parent hash, then read forward in the blocks' order: the current chain's entries, a
    /// reset discarding what came before it. A block the adapter's own client cannot serve (a full
    /// node keeps about 256 blocks of events) is read from the history endpoint, and so is every
    /// older one; one neither serves makes the range unreadable (`Err`, with why).
    async fn walk(&self, fin: &BlockRef, to: WalkTo) -> StepResult<Result<Walked, String>> {
        let instance = self.config.instance;
        let mut blocks = Vec::new();
        let mut at = *fin;
        let mut own_client = true;
        loop {
            if let WalkTo::After(b) = to {
                if at.number <= b {
                    break;
                }
            }
            let read = if own_client {
                match self.chain.block_events(&at, instance).await {
                    Ok(b) => Ok(b),
                    Err(e) => {
                        own_client = false;
                        self.history_read(&at, &e.to_string()).await?
                    }
                }
            } else {
                self.history_read(&at, "older than the adapter's client serves")
                    .await?
            };
            let b = match read {
                Ok(b) => b,
                Err(why) => return Ok(Err(why)),
            };
            let parent = b.parent;
            let start_here = match to {
                WalkTo::Start(start) => b.events.iter().any(|e| match e {
                    ChainEvent::Reset => true,
                    ChainEvent::Entry { seq, .. } => *seq == start,
                    ChainEvent::Legacy { .. } => false,
                }),
                WalkTo::After(_) => false,
            };
            blocks.push(b);
            if start_here || at.number == 0 {
                break;
            }
            at = BlockRef {
                number: at.number - 1,
                hash: parent,
            };
        }
        let mut out = Walked::default();
        for b in blocks.into_iter().rev() {
            for e in b.events {
                match e {
                    ChainEvent::Entry { seq, entry } => out.entries.push((seq, entry)),
                    ChainEvent::Reset => {
                        out.dropped += out.entries.len() + out.legacy.len();
                        out.entries.clear();
                        out.legacy.clear();
                        out.reset = Some(b.block.number);
                    }
                    ChainEvent::Legacy { seq } => out.legacy.push(seq),
                }
            }
        }
        Ok(Ok(out))
    }

    /// `block`'s events from the history endpoint, after the adapter's own client could not
    /// serve them (`why`).
    async fn history_read(
        &self,
        block: &BlockRef,
        why: &str,
    ) -> StepResult<Result<BlockEvents, String>> {
        Ok(
            match self.chain.history_events(block, self.config.instance).await {
                Ok(Some(b)) => {
                    tracing::info!(
                        block = block.number,
                        "a block's events read from the history endpoint"
                    );
                    Ok(b)
                }
                Ok(None) => Err(format!(
                    "the events of block #{} could not be read ({why}), and no history endpoint \
                     is configured",
                    block.number
                )),
                Err(e) => Err(format!(
                    "the events of block #{} could not be read ({why}), nor from the history \
                     endpoint ({e})",
                    block.number
                )),
            },
        )
    }

    /// The sync (amendment 0040, `REQ-IN-5`): does what the layer holds lead, entry by entry, from
    /// the head the adapter last verified to the head it holds at `fin`? Unchanged: done, with no
    /// other read (but the anchor sequence's, while the chain has no entry). Otherwise the events
    /// of the blocks since are folded from the verified digest; a result that differs is read once
    /// more before the sync stops.
    async fn sync(
        &self,
        fin: &BlockRef,
        head: &ChainHead,
        verified: Option<AcceptedHead>,
    ) -> StepResult<Result<Synced, Stop>> {
        let instance = self.config.instance;
        // The block of a reset the anchor sequence shows on an empty chain, which the walk must find.
        let mut expected_reset = None;
        let (from, digest, to) = match verified {
            Some(v) if v.position == head.position && v.digest == head.digest => {
                // A chain with no entry reads `(0, zeros)` before a reset and after it alike: only
                // its anchor sequence tells them apart.
                let moved = if v.digest == ZERO_DIGEST {
                    let e = self.chain.chain_epoch(fin, instance).await?;
                    (e.map_or(0, |e| e.epoch) != v.epoch).then(|| e.map_or(v.block, |e| e.at))
                } else {
                    None
                };
                let Some(at) = moved else {
                    return Ok(Ok(Synced::Idle));
                };
                expected_reset = Some(at);
                // The walk finds the reset, and any entry of the chain before it past the verified
                // head, which a reset to an empty chain hides (amendment 0041).
                (v.position, v.digest, WalkTo::After(v.block))
            }
            Some(v) => (v.position, v.digest, WalkTo::After(v.block)),
            // A first start: the chain from its start (the reset that began it, or the
            // instance's instantiation), with the zero digest.
            None if head.position == head.start && head.digest == ZERO_DIGEST => {
                return Ok(Ok(Synced::New {
                    entries: Vec::new(),
                }));
            }
            None => (head.start, ZERO_DIGEST, WalkTo::Start(head.start)),
        };
        // On a first start, the entries below the chain's start (an upgrade's block holds the
        // earlier code's) are not this chain's.
        let floor = if verified.is_some() { 0 } else { from };
        let mut last = String::new();
        for attempt in 1..=2 {
            let walked = match self.walk(fin, to).await? {
                Ok(w) => w,
                Err(why) => {
                    // A reset found too late to read the chain before it up to the reset: not a
                    // range to record as a gap (that would take the new chain's head as the old
                    // one's, and drop the new chain's entries below it).
                    if let Some(v) = verified {
                        let e = self.chain.chain_epoch(fin, instance).await?;
                        if e.map_or(0, |e| e.epoch) != v.epoch {
                            let at = e.map_or(v.block, |e| e.at);
                            return Ok(Err(Stop::ResetUnread(at, why)));
                        }
                    }
                    return Ok(Err(Stop::Unreadable(why)));
                }
            };
            if verified.is_some() {
                if let Some(block) = walked.reset {
                    return Ok(Err(Stop::Reset(block, walked.dropped, 0)));
                }
                // The anchor sequence moved on an empty chain, and the walk read no reset: a
                // reader that missed the event. Stopped as a reset, as before amendment 0041.
                if let Some(at) = expected_reset {
                    return Ok(Err(Stop::Reset(at, 0, 0)));
                }
            }
            let entries: Vec<(u64, InboxEntry)> = walked
                .entries
                .into_iter()
                .filter(|(seq, _)| *seq >= floor)
                .collect();
            let checked = match walked.legacy.into_iter().find(|seq| *seq >= floor) {
                Some(seq) => Err(format!(
                    "the event of position {seq} has the earlier code's shape: its entry cannot be \
                     rebuilt"
                )),
                None => crate::chain::fold(from, &digest, &entries, head),
            };
            match checked {
                Ok(()) => return Ok(Ok(Synced::New { entries })),
                Err(why) => {
                    tracing::warn!(
                        attempt,
                        from,
                        head = head.position,
                        error = %why,
                        "the chain's events do not lead to its head"
                    );
                    last = why;
                }
            }
        }
        Ok(Err(Stop::Mismatch(last)))
    }

    /// Whether the adapter is held back (amendment 0041): a reset under it (`reset:<instance>`), or
    /// a start the core refused (`epoch:`) or cannot take (`epoch-unsupported:`). It declares that
    /// it does not serve while held.
    async fn held(&self) -> StepResult<bool> {
        let instance = self.config.instance;
        let reset = format!("reset:{instance}");
        let prefixes = epoch_alert_prefixes(instance);
        Ok(self.store.alerts().await?.iter().any(|a| {
            a.cleared_at.is_none()
                && (a.key == reset || prefixes.iter().any(|p| a.key.starts_with(p.as_str())))
        }))
    }

    /// Declare again at once when the step changed whether the adapter is held ([`Adapter::held`]):
    /// a withdrawal accepted between a reset found and the next step's declaration would be built
    /// from a stale head.
    async fn declare_hold(&mut self) -> StepResult<()> {
        if self.held().await? != self.held {
            self.heartbeat().await?;
        }
        Ok(())
    }

    /// Whether a reset of the instance under this adapter is open (`reset:<instance>`): its book
    /// was re-founded, and nothing more goes to it (no observation, head, anchor or position cut)
    /// until the adapter is reset with the re-founding ([`Adapter::refound`]).
    async fn refounding(&self) -> StepResult<bool> {
        let key = format!("reset:{}", self.config.instance);
        Ok(self
            .store
            .alerts()
            .await?
            .iter()
            .any(|a| a.key == key && a.cleared_at.is_none()))
    }

    // -------------------------------------------------------------------- a new sequence

    /// Report the sequence `e` to the core (amendment 0041, `REQ-SA-10` as proposed), and keep its
    /// answer under [`epoch_key`]: what it answered last for this sequence, if that is final or not
    /// yet due again, without asking; otherwise asked, under a client key of its own for each
    /// attempt ([`EPOCH_RETRY`] apart), so a refusal the core gave is not replayed to the next.
    async fn report_epoch(&mut self, e: &crate::chain::ChainEpoch) -> StepResult<EpochAnswer> {
        let instance = self.config.instance;
        let now = self.now();
        let key = epoch_key(instance);
        if let Some(r) = self.store.report(&key).await? {
            if r.body["epoch"].as_u64() == Some(u64::from(e.epoch)) {
                if let (Some(v), Some(at)) = (r.verdict.as_deref(), r.answered_at) {
                    let a = EpochAnswer::of(v);
                    if a == EpochAnswer::Confirmed || now < at.saturating_add(EPOCH_RETRY) {
                        return Ok(a);
                    }
                }
            }
        }
        let body = EpochBody {
            epoch: e.epoch,
            base: e.base,
            from_seq: e.from_seq,
            at: e.at,
            book: hex::encode(e.book),
            previous: e.previous.map(|(number, commitment)| PreviousAnchorBody {
                number,
                commitment: hex::encode(commitment),
            }),
        };
        let attempt = format!("{key}/{}/{}", e.epoch, now / EPOCH_RETRY);
        let a = self.link.epoch(&attempt, &body).await?;
        let verdict = epoch_verdict(&a);
        tracing::info!(epoch = e.epoch, verdict = %verdict, "the new sequence reported to the core");
        self.store
            .put_report(&ReportRow {
                key,
                kind: "epoch".into(),
                body: serde_json::to_value(&body).unwrap_or_default(),
                verdict: Some(verdict.clone()),
                sent_at: now,
                answered_at: Some(now),
            })
            .await?;
        let answer = EpochAnswer::of(&verdict);
        self.epoch_alerts(e, &answer, &epoch_refusal_detail(&a))
            .await?;
        Ok(answer)
    }

    /// The alerts of the core's answer to the sequence `e`: cleared once it follows it; `epoch:`
    /// (pages) for a refusal; `epoch-unsupported:` (does not page) for a core that cannot take it.
    async fn epoch_alerts(
        &mut self,
        e: &crate::chain::ChainEpoch,
        answer: &EpochAnswer,
        named: &str,
    ) -> StepResult<()> {
        let instance = self.config.instance;
        let now = self.now();
        let refused = format!("{EPOCH_ALERT}:{instance}:{}", e.epoch);
        let unsupported = format!("{EPOCH_UNSUPPORTED_ALERT}:{instance}:{}", e.epoch);
        match answer {
            // Every epoch alert of the instance, an earlier sequence's included: the core follows
            // the latest.
            EpochAnswer::Confirmed => {
                let prefixes = epoch_alert_prefixes(instance);
                for a in self.store.alerts().await? {
                    if a.cleared_at.is_none()
                        && prefixes.iter().any(|p| a.key.starts_with(p.as_str()))
                    {
                        self.store.clear(&a.key, now).await?;
                    }
                }
            }
            // Another book's reset: the stop until `refound` (`reset:`) is the alert.
            EpochAnswer::Refused(why) if why == "other_book" => {}
            EpochAnswer::Refused(why) => {
                let detail = format!(
                    "the core refused to follow sequence {} of the instance (reset at block #{}): \
                     {why}{named}. Nothing of the new chain is reported to it, and the adapter \
                     declares itself not serving, so new withdrawals are refused; the start is \
                     reported again every ten minutes. See ops/runbooks/gap-filling.md, \"After a \
                     reset\"",
                    e.epoch, e.at
                );
                if self.store.raise(&refused, &detail, now).await? {
                    tracing::error!(alert = %refused, detail = %detail, "the core refused a new sequence");
                }
            }
            EpochAnswer::Unsupported(code) => {
                let detail = format!(
                    "the core cannot take sequence {} of the instance (reset at block #{}): {code}. \
                     Its code is before amendment 0041's generation, or ingress's \
                     `settlement_epochs` is off; the adapter declares itself not serving, so new \
                     withdrawals are refused. The start is reported again every ten minutes, and \
                     taken once the core can take it",
                    e.epoch, e.at
                );
                if self.store.raise(&unsupported, &detail, now).await? {
                    tracing::warn!(alert = %unsupported, detail = %detail, "the core cannot take a new sequence yet");
                }
            }
        }
        Ok(())
    }

    /// Keep why `reset:` is raised (the delta review's finding 6): [`Adapter::retry_epoch`] follows
    /// only a stop the core may still lift.
    async fn reset_reason(&self, reason: ResetReason) -> StepResult<()> {
        self.store
            .put_report(&ReportRow {
                key: reset_reason_key(self.config.instance),
                kind: "reset".into(),
                body: serde_json::json!({ "reason": reason.as_str() }),
                verdict: None,
                sent_at: self.now(),
                answered_at: None,
            })
            .await
            .map_err(Into::into)
    }

    /// Why `reset:` was raised last ([`Adapter::reset_reason`]); none if unknown.
    async fn stored_reset_reason(&self) -> StepResult<Option<ResetReason>> {
        Ok(self
            .store
            .report(&reset_reason_key(self.config.instance))
            .await?
            .and_then(|r| r.body["reason"].as_str().and_then(ResetReason::of)))
    }

    /// How many verified positions the core has not taken in: `[cursor, verified)`.
    async fn unreported(&self) -> StepResult<u64> {
        let instance = self.config.instance;
        let Some(v) = self.store.verified_head(instance).await? else {
            return Ok(0);
        };
        let cursor = self.store.cursor(instance).await?;
        Ok(v.position.saturating_sub(cursor))
    }

    /// Report the verified entries the core has not taken in, from the cursor ([`Adapter::observe`]'s
    /// step 3, alone): before a reset is followed, so none is dropped with the chain before it.
    async fn report_verified(&mut self, fin: &BlockRef) -> StepResult<usize> {
        let instance = self.config.instance;
        let Some(v) = self.store.verified_head(instance).await? else {
            return Ok(0);
        };
        let accepted = self.store.accepted_head(instance).await?;
        let kept: BTreeMap<u64, Vec<u8>> = self
            .store
            .chain_entries(instance, accepted.map_or(0, |a| a.position), v.position)
            .await?
            .into_iter()
            .collect();
        self.report_kept(fin, v, accepted, &kept).await
    }

    /// A reset found live, past the verified head, with nothing of the chain before it left
    /// unreported (amendment 0041): reported to the core, and once the core holds it, the
    /// adapter's cursor and heads are reset to the new chain's start, as `refound` does, and the
    /// next step starts on it. `false` if the core does not follow it: the caller stops as before.
    async fn follow_reset(&mut self, fin: &BlockRef) -> StepResult<bool> {
        let instance = self.config.instance;
        let Some(e) = self.chain.chain_epoch(fin, instance).await? else {
            return Ok(false);
        };
        // The entries verified before the reset that the core has not taken in (a core outage, a
        // refused observation, a crash between the two): reported first, or the reset is not
        // followed. `reset_chain` would drop them.
        if self.unreported().await? > 0 {
            let fin = *fin;
            self.report_verified(&fin).await?;
            if self.unreported().await? > 0 {
                return Ok(false);
            }
        }
        if self.report_epoch(&e).await? != EpochAnswer::Confirmed {
            return Ok(false);
        }
        self.store.reset_chain(instance, 0).await?;
        tracing::warn!(
            instance,
            epoch = e.epoch,
            at = e.at,
            "the instance was reset and the core follows the new chain: the cursor and heads start \
             on it"
        );
        Ok(true)
    }

    /// While a reset stops the adapter (`reset:`), a start the core refused, or could not take,
    /// is reported again when due (amendment 0041); once the core holds it, the stop is lifted and
    /// the adapter starts on the new chain. A stop with no such alert (another book's reset, or
    /// entries left unreported) waits for the operator, as before.
    async fn retry_epoch(&mut self, fin: &BlockRef) -> StepResult<()> {
        let instance = self.config.instance;
        let prefixes = epoch_alert_prefixes(instance);
        let refused = self.store.alerts().await?.iter().any(|a| {
            a.cleared_at.is_none() && prefixes.iter().any(|p| a.key.starts_with(p.as_str()))
        });
        // A reset not followed because verified entries were not taken in yet: tried again, as
        // they are reported first.
        let unreported = self.unreported().await? > 0;
        if !refused && !unreported {
            return Ok(());
        }
        // Only a stop the core may still lift is retried: one for a start not taken, or for
        // verified entries not taken in yet. Never one for entries never read, a reset whose chain
        // before could not be read up to it, or another book's.
        if !matches!(
            self.stored_reset_reason().await?,
            Some(ResetReason::NotFollowed | ResetReason::Unreported)
        ) {
            return Ok(());
        }
        if self.follow_reset(fin).await? {
            let now = self.now();
            self.store.clear(&format!("reset:{instance}"), now).await?;
        }
        Ok(())
    }

    /// Whether the core follows the sequence the verified head is on (amendment 0041): an instance
    /// never reset has none to follow; otherwise the core's answer, asked once per sequence and
    /// again every ten minutes while it refuses. While it refuses, nothing of the new chain is
    /// reported (`false`); a refusal for another book is a reset under the adapter (`reset:`, until
    /// `refound`). A core that cannot take the input does not stop the adapter. This is how a
    /// sequence the adapter moved to on its first start reaches the core (test's, on 28 Oct).
    async fn epoch_followed(&mut self, fin: &BlockRef) -> StepResult<bool> {
        let instance = self.config.instance;
        let Some(v) = self.store.verified_head(instance).await? else {
            return Ok(true);
        };
        if v.epoch == 0 {
            return Ok(true);
        }
        // The answer kept, while it holds: no chain read, so an idle sync stays one read.
        if let Some(r) = self.store.report(&epoch_key(instance)).await? {
            if r.body["epoch"].as_u64() == Some(u64::from(v.epoch)) {
                if let (Some(verdict), Some(at)) = (r.verdict.as_deref(), r.answered_at) {
                    let a = EpochAnswer::of(verdict);
                    if a == EpochAnswer::Confirmed || self.now() < at.saturating_add(EPOCH_RETRY) {
                        return Ok(matches!(
                            a,
                            EpochAnswer::Confirmed | EpochAnswer::Unsupported(_)
                        ));
                    }
                }
            }
        }
        // A sequence later than the verified head's is the sync's: it finds the reset first.
        let Some(e) = self
            .chain
            .chain_epoch(fin, instance)
            .await?
            .filter(|e| e.epoch == v.epoch)
        else {
            return Ok(true);
        };
        Ok(match self.report_epoch(&e).await? {
            EpochAnswer::Confirmed | EpochAnswer::Unsupported(_) => true,
            EpochAnswer::Refused(why) if why == "other_book" => {
                let now = self.now();
                self.reset_reason(ResetReason::OtherBook).await?;
                let key = format!("reset:{instance}");
                let detail = format!(
                    "sequence {} of the instance (reset at block #{}) names another book than the \
                     one this adapter serves: nothing of its chain is reported. Restart the adapter \
                     for the re-founded book with `aledgent-settle <config> refound`",
                    e.epoch, e.at
                );
                if self.store.raise(&key, &detail, now).await? {
                    tracing::error!(alert = %key, detail = %detail, "a reset for another book");
                }
                false
            }
            EpochAnswer::Refused(_) => false,
        })
    }

    /// A first start on the chain (no head verified yet): where the cursor kept in the store stands
    /// against the chain's start `start`. `Ok(epoch)` to go on, with the instance's anchor
    /// sequence; `Err` with the alert raised. A cursor past the start is moved back only if it is
    /// a chain's before the instance's reset (the reset by this code starts at 0); a cursor behind
    /// it is never moved forward: positions the earlier code wrote and nobody read are the
    /// operator's to record as a gap.
    async fn first_start(
        &mut self,
        fin: &BlockRef,
        start: u64,
        head: &ChainHead,
    ) -> StepResult<Result<u32, ()>> {
        let instance = self.config.instance;
        let now = self.now();
        let epoch = self.chain.chain_epoch(fin, instance).await?;
        let cursor = self.store.cursor(instance).await?;
        if cursor > start {
            if start == 0 && epoch.is_some_and(|e| e.from_seq == 0) {
                tracing::warn!(
                    cursor,
                    start,
                    "a cursor kept from before the instance's reset is reset to the chain's start"
                );
                self.store.reset_chain(instance, start).await?;
            } else {
                self.store
                    .raise(
                        &format!("{CHAIN_ALERT}:{instance}:{start}"),
                        &format!(
                            "the cursor stands at {cursor}, past the chain's start at {start}, on an \
                             instance not reset since: it was not this chain's, and is not moved"
                        ),
                        now,
                    )
                    .await?;
                return Ok(Err(()));
            }
        } else if cursor < start {
            self.store
                .raise(
                    &format!("unreadable:{instance}:{cursor}"),
                    &format!(
                        "positions [{cursor}, {start}) were written before the chain's start by the \
                         contract's earlier code and never read; the cursor is not moved past \
                         them. Record them as a gap (`aledgent-settle <config> gap {cursor} {}`)",
                        head.position
                    ),
                    now,
                )
                .await?;
            return Ok(Err(()));
        }
        Ok(Ok(epoch.map_or(0, |e| e.epoch)))
    }

    /// The sync's first half (amendment 0040): fold what the layer holds since the verified head,
    /// and keep what it shows complete, entries and head, before anything is reported. However long
    /// the core takes them in, or however long it is down (a step whose core cannot be reached runs
    /// this alone), the next sync folds only the blocks after. The chain's head at `fin`, or `None`
    /// when nothing goes to the core: the instance unregistered, or reset under the adapter.
    async fn verify(&mut self, fin: &BlockRef) -> StepResult<Option<ChainHead>> {
        let instance = self.config.instance;
        let now = self.now();
        if self.refounding().await? {
            // A reset the core may still follow (amendment 0041): reported again when due.
            if self.config.report_resets {
                self.retry_epoch(fin).await?;
            }
            return Ok(None);
        }
        let Some(head) = self.chain.chain_head(fin, instance).await? else {
            self.store
                .raise("instance", "the instance is not registered", now)
                .await?;
            return Ok(None);
        };
        let verified = self.store.verified_head(instance).await?;
        // The first new position: the verified head's, or the chain's start.
        let from = verified.map_or(head.start, |v| v.position);
        // 1. The sync. What it shows complete is kept, entries and head, before anything is
        // reported: however long the core takes them in, the next sync folds only the blocks after.
        match self.sync(fin, &head, verified).await? {
            Ok(Synced::Idle) => {
                if let Some(v) = verified.filter(|v| fin.number > v.block) {
                    let v = AcceptedHead {
                        block: fin.number,
                        ..v
                    };
                    self.store.verify_head(instance, &v, &[]).await?;
                }
                self.cleared(from).await?;
            }
            Ok(Synced::New { entries }) => {
                let epoch = match verified {
                    Some(v) => Ok(v.epoch),
                    None => self.first_start(fin, from, &head).await?,
                };
                if let Ok(epoch) = epoch {
                    let raw: Vec<(u64, Vec<u8>)> =
                        entries.into_iter().map(|(seq, e)| (seq, e.raw)).collect();
                    let v = AcceptedHead {
                        position: head.position,
                        digest: head.digest,
                        block: fin.number,
                        epoch,
                    };
                    self.store.verify_head(instance, &v, &raw).await?;
                    self.cleared(from).await?;
                }
            }
            Err(Stop::Reset(block, dropped, _)) => {
                // Amendment 0041: a reset with nothing of the chain before it left unreported is
                // reported to the core, and followed once the core holds it.
                if dropped == 0 && self.config.report_resets && self.follow_reset(fin).await? {
                    return Ok(None);
                }
                let unreported = self.unreported().await?;
                self.stopped(from, &head, Stop::Reset(block, dropped, unreported))
                    .await?;
                return Ok(None);
            }
            Err(Stop::ResetUnread(block, why)) => {
                self.stopped(from, &head, Stop::ResetUnread(block, why))
                    .await?;
                return Ok(None);
            }
            Err(stop) => self.stopped(from, &head, stop).await?,
        }
        Ok(Some(head))
    }

    /// Checks, the sync, provisional reads, and the head. Returns (reported, gaps, whether the
    /// core took the head in).
    async fn observe(
        &mut self,
        fin: &BlockRef,
        best: &BlockRef,
    ) -> StepResult<(usize, usize, bool)> {
        let instance = self.config.instance;
        let now = self.now();
        let Some(head) = self.verify(fin).await? else {
            return Ok((0, 0, false));
        };
        // Amendment 0041: nothing of a new chain goes to a core that refused to follow it.
        if self.config.report_resets && !self.epoch_followed(fin).await? {
            return Ok((0, 0, false));
        }
        let verified = self.store.verified_head(instance).await?;
        let accepted = self.store.accepted_head(instance).await?;
        let verified_to = verified.map_or(0, |v| v.position);
        // The entries verified and not yet accepted.
        let mut kept = BTreeMap::new();
        for (seq, raw) in self
            .store
            .chain_entries(instance, accepted.map_or(0, |a| a.position), verified_to)
            .await?
        {
            kept.insert(seq, raw);
        }
        // 2. What was reported provisionally and is now below the finality point: its position is
        // in the verified chain, or the finalized head has passed the block that wrote it. In the
        // second case a position the finalized chain has not reached was never written by that
        // block on the finalized chain, and never will be (the entry's digest binds its block):
        // the record is orphaned (`F-027`: a batch whose era's birth block was reorged away
        // otherwise leaves its provisional records unchecked for as long as the chain is idle).
        // A position the sync could not show complete waits for it.
        //
        // Every record that no longer matches is reported before any that does (`F-027`): a
        // transaction whose era is born at the finalized head survives a reorganisation and can
        // be included again at the same position in another block, leaving two provisional
        // records of one operation at one position. The core rebinds the operation to the
        // surviving record when the first is orphaned; were the survivor already made final by an
        // earlier check, nothing would promote the operation afterwards.
        let mut checks = Vec::new();
        for obs in self.store.observations().await? {
            if obs.finalized {
                continue;
            }
            let still_matches = if obs.seq < head.position {
                if obs.seq < verified_to {
                    // Below the entries kept, the final record there was taken in with another
                    // digest, or belongs to a chain before a reset.
                    Some(kept.get(&obs.seq).is_some_and(|raw| {
                        hex::encode(crate::chain::blake2_256(raw)) == obs.digest
                    }))
                } else {
                    None
                }
            } else if fin.number >= obs.at {
                Some(false)
            } else {
                None
            };
            if let Some(still_matches) = still_matches {
                checks.push((still_matches, obs));
            }
        }
        checks.sort_by_key(|(still_matches, obs)| (*still_matches, obs.seq));
        for (still_matches, obs) in checks {
            let c = CoordinateBody {
                instance,
                seq: obs.seq,
                digest: obs.digest.clone(),
            };
            self.report_check(&c, fin.number, still_matches).await?;
            // `NFR-15`: when a provisional entry became final for the core.
            tracing::info!(
                seq = obs.seq,
                at = obs.at,
                finalized = fin.number,
                still_matches,
                operation = obs.operation.as_deref().unwrap_or("-"),
                "provisional observation checked at the finalized head"
            );
            let mut done = obs.clone();
            done.finalized = true;
            // Marks the provisional row as settled either way.
            self.store.insert_observation(&done).await?;
        }
        // 3. The verified entries the core has not taken in, from the store, in order from the
        // cursor; the verified head is accepted once the core has taken in every one.
        let mut reported = match verified {
            Some(v) => self.report_kept(fin, v, accepted, &kept).await?,
            None => 0,
        };
        // 4. The adapter's own extrinsics seen above the finality point: provisional. Read only
        // while one of its withdrawals is in flight, so an idle sync reads the head alone.
        if best.number > fin.number {
            let ours: BTreeSet<String> = self
                .store
                .instructions()
                .await?
                .into_iter()
                .filter(|r| {
                    matches!(
                        r.state,
                        InstructionState::Signed | InstructionState::Broadcast
                    )
                })
                .map(|r| r.operation)
                .collect();
            if !ours.is_empty() {
                let mut blocks = Vec::new();
                let mut at = *best;
                while at.number > fin.number {
                    let b = self.chain.block_events(&at, instance).await?;
                    at = BlockRef {
                        number: at.number - 1,
                        hash: b.parent,
                    };
                    blocks.push(b);
                }
                'blocks: for b in blocks.into_iter().rev() {
                    for ev in b.events {
                        let (s, e) = match ev {
                            ChainEvent::Entry { seq, entry } => (seq, entry),
                            // A reset above finality: what follows is another chain's.
                            ChainEvent::Reset => break 'blocks,
                            ChainEvent::Legacy { .. } => continue,
                        };
                        if s < head.position {
                            continue;
                        }
                        let row = self.observation_row(s, &e, false);
                        if !row.operation.as_ref().is_some_and(|o| ours.contains(o)) {
                            continue;
                        }
                        if self.store.insert_observation(&row).await? {
                            let body = self.observation_body(s, &e, false);
                            self.report_observation(&body).await?;
                            reported += 1;
                        }
                    }
                }
            }
        }
        // 5. The finalised head: expiry and non-inclusion are the core's to decide from it, with
        // the nonce of every signing key read at the same finalized block (the owner's ruling of
        // 2026-09-24). Its `next_seq` is the chain's position, which the contract keeps equal to
        // its head's.
        let mut nonces = Vec::new();
        for l in self.lanes.iter().chain(self.anchor_signer.iter()) {
            let key = l.account();
            if nonces
                .iter()
                .any(|n: &KeyNonceBody| n.key == hex::encode(key))
            {
                continue;
            }
            nonces.push(KeyNonceBody {
                key: hex::encode(key),
                nonce: self.chain.account_nonce(fin, &key).await?,
            });
        }
        let head = HeadBody {
            height: fin.number,
            next_seq: head.position,
            nonces,
        };
        let key = format!(
            "settle/head/{}/{}/{}",
            head.height,
            head.next_seq,
            crate::chain::blake2_256(&serde_json::to_vec(&head.nonces).unwrap_or_default())
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let verdict = match self.store.report(&key).await?.and_then(|r| r.verdict) {
            Some(v) => v,
            None => {
                self.report_start(
                    &key,
                    "head",
                    serde_json::to_value(&head).unwrap_or_default(),
                )
                .await?;
                let a = self.link.head(&key, &head).await?;
                self.report_end(&key, &a).await?;
                a.verdict()
            }
        };
        let taken = verdict == "decided";
        if !taken {
            self.store
                .raise(
                    "head",
                    &format!("the core did not take the finalised head in: {verdict}"),
                    now,
                )
                .await?;
        } else {
            self.store.clear("head", now).await?;
        }
        Ok((reported, 0, taken))
    }

    /// Clear the alerts of a range that stopped the sync at `from`: a read shows it complete now.
    async fn cleared(&mut self, from: u64) -> StepResult<()> {
        let instance = self.config.instance;
        let now = self.now();
        for key in [
            format!("{CHAIN_ALERT}:{instance}:{from}"),
            format!("unreadable:{instance}:{from}"),
        ] {
            self.store.clear(&key, now).await?;
        }
        Ok(())
    }

    /// A sync that could not show the new entries complete (amendment 0040): an alert naming the
    /// first new position, which nothing passes until a read shows the range complete or the
    /// operator records it as a gap ([`Adapter::record_operator_gap`]). The rest of the step goes on:
    /// submitting and anchoring do not wait for observation (`REQ-PR-9`).
    async fn stopped(&mut self, from: u64, head: &ChainHead, stop: Stop) -> StepResult<()> {
        let instance = self.config.instance;
        let now = self.now();
        let to = head.position;
        if let Stop::Reset(_, dropped, unreported) = &stop {
            self.reset_reason(if *dropped > 0 {
                ResetReason::Dropped
            } else if *unreported > 0 {
                ResetReason::Unreported
            } else {
                ResetReason::NotFollowed
            })
            .await?;
        }
        let (key, detail) = match stop {
            Stop::Mismatch(why) => (
                format!("{CHAIN_ALERT}:{instance}:{from}"),
                format!(
                    "the settlement layer's events do not lead from the head accepted at position \
                     {from} to its head at {to}, read twice: {why}. Nothing past position {from} \
                     is reported until a read shows the range complete, or the operator records it \
                     as a gap (`aledgent-settle <config> gap {from} {to}`)"
                ),
            ),
            Stop::Unreadable(why) => (
                format!("unreadable:{instance}:{from}"),
                format!(
                    "positions [{from}, {to}) cannot be read: {why}. A history endpoint \
                     (`history_endpoint`) catches up; else the operator records the range as a \
                     gap (`aledgent-settle <config> gap {from} {to}`)"
                ),
            ),
            Stop::ResetUnread(block, why) => {
                self.reset_reason(ResetReason::Unread).await?;
                (
                    format!("reset:{instance}"),
                    format!(
                        "the instance was reset at block #{block} (its anchor sequence moved past the \
                         verified head's), and the chain before it could not be read up to the reset \
                         from position {from}: {why}. The reset is not followed, and nothing more is \
                         reported; do not record a gap (it would take the new chain's head as the old \
                         one's). The entries of the chain before it past position {from} cannot reach \
                         the book from this chain: escalate to the owner, who re-founds \
                         (`aledgent-settle <config> refound`), or give the adapter a history endpoint \
                         that reaches back to position {from}"
                    ),
                )
            }
            Stop::Reset(block, dropped, unreported) => (
                format!("reset:{instance}"),
                if dropped > 0 {
                    format!(
                        "the instance was reset at block #{block} (a new chain from position 0), \
                         and {dropped} entries of the chain before it past the verified head were \
                         never read: the reset is not followed, and nothing more is reported. The \
                         book cannot take them from this chain any more. If the book was \
                         re-founded, restart the adapter for the new book with \
                         `aledgent-settle <config> refound`"
                    )
                } else if unreported > 0 {
                    format!(
                        "the instance was reset at block #{block} (a new chain from position 0), \
                         and {unreported} entries of the chain before it, verified below position \
                         {from}, were not taken in by the core: the reset is not followed. With \
                         `report_resets` on they are reported first, every step (the `report:` \
                         alerts say why the core does not take them); once taken they are final \
                         records at positions the new chain reuses, so the start is then refused \
                         (`earlier_final`) and the book needs a re-founding. `refound` drops them"
                    )
                } else if !self.config.report_resets {
                    format!(
                        "the instance was reset at block #{block} (a new chain from position 0). \
                         `report_resets` is off, so the core was not told; nothing of the new \
                         chain is reported. If the book was re-founded, restart the adapter for \
                         the new book with `aledgent-settle <config> refound`"
                    )
                } else {
                    format!(
                        "the instance was reset at block #{block} (a new chain from position 0), \
                         and the core does not follow it: the reset names another book, or the core \
                         refused it or cannot take it yet (the `epoch` alerts say which). Nothing of \
                         the new chain is reported. If the book was re-founded, restart the adapter \
                         for the new book with `aledgent-settle <config> refound`"
                    )
                },
            ),
        };
        if self.store.raise(&key, &detail, now).await? {
            tracing::error!(alert = %key, detail = %detail, "the adapter stops at the first new position");
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------ operator

    /// **The operator's gap** (amendment 0040, ruling a40-q3): when no reader can serve the range
    /// the adapter stopped at, or a mismatch does not clear, the operator has the adapter record it
    /// through its own source, the only one the core takes a gap from (`record_gap`), and the
    /// adapter then takes the layer's head as it stands as verified and accepted, unverified for
    /// the range. `[from, to)` is the range the alert names: `from` must be the position the
    /// adapter stopped at, and `to` at most the head's at the finalized block now. The gap
    /// recorded runs to that head, which may have moved since the alert; it is returned. Anything
    /// else is refused and nothing is written; so is a gap while the core has not taken in the
    /// entries already verified, or while a reset is open. The `gap:` alert the gap raises is
    /// cleared with it: it is the operator's own act, recorded in the core and the store. Holds
    /// the lease, as a step does.
    pub async fn record_operator_gap(&mut self, from: u64, to: u64) -> StepResult<(u64, u64)> {
        self.hold_lease().await?;
        let instance = self.config.instance;
        if self.refounding().await? {
            return Err(AdapterError::Refused(
                "the instance was reset under this adapter: `refound` it first".into(),
            ));
        }
        let fin = self.chain.finalized().await?;
        let head = self
            .chain
            .chain_head(&fin, instance)
            .await?
            .ok_or_else(|| AdapterError::Refused("the instance is not registered".into()))?;
        let epoch = self.chain.chain_epoch(&fin, instance).await?;
        let cursor = self.store.cursor(instance).await?;
        // A reset under the verified head (the delta review's finding 1): a gap would take the new
        // chain's head as the old one's and drop the new chain's entries below it.
        if let Some(v) = self.store.verified_head(instance).await? {
            let now_on = epoch.map_or(0, |e| e.epoch);
            if now_on != v.epoch {
                return Err(AdapterError::Refused(format!(
                    "the instance was reset under the verified head (sequence {} verified, {now_on} \
                     on the chain): no gap spans a reset. Step the adapter first: it stops as a \
                     reset (`reset:`)",
                    v.epoch
                )));
            }
        }
        // Where the adapter stopped, and whether a cursor from before the instance's reset goes
        // back to the chain's start first.
        let (stopped_at, reset_cursor) = match self.store.verified_head(instance).await? {
            Some(v) if cursor < v.position => {
                return Err(AdapterError::Refused(format!(
                    "the core has not taken in the verified positions [{cursor}, {}) yet: they \
                     are reported first",
                    v.position
                )));
            }
            Some(v) => (v.position, false),
            None if cursor <= head.start => (cursor, false),
            None if head.start == 0 && epoch.is_some_and(|e| e.from_seq == 0) => (0, true),
            None => {
                return Err(AdapterError::Refused(format!(
                    "the cursor stands at {cursor}, past the chain's start at {}, on an instance \
                     not reset since",
                    head.start
                )));
            }
        };
        if from != stopped_at || from >= to || to > head.position {
            return Err(AdapterError::Refused(format!(
                "the adapter stands at position {stopped_at} and the head at {} (block #{}): a \
                 gap starts at {stopped_at} and names a range up to the head, not [{from}, {to})",
                head.position, fin.number
            )));
        }
        let to = head.position;
        if reset_cursor {
            self.store.reset_chain(instance, 0).await?;
        }
        // Amendment 0041, decision 6 made exact: the gap is of the chain at the head, whose
        // sequence the core keeps it under, so a start of that sequence does not clear it. Named
        // only with `report_resets` on (ingress drops it with its own switch off).
        let sequence = self
            .config
            .report_resets
            .then(|| epoch.map(|e| e.epoch))
            .flatten();
        self.record_gap(from, to, GapReasonBody::Unreadable, sequence)
            .await?;
        self.cleared(from).await?;
        let now = self.now();
        self.store
            .clear(&format!("gap:{instance}:{from}"), now)
            .await?;
        let h = AcceptedHead {
            position: head.position,
            digest: head.digest,
            block: fin.number,
            epoch: epoch.map_or(0, |e| e.epoch),
        };
        self.store.verify_head(instance, &h, &[]).await?;
        self.store.accept_head(instance, &h).await?;
        tracing::warn!(
            from,
            to,
            block = fin.number,
            "the operator's gap: the head is accepted unverified for the range"
        );
        Ok((from, to))
    }

    /// **The re-founding's reset** (amendment 0040): the book this adapter serves was re-founded
    /// with its instance's reset, and the adapter is started for the new book. Its cursor, the heads
    /// it verified and accepted and the entries it kept belong to the chain before; all are reset,
    /// and the next step starts on the current chain from its start. Until then nothing goes to the
    /// book (no observation, head, anchor or position cut). Holds the lease, as a step does.
    pub async fn refound(&mut self) -> StepResult<()> {
        self.hold_lease().await?;
        let instance = self.config.instance;
        self.store.reset_chain(instance, 0).await?;
        let now = self.now();
        self.store.clear(&format!("reset:{instance}"), now).await?;
        // The re-founded book's core is asked about the sequence afresh (amendment 0041).
        if let Some(mut r) = self.store.report(&epoch_key(instance)).await? {
            r.verdict = None;
            r.answered_at = None;
            self.store.put_report(&r).await?;
        }
        for a in self.store.alerts().await? {
            if a.cleared_at.is_none()
                && (a.key.starts_with(&format!("{EPOCH_ALERT}:{instance}:"))
                    || a.key
                        .starts_with(&format!("{EPOCH_UNSUPPORTED_ALERT}:{instance}:")))
            {
                self.store.clear(&a.key, now).await?;
            }
        }
        tracing::warn!(
            instance,
            "the adapter's cursor and accepted head are reset with the book's re-founding"
        );
        Ok(())
    }

    /// Record with the core what was done about signing `row`'s instruction, once: the stored
    /// transaction's key, nonce and last valid block, or a tombstone. Returns whether the core
    /// holds it; nothing is broadcast until it does. A refusal raises one alert.
    async fn record_signing(
        &mut self,
        row: &InstructionRow,
        record: SigningRecordBody,
    ) -> StepResult<bool> {
        let key = format!("settle/sign/{}", row.operation);
        let verdict = match self.store.report(&key).await?.and_then(|r| r.verdict) {
            Some(v) => v,
            None => {
                let body = SigningBody {
                    operation_reference: row.operation.clone(),
                    instruction: row.digest.clone(),
                    record,
                };
                self.report_start(
                    &key,
                    "sign",
                    serde_json::to_value(&body).unwrap_or_default(),
                )
                .await?;
                let a = self.link.sign(&key, &body).await?;
                self.report_end(&key, &a).await?;
                a.verdict()
            }
        };
        if verdict == "decided" {
            return Ok(true);
        }
        self.store
            .raise(
                &format!("sign:{}", row.operation),
                &format!("the core did not record the signing: {verdict}; nothing is broadcast"),
                self.now(),
            )
            .await?;
        Ok(false)
    }

    /// [`Adapter::record_signing`] of the stored extrinsic `ext`.
    async fn record_signed(&mut self, ext: &ExtrinsicRow) -> StepResult<bool> {
        let Some(row) = self
            .store
            .instructions()
            .await?
            .into_iter()
            .find(|r| r.operation == ext.operation)
        else {
            return Ok(false);
        };
        let Some(signer) = self.signer_of(&ext.lane) else {
            return Ok(false);
        };
        let record = SigningRecordBody::Signed {
            key: hex::encode(signer.account()),
            nonce: ext.nonce,
            last_valid: ext.era.last_valid(),
        };
        self.record_signing(&row, record).await
    }

    // ------------------------------------------------------------------------------ submit

    /// Sign, store, record and broadcast what can be, in batches; park what cannot; tombstone
    /// what never can. Nothing new is signed unless `head_taken`. Returns (calls signed, parked,
    /// transactions broadcast).
    async fn submit(
        &mut self,
        epoch: u64,
        fin: &BlockRef,
        best: &BlockRef,
        head_taken: bool,
    ) -> StepResult<(usize, usize, usize)> {
        let now = self.now();
        let instance = self.config.instance;
        let runtime = self.chain.runtime().await?;
        // T-027-12: the runtime the step's assertions passed, unless it changed since; if it did,
        // nothing new is signed this step.
        let chosen = self.chosen(&runtime).ok();
        let max_era = self.config.max_era.min(runtime.block_hash_count).max(4);
        let extrinsics = self.store.extrinsics().await?;
        let signed_ops: BTreeSet<String> = extrinsics.iter().map(|e| e.operation.clone()).collect();
        let mut rows = self.store.instructions().await?;
        rows.sort_by(|a, b| (a.received_at, &a.operation).cmp(&(b.received_at, &b.operation)));
        // What is signed and not yet decided leaves the position when it executes.
        let mut pending: BTreeMap<String, u128> = BTreeMap::new();
        for e in &extrinsics {
            if matches!(
                e.status,
                ExtrinsicStatus::Signed | ExtrinsicStatus::Broadcast { .. }
            ) {
                if let Some(r) = rows.iter().find(|r| r.operation == e.operation) {
                    *pending.entry(r.call.asset.0.clone()).or_default() += r.call.amount;
                }
            }
        }
        // What the deployment can pay out of each asset: what its contract account holds, and
        // no more than the position its contract accounts for (`F-026` decision 2).
        let mut positions: BTreeMap<String, u128> = BTreeMap::new();
        let (mut signed, mut parked, mut broadcast) = (0, 0, 0);
        let mut sent_now: BTreeSet<[u8; 32]> = BTreeSet::new();
        let mut short_positions: BTreeSet<String> = BTreeSet::new();
        let mut short_lanes: BTreeSet<String> = BTreeSet::new();
        // 1. What is ready to go out, in order.
        let mut ready: VecDeque<InstructionRow> = VecDeque::new();
        for mut row in rows {
            // A tombstone the core does not hold yet (the ruling of 2026-09-24): recorded, so the
            // core may release it on the inbox alone.
            if row.acknowledged
                && matches!(
                    row.state,
                    InstructionState::NotExecuted {
                        proof: NotExecuted::NeverSigned { .. }
                    }
                )
            {
                self.record_signing(&row, SigningRecordBody::NeverSigned)
                    .await?;
                continue;
            }
            if !row.acknowledged
                || !matches!(
                    row.state,
                    InstructionState::Acknowledged | InstructionState::Parked { .. }
                )
                || signed_ops.contains(&row.operation)
                || !head_taken
            {
                continue;
            }
            // REQ-SL-4: an era that ends at or before the expiry, or never. It must also leave a
            // block to be included in: a head at or past its last valid block is then always a
            // head the core has not seen, and decides its nonce.
            if Mortality::within(best.number, row.expiry, max_era)
                .filter(|m| m.last_valid() > best.number)
                .is_none()
            {
                row.state = InstructionState::NotExecuted {
                    proof: NotExecuted::NeverSigned { at: best.number },
                };
                row.updated_at = now;
                self.store.put_instruction(&row).await?;
                // Stored first: it is never signed after this, whatever the core answers.
                self.record_signing(&row, SigningRecordBody::NeverSigned)
                    .await?;
                continue;
            }
            // Liquidity at the finalized head, less what is pending (REQ-SN-2).
            let asset = row.call.asset.clone();
            let position = match positions.get(&asset.0) {
                Some(p) => *p,
                None => {
                    let held = self.chain.position(fin, instance, &asset).await?;
                    let recorded = self.chain.recorded_position(fin, instance, &asset).await?;
                    let p = held.min(recorded);
                    positions.insert(asset.0.clone(), p);
                    p
                }
            };
            let pending_now = pending.get(&asset.0).copied().unwrap_or(0);
            if position.saturating_sub(pending_now) < row.call.amount {
                short_positions.insert(asset.0.clone());
                if !matches!(
                    row.state,
                    InstructionState::Parked {
                        reason: ParkReason::Liquidity,
                        ..
                    }
                ) {
                    row.state = InstructionState::Parked {
                        reason: ParkReason::Liquidity,
                        since: now,
                    };
                    row.updated_at = now;
                    self.store.put_instruction(&row).await?;
                    parked += 1;
                }
                continue;
            }
            *pending.entry(asset.0.clone()).or_default() += row.call.amount;
            ready.push_back(row);
        }
        // 2. Each free lane whose fees are covered takes a batch (`F-027` T-027-02): up to the
        // contract's limit, in one `execute` transaction.
        let mut any_free = false;
        let mut any_funded = false;
        for l in self.lanes.clone() {
            if ready.is_empty() {
                break;
            }
            let Some(chosen) = chosen.clone() else {
                break;
            };
            let key = Self::lane_key(l.as_ref());
            let lane = self.store.lane(&key).await?;
            if lane.as_ref().is_some_and(|x| x.in_flight.is_some()) {
                continue;
            }
            any_free = true;
            if self.chain.native_balance(fin, &l.account()).await? < self.config.fee_floor {
                short_lanes.insert(key);
                continue;
            }
            any_funded = true;
            let take = ready.len().min(crate::chain::kreivo::MAX_CALLS);
            let candidates: Vec<InstructionRow> = ready.drain(..take).collect();
            // The era is born at the finalized head (the context of its PASS challenge too), and
            // ends at or before the earliest expiry in the batch and within the challenge's
            // window.
            let window = fin.number.saturating_add(self.config.challenge_window);
            let era_for = |rows: &[InstructionRow]| {
                let expiry = rows.iter().map(|r| r.expiry).min().unwrap_or(0);
                let bound = expiry.min(window);
                Mortality::born_at_finalized(fin.number, best.number, bound, max_era)
            };
            if era_for(&candidates).is_none() {
                // Finality lags the best head by more than any era the expiries allow: they
                // wait for the next step (a later finalized head), or for their tombstone.
                for r in candidates.into_iter().rev() {
                    ready.push_front(r);
                }
                break;
            }
            // Sized and checked by dry runs (`F-027`, finding 2 of the first zombienet runs).
            let fitted = self.fit_batch(fin, &l.account(), candidates, &key).await?;
            for r in fitted.rest.into_iter().rev() {
                ready.push_front(r);
            }
            for (row, why) in fitted.held {
                self.hold_for_review(row, &why).await?;
            }
            let batch = fitted.batch;
            let limits = fitted.limits;
            if batch.is_empty() {
                continue;
            }
            // The lane holds the batch's storage deposit, as its dry run put it, besides its fees
            // (REQ-SN-2): short of that, the batch waits.
            let deposit = limits.storage_deposit_limit.unwrap_or(0);
            if self.chain.native_balance(fin, &l.account()).await?
                < self.config.fee_floor.saturating_add(deposit)
            {
                short_lanes.insert(key);
                for r in batch.into_iter().rev() {
                    ready.push_front(r);
                }
                continue;
            }
            // Fewer calls, so no earlier expiry: an era still exists.
            let Some(era) = era_for(&batch) else {
                for r in batch.into_iter().rev() {
                    ready.push_front(r);
                }
                break;
            };
            let chain_nonce = self.chain.account_nonce(fin, &l.account()).await?;
            let nonce = lane.as_ref().map_or(0, |x| x.next_nonce).max(chain_nonce);
            let calls: Vec<BatchCall> =
                batch.iter().map(|r| BatchCall::withdraw(&r.call)).collect();
            let ext = self
                .chain
                .sign_batch(
                    instance,
                    &calls,
                    l.as_ref(),
                    nonce,
                    &era,
                    fin,
                    &limits,
                    &chosen.signing,
                )
                .await?;
            // T-027-12: the layer's confirmation before the first broadcast on a runtime. Refused,
            // the batch is never stored: its calls wait, and nothing more is signed.
            if !self.confirmed(&chosen, &ext).await? {
                for r in batch.into_iter().rev() {
                    ready.push_front(r);
                }
                break;
            }
            self.fault(CrashPoint::AfterSign)?;
            let ops: Vec<String> = batch.iter().map(|r| r.operation.clone()).collect();
            let ext_rows: Vec<ExtrinsicRow> = batch
                .iter()
                .map(|r| ExtrinsicRow {
                    operation: r.operation.clone(),
                    lane: key.clone(),
                    nonce,
                    era,
                    extrinsic: ext.clone(),
                    status: ExtrinsicStatus::Signed,
                    signed_at: now,
                    batch: ops.clone(),
                })
                .collect();
            // REQ-SA-6: stored before it is broadcast, every row or none.
            match self
                .store
                .store_batch(
                    epoch,
                    &ext_rows,
                    &LaneRow {
                        lane: key.clone(),
                        next_nonce: nonce + 1,
                        in_flight: ops.first().cloned(),
                    },
                    &batch,
                )
                .await
            {
                Ok(()) => {}
                Err(StoreError::Duplicate) => continue,
                Err(e) => return Err(e.into()),
            }
            signed += batch.len();
            tracing::info!(
                calls = batch.len(),
                lane = %key,
                nonce,
                era_birth = era.birth,
                era_death = era.death(),
                gas_proof_size = limits.gas_limit.proof_size,
                gas_ref_time = limits.gas_limit.ref_time,
                storage_deposit_limit = deposit,
                hash = %hex::encode(ext.hash),
                "batch signed and stored"
            );
            self.fault(CrashPoint::AfterPersist)?;
            // The ruling of 2026-09-24: the core records every operation's key and nonce before
            // anything is broadcast. If it does not hold them all, the bytes stay stored and unsent
            // until their era ends.
            if !self.record_batch(&ext_rows).await? {
                continue;
            }
            self.chain.submit(&ext).await?;
            self.fault(CrashPoint::AfterBroadcast)?;
            for r in &ext_rows {
                self.mark_broadcast(r).await?;
            }
            sent_now.insert(ext.hash);
            broadcast += 1;
        }
        // What no lane took: parked for fees if every free lane is short, else it waits.
        if any_free && !any_funded {
            for mut row in ready {
                if !matches!(
                    row.state,
                    InstructionState::Parked {
                        reason: ParkReason::Fees,
                        ..
                    }
                ) {
                    row.state = InstructionState::Parked {
                        reason: ParkReason::Fees,
                        since: now,
                    };
                    row.updated_at = now;
                    self.store.put_instruction(&row).await?;
                    parked += 1;
                }
            }
        }
        // One alert per position, and per fee position; cleared when nothing waits on it.
        for b in &self.config.assets {
            let key = format!("position:{instance}:{}", b.chain.0);
            if short_positions.contains(&b.chain.0) {
                self.store
                    .raise(
                        &key,
                        &format!(
                            "the instance's position in {} does not cover what is instructed",
                            b.asset
                        ),
                        now,
                    )
                    .await?;
            } else {
                self.store.clear(&key, now).await?;
            }
        }
        for l in &self.lanes {
            let key = Self::lane_key(l.as_ref());
            let alert = format!("fees:{key}");
            if short_lanes.contains(&key) {
                self.store
                    .raise(
                        &alert,
                        "the lane's native balance is below the fee floor",
                        now,
                    )
                    .await?;
            } else if self.chain.native_balance(fin, &l.account()).await? >= self.config.fee_floor {
                self.store.clear(&alert, now).await?;
            }
        }
        // Broadcast every stored, uncertain transaction again: the same bytes (REQ-SO-7).
        let mut again: BTreeMap<[u8; 32], Vec<ExtrinsicRow>> = BTreeMap::new();
        for ext in self.store.extrinsics().await? {
            let uncertain = matches!(
                ext.status,
                ExtrinsicStatus::Signed | ExtrinsicStatus::Broadcast { .. }
            );
            if uncertain && best.number < ext.era.death() && !sent_now.contains(&ext.extrinsic.hash)
            {
                again.entry(ext.extrinsic.hash).or_default().push(ext);
            }
        }
        for rows in again.into_values() {
            // After a restart between storing and recording: recorded first, then broadcast.
            if !self.record_batch(&rows).await? {
                continue;
            }
            self.chain.submit(&rows[0].extrinsic).await?;
            for r in &rows {
                self.mark_broadcast(r).await?;
            }
            broadcast += 1;
        }
        Ok((signed, parked, broadcast))
    }

    /// What a batch's `Contracts::call` is given, from its own dry run (`F-027`, finding 2 of the
    /// first zombienet runs; the owner: weights known before submitting). **The one place limits
    /// are computed**: a per-call limit in the envelope, if the contract gains one, is filled here
    /// too.
    ///
    /// - **Gas**: the dry run's `gas_required`, both ref time and proof size, with the configured
    ///   margin (half again), capped at `cap`, the most a batch is given at the block it is sized
    ///   against ([`Adapter::batch_cap`]: the runtime's normal-extrinsic maximum there). A batch of
    ///   several calls fits only with its margin under the cap; a lone call (`alone`), which
    ///   cannot be made smaller, as long as what it requires is under the cap, and it is then
    ///   given the cap.
    /// - **Storage deposit**: what the dry run says the batch puts down, with the same margin, as
    ///   its limit; the lane's balance must cover it (`Adapter::submit`).
    ///
    /// `None` if the batch does not fit.
    fn batch_limits(
        &self,
        dry: &crate::chain::DryRun,
        alone: bool,
        cap: kreivo::Weight,
    ) -> Option<kreivo::CallLimits> {
        let percent = self.config.batch_gas_margin_percent;
        // Half again, and never less than the contract's per-call overhead above its dry run
        // (per-call isolation, #102): a batch of anchors only is too small for the percentage to
        // cover it.
        let by_percent = dry.gas_required.with_margin(percent);
        let by_floor = kreivo::Weight {
            ref_time: dry
                .gas_required
                .ref_time
                .saturating_add(BATCH_GAS_FLOOR.ref_time),
            proof_size: dry
                .gas_required
                .proof_size
                .saturating_add(BATCH_GAS_FLOOR.proof_size),
        };
        let with = kreivo::Weight {
            ref_time: by_percent.ref_time.max(by_floor.ref_time),
            proof_size: by_percent.proof_size.max(by_floor.proof_size),
        };
        let gas_limit = if with.fits(cap) {
            with
        } else if alone && dry.gas_required.fits(cap) {
            cap
        } else {
            return None;
        };
        let deposit = dry
            .storage_deposit
            .saturating_mul(u128::from(percent))
            .div_ceil(100);
        Some(kreivo::CallLimits {
            gas_limit,
            storage_deposit_limit: Some(deposit),
        })
    }

    /// The most gas a batch sized against `at` may be given: the runtime's normal-extrinsic
    /// maximum at `at` ([`Chain::extrinsic_limit`]; Kreivo `0.17.0`'s moves with its cores), or
    /// the configured cap if lower. `None` when the limit cannot be read: dry runs pause, as for a
    /// dry run the client could not give, and nothing is signed.
    async fn batch_cap(&mut self, at: &BlockRef) -> StepResult<Option<kreivo::Weight>> {
        match self.chain.extrinsic_limit(at).await {
            Ok(limit) => Ok(Some(match self.config.batch_gas_cap {
                Some(lower) => limit.min(lower),
                None => limit,
            })),
            Err(ChainError::Unavailable(e)) => {
                self.pause_dry_runs(&e).await?;
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Fit up to [`kreivo::MAX_CALLS`] ready withdrawals, in order, into one batch for the lane
    /// `origin` (`F-027`, finding 2 of the first zombienet runs), by dry-running `execute` at the
    /// finalized head `at`:
    ///
    /// - **One dry run in the common case**: the whole candidate batch applies, and its gas with
    ///   the margin is under the cap. That is its gas limit.
    /// - **No dry run to be had** from the client (a [`ChainError::Unavailable`] answer): nothing
    ///   is signed; the calls go back, dry runs pause for a minute ([`Adapter::pause_dry_runs`]),
    ///   and an alert says so. Nothing is held on that account.
    /// - **Otherwise it is split**: the longest prefix that applies and fits is found by
    ///   bisection (a shorter prefix needs no more proof size); it goes now, and the rest go back,
    ///   in order, to be fitted again by another lane or the next step. A batch fails as a whole
    ///   only by running out of gas, proof size or storage deposit, or by one of the contract's
    ///   invariant traps ([`BatchFailure::Trapped`]): every other problem of one call is that
    ///   call's refusal, in the inbox.
    /// - **A call that is no good alone** (its own dry run fails, or it alone needs more than the
    ///   cap) is not refused on chain, since nothing moved: it is left unsubmitted and its
    ///   operation goes to review with the dry run's error ([`Adapter::hold_for_review`]). The
    ///   rest are fitted again.
    /// - **The contract reverts the batch** ([`BatchFailure::Reverted`]: not a key, for one): not
    ///   a call's doing, so nothing is held; nothing is signed on this lane in this step, the
    ///   calls go back, and the lane's alert says why.
    ///
    /// Nothing here is signed or recorded.
    async fn fit_batch(
        &mut self,
        at: &BlockRef,
        origin: &AccountId32,
        mut candidates: Vec<InstructionRow>,
        lane: &str,
    ) -> StepResult<Fitted> {
        let instance = self.config.instance;
        let alert = format!("batch:{lane}");
        let calls = |rows: &[InstructionRow]| -> Vec<BatchCall> {
            rows.iter().map(|r| BatchCall::withdraw(&r.call)).collect()
        };
        let mut held: Vec<(InstructionRow, String)> = Vec::new();
        let mut cap_at: Option<kreivo::Weight> = None;
        loop {
            if candidates.is_empty() {
                self.store.clear(&alert, self.now()).await?;
                return Ok(Fitted {
                    batch: Vec::new(),
                    limits: kreivo::CallLimits::default(),
                    rest: Vec::new(),
                    held,
                });
            }
            let n = candidates.len();
            // No batch goes out without its dry run (the owner: weights known before
            // submitting). While the client cannot give one, nothing is signed.
            if !self.dry_runs_due().await? {
                return Ok(Fitted {
                    batch: Vec::new(),
                    limits: kreivo::CallLimits::default(),
                    rest: candidates,
                    held,
                });
            }
            let cap = match cap_at {
                Some(c) => c,
                None => match self.batch_cap(at).await? {
                    Some(c) => *cap_at.insert(c),
                    None => {
                        return Ok(Fitted {
                            batch: Vec::new(),
                            limits: kreivo::CallLimits::default(),
                            rest: candidates,
                            held,
                        });
                    }
                },
            };
            let whole = match self
                .chain
                .dry_run(at, instance, &calls(&candidates), origin)
                .await
            {
                Ok(d) => {
                    self.store.clear(DRY_RUN_ALERT, self.now()).await?;
                    d
                }
                Err(ChainError::Unavailable(e)) => {
                    self.pause_dry_runs(&e).await?;
                    return Ok(Fitted {
                        batch: Vec::new(),
                        limits: kreivo::CallLimits::default(),
                        rest: candidates,
                        held,
                    });
                }
                Err(e) => return Err(e.into()),
            };
            if whole.failure.is_none() {
                if let Some(limits) = self.batch_limits(&whole, n == 1, cap) {
                    self.store.clear(&alert, self.now()).await?;
                    return Ok(Fitted {
                        batch: candidates,
                        limits,
                        rest: Vec::new(),
                        held,
                    });
                }
            }
            // The longest good prefix: `good(lo)` (vacuously for 0), `!good(hi)`.
            let (mut lo, mut hi) = (0usize, n);
            let mut lo_limits = kreivo::CallLimits::default();
            let mut at_hi = whole;
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                let d = self
                    .chain
                    .dry_run(at, instance, &calls(&candidates[..mid]), origin)
                    .await?;
                match (&d.failure, self.batch_limits(&d, mid == 1, cap)) {
                    (None, Some(limits)) => {
                        lo = mid;
                        lo_limits = limits;
                    }
                    _ => {
                        hi = mid;
                        at_hi = d;
                    }
                }
            }
            if lo > 0 {
                // A prefix applies and fits: it goes now. The rest, from the call that tipped it
                // over, goes back to be fitted again (on its own, next).
                let rest = candidates.split_off(lo);
                self.store.clear(&alert, self.now()).await?;
                return Ok(Fitted {
                    batch: candidates,
                    limits: lo_limits,
                    rest,
                    held,
                });
            }
            // The first call alone is no good (`at_hi` is its own dry run).
            let why = match at_hi.failure {
                Some(BatchFailure::Reverted(e)) => {
                    // Not the call's doing: the contract refuses this lane's batch whatever it
                    // carries.
                    let why = format!(
                        "the contract reverts this lane's batch as a whole ({e}); nothing is signed on it"
                    );
                    tracing::warn!(lane, error = %e, "the contract reverts the batch");
                    self.store.raise(&alert, &why, self.now()).await?;
                    return Ok(Fitted {
                        batch: Vec::new(),
                        limits: kreivo::CallLimits::default(),
                        rest: candidates,
                        held,
                    });
                }
                Some(BatchFailure::Trapped(e)) => format!("alone, its dry run fails: {e}"),
                None => format!(
                    "alone it needs {:?}, more than a batch is given ({:?})",
                    at_hi.gas_required, cap
                ),
            };
            let row = candidates.remove(0);
            tracing::warn!(operation = %row.operation, why, "a call is left unsubmitted for review");
            held.push((row, why));
        }
    }

    /// Whether a dry run may be asked for now: not while they are paused
    /// ([`Adapter::pause_dry_runs`]).
    async fn dry_runs_due(&mut self) -> StepResult<bool> {
        match &self.dry_run_pause {
            Some((until, _)) if self.now() < *until => Ok(false),
            _ => {
                self.dry_run_pause = None;
                Ok(true)
            }
        }
    }

    /// The client could not give a dry run (`e`): nothing is signed without one, and none is asked
    /// for again for [`DRY_RUN_PAUSE`]. The light client bans for 40 s every peer whose call proof
    /// it could not use (10 s one that did not answer), and with one boot node per chain a ban
    /// stops every read; asking again at once would keep it banned (decision 35).
    async fn pause_dry_runs(&mut self, e: &str) -> StepResult<()> {
        tracing::warn!(error = %e, "a dry run cannot be had; nothing is signed without one");
        self.store
            .raise(
                DRY_RUN_ALERT,
                &format!("a dry run cannot be had ({e}); nothing is signed without one"),
                self.now(),
            )
            .await?;
        self.dry_run_pause = Some((self.now().saturating_add(DRY_RUN_PAUSE), e.to_owned()));
        Ok(())
    }

    /// Leave a call no batch can carry unsubmitted, for review ([`Adapter::fit_batch`]; the owner's
    /// ruling on #96): [`InstructionState::Held`], never signed after this. It is neither signed
    /// nor tombstoned with the core, so the core holds it acknowledged with no signing record:
    /// nothing is released, and the operation joins the review queue at its deadline. The dry
    /// run's error is in the store and in one alert, for the operator who reviews it.
    async fn hold_for_review(&mut self, mut row: InstructionRow, why: &str) -> StepResult<()> {
        let now = self.now();
        row.state = InstructionState::Held {
            error: why.to_owned(),
            since: now,
        };
        row.updated_at = now;
        self.store.put_instruction(&row).await?;
        self.store
            .raise(
                &format!("review:{}", row.operation),
                &format!("left unsubmitted for review: {why}"),
                now,
            )
            .await?;
        Ok(())
    }

    /// [`Adapter::record_signed`] of every operation a stored transaction carries. Returns
    /// whether the core holds them all.
    async fn record_batch(&mut self, rows: &[ExtrinsicRow]) -> StepResult<bool> {
        let mut all = true;
        for r in rows {
            if !self.record_signed(r).await? {
                all = false;
            }
        }
        Ok(all)
    }

    async fn mark_broadcast(&self, ext: &ExtrinsicRow) -> StepResult<()> {
        let mut e = ext.clone();
        e.status = ExtrinsicStatus::Broadcast {
            attempts: match ext.status {
                ExtrinsicStatus::Broadcast { attempts } => attempts + 1,
                _ => 1,
            },
        };
        self.store.put_extrinsic(&e).await?;
        if let Some(mut row) = self
            .store
            .instructions()
            .await?
            .into_iter()
            .find(|r| r.operation == ext.operation)
        {
            if row.state == InstructionState::Signed {
                row.state = InstructionState::Broadcast;
                row.updated_at = self.now();
                self.store.put_instruction(&row).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::EpochAnswer;

    /// Review of #97, S3: only `ERR-MalformedCommand` (a core before the amendment's generation,
    /// or ingress's route closed) reads as "cannot take the input yet"; any other refusal code is
    /// a refusal, which pages and holds the new chain back.
    #[test]
    fn t043_15_only_a_malformed_command_is_unsupported() {
        assert_eq!(EpochAnswer::of("started"), EpochAnswer::Confirmed);
        assert_eq!(EpochAnswer::of("recorded"), EpochAnswer::Confirmed);
        assert_eq!(
            EpochAnswer::of("refused:ERR-MalformedCommand"),
            EpochAnswer::Unsupported("ERR-MalformedCommand".into())
        );
        for code in ["ERR-NotAuthorised", "ERR-RateLimited", "in_flight"] {
            assert_eq!(
                EpochAnswer::of(&format!("refused:{code}")),
                EpochAnswer::Refused(code.into())
            );
        }
    }
}
