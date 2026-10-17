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
//! 5. **Observe** (`REQ-SA-7`, `REQ-SI-1`–`SI-5`). Check what was reported provisionally; read
//!    the inbox from the cursor at the finalized head, 32 positions a page, reporting every entry
//!    with its canonical check; record a pruned range as an explicit gap; report the adapter's own
//!    extrinsics seen above finality as provisional; report the finalised head **with the
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
//! 7. **Re-scan** (`REQ-IN-5`), every so many steps.
//! 8. **Anchor** (`F-017`, with an anchor key only). Decide the anchor key's transaction at the
//!    finalized head; take the checkpoint in flight from the call outbox; store it, then
//!    acknowledge it; unless the finalized deployment already holds it, sign the anchor call in a
//!    batch of its own on the anchor key, **store the bytes, then broadcast them**. An anchor moves
//!    no value and the contract takes a replay of its latest anchor as a no-op, so a transaction
//!    that provably did not store it (not included, failed, or the contract refused the call) is
//!    reported (`AnchorNotExecuted`, the core emits the anchor again) and a new one is signed.
//! 9. **Positions** (`F-017`, `REQ-PR-5`, with an anchor key only). Once the cursor has read
//!    the whole finalized inbox, report the instance's positions at the finalized head, when the
//!    core says a comparison is due or they changed.
//!
//! The adapter reports observations only. It never tells the core an outcome (`INV-11`), and
//! "unknown" produces no report at all (`REQ-SO-4`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use ingress::clock::SharedClock;
use ingress::settlement::{
    AcknowledgementBody, AnchorAcknowledgementBody, AnchorFactBody, AnchorFailureBody,
    AttestationBody, CallBody, CheckBody, EventKindBody, GapBody, GapReasonBody, HeadBody,
    KeyNonceBody, LayerAssetFactsBody, LayerAssetStatusBody, LayerEntryBody, MovementBody,
    ObservationBody, ObservedFinalityBody, PositionBody, PositionsBody, RescanBody,
    RescanResultBody, SettlementCallBody, SigningBody, SigningRecordBody,
    destination as parse_destination, venue_body,
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
    AccountId32, AnchorCall, BatchCall, BatchFailure, BindCall, BlockRef, Chain, ChainAsset,
    ChainError, InboxEntry, InboxEvent, InboxRead, Mortality, RuntimeInfo, SignedExtrinsic,
    Validity, WithdrawCall,
};
use crate::config::AdapterConfig;
use crate::link::{Answer, CoreLink, LinkError, Transport};
use crate::signer::Signer;
use crate::store::{
    AnchorExtrinsic, AnchorRow, BindRow, ExtrinsicRow, ExtrinsicStatus, GapRow, Inserted,
    InstructionRow, InstructionState, LaneRow, NotExecuted, ObservationRow, ParkReason, ReportRow,
    RuntimeRow, Store, StoreError,
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
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::LeaseLost => f.write_str("another instance holds the lease"),
            AdapterError::Chain(e) => e.fmt(f),
            AdapterError::Link(e) => e.fmt(f),
            AdapterError::Store(e) => e.fmt(f),
            AdapterError::Crashed(p) => write!(f, "crashed at {p:?}"),
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

/// What a re-scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rescanned {
    /// The core's report.
    pub core: RescanResultBody,
    /// Positions the layer holds that the adapter's own store never recorded.
    pub adapter_missing: Vec<u64>,
}

/// Test hooks. None of them is reachable from configuration.
#[derive(Clone, Debug, Default)]
pub struct Faults {
    crash: BTreeSet<CrashPoint>,
    drop_reports: BTreeSet<u64>,
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
}

/// The format the adapter signs in on a runtime, and that runtime's identity (`T-027-12`).
#[derive(Clone, Debug)]
struct Chosen {
    /// What a transaction is signed for.
    signing: kreivo::Signing,
    /// The runtime's surface.
    surface: kreivo::Surface,
    /// The runtime, as its confirmation is recorded: `spec_version`, the surface's fingerprint
    /// and the format's name.
    key: String,
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

    /// Read inbox position `seq` but never report it: the core's copy then lacks it (tests,
    /// `AC-D2.1`).
    pub fn drop_report_of(&mut self, seq: u64) {
        self.faults.drop_reports.insert(seq);
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
        let key = format!(
            "{}:{}:{}",
            runtime.spec_version,
            hex::encode(runtime.surface.fingerprint()),
            format.name
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
            at: runtime.at,
        })
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
        let format = chosen.signing.format.name;
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
        let due = match self.last_declared {
            None => true,
            Some((at, was)) => {
                was != serving || now.saturating_sub(at) >= self.config.status_lease / 2
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
        if serving {
            self.attest_assets().await?;
        }
        let support_changed = serving && self.support() != self.declared_support;
        if !due && !support_changed {
            return Ok(());
        }
        // Every declaration is a new request: a repeat under an old key would be answered with
        // the old declaration and never reach the core.
        self.declarations += 1;
        let key = format!(
            "settle/declare/{}/{now}/{}/{serving}",
            self.session, self.declarations
        );
        let support = serving.then(|| self.support());
        let body = serde_json::json!({
            "serving": serving, "lease": self.config.status_lease, "support": support
        });
        self.report_start(&key, "declare", body).await?;
        let a = self
            .link
            .declare(&key, serving, self.config.status_lease, support.clone())
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
            self.last_declared = Some((now, serving));
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
        let epoch = self.hold_lease().await?;
        self.heartbeat().await?;
        let mut report = StepReport {
            serving: self.serving,
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
        report.observed = observed;
        report.gaps = gaps;
        let (signed, parked, broadcast) = self.submit(epoch, &fin, &best, head_taken).await?;
        report.signed = signed;
        report.parked = parked;
        report.broadcast = broadcast;
        if self.config.rescan_every > 0 && self.steps % self.config.rescan_every == 0 {
            let to = self.store.cursor(self.config.instance).await?;
            let from = to.saturating_sub(self.config.rescan_window);
            if to > from {
                self.rescan(from, to).await?;
            }
        }
        if self.anchor_signer.is_some() {
            let anchor = calls.iter().find_map(|c| match &c.call {
                SettlementCallBody::Anchor { number, commitment } => {
                    Some((*number, commitment.clone(), c.acknowledged))
                }
                SettlementCallBody::Withdraw { .. } | SettlementCallBody::Bind { .. } => None,
            });
            self.anchor(epoch, &fin, &best, anchor).await?;
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
            self.binds(epoch, &fin, &best, &binds).await?;
            self.positions(&fin).await?;
        }
        Ok(report)
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

    async fn record_gap(&mut self, from: u64, to: u64, reason: GapReasonBody) -> StepResult<()> {
        tracing::warn!(
            from,
            to,
            "an inbox range could not be read: recorded as a gap"
        );
        let now = self.now();
        let reason_text = match reason {
            GapReasonBody::Pruned => "pruned",
            GapReasonBody::Unreadable => "unreadable",
        };
        self.store
            .insert_gap(&GapRow {
                instance: self.config.instance,
                from,
                to,
                reason: reason_text.to_owned(),
                at: now,
            })
            .await?;
        self.store
            .raise(
                &format!("gap:{}:{from}", self.config.instance),
                &format!("inbox positions [{from}, {to}) were {reason_text} before they were read"),
                now,
            )
            .await?;
        let body = GapBody { from, to, reason };
        let key = format!("settle/gap/{}/{from}/{to}", self.config.instance);
        self.report_start(&key, "gap", serde_json::to_value(&body).unwrap_or_default())
            .await?;
        let a = self.link.gap(&key, &body).await?;
        self.report_end(&key, &a).await?;
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

    /// Checks, the cursor read, provisional reads, and the head. Returns (reported, gaps, whether
    /// the core took the head in).
    async fn observe(
        &mut self,
        fin: &BlockRef,
        best: &BlockRef,
    ) -> StepResult<(usize, usize, bool)> {
        let instance = self.config.instance;
        let now = self.now();
        let Some(view) = self.chain.instance(fin, instance).await? else {
            self.store
                .raise("instance", "the instance is not registered", now)
                .await?;
            return Ok((0, 0, false));
        };
        // 1. What was reported provisionally and is now below the finality point: its position is
        // in the finalized inbox, or the finalized head has passed the block that wrote it. In the
        // second case a position the finalized inbox has not reached was never written by that
        // block on the finalized chain, and never will be (the entry's digest binds its block):
        // the record is orphaned (`F-027`: a batch whose era's birth block was reorged away
        // otherwise leaves its provisional records unchecked for as long as the inbox is idle).
        //
        // Every record that no longer matches is reported before any that does (`F-027`): a
        // transaction whose era is born at the finalized head survives a reorganisation and can
        // be included again at the same position in another block, leaving two provisional
        // records of one operation at one position. The core rebinds the operation to the
        // surviving record when the first is orphaned; were the survivor already made final by an
        // earlier check, nothing would promote the operation afterwards.
        let mut checks = Vec::new();
        for obs in self.store.observations().await? {
            if obs.finalized || (obs.seq >= view.next_seq && fin.number < obs.at) {
                continue;
            }
            let read = self.chain.inbox(fin, instance, obs.seq, 1).await?;
            let still_matches = matches!(
                read.first(),
                Some((_, InboxRead::Entry(e))) if hex::encode(e.digest()) == obs.digest
            );
            checks.push((still_matches, obs));
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
        // 2. The cursor read at the finalized head.
        let mut reported = 0;
        let mut gaps = 0;
        let mut cursor = self.store.cursor(instance).await?;
        let mut gap_from: Option<u64> = None;
        'read: while cursor < view.next_seq {
            let page = self
                .chain
                .inbox(fin, instance, cursor, self.config.page)
                .await?;
            if page.is_empty() {
                break;
            }
            for (seq, read) in page {
                match read {
                    InboxRead::Pruned => {
                        gap_from.get_or_insert(seq);
                    }
                    InboxRead::Absent => {
                        // Not pruned and not there: never skipped silently, never passed.
                        self.store
                            .raise(
                                &format!("unreadable:{instance}:{seq}"),
                                "an inbox position below the head could not be read",
                                now,
                            )
                            .await?;
                        break 'read;
                    }
                    InboxRead::Entry(e) => {
                        if let Some(from) = gap_from.take() {
                            self.record_gap(from, seq, GapReasonBody::Pruned).await?;
                            gaps += 1;
                        }
                        self.store
                            .insert_observation(&self.observation_row(seq, &e, true))
                            .await?;
                        self.fault(CrashPoint::AfterObserve)?;
                        if !self.faults.drop_reports.contains(&seq) {
                            let body = self.observation_body(seq, &e, true);
                            let a = self.report_observation(&body).await?;
                            if !a.ingested() {
                                // The core did not take it in: do not pass it.
                                self.store
                                    .raise(
                                        &format!("report:{instance}:{seq}"),
                                        &format!(
                                            "the core refused an observation: {}",
                                            a.verdict()
                                        ),
                                        now,
                                    )
                                    .await?;
                                break 'read;
                            }
                            self.report_check(&body.coordinate, fin.number, true)
                                .await?;
                            reported += 1;
                        }
                        self.note_record(&e.event).await?;
                        self.fault(CrashPoint::AfterVerdict)?;
                        self.store.advance_cursor(instance, seq + 1).await?;
                    }
                }
                cursor = seq + 1;
            }
        }
        if let Some(from) = gap_from.take() {
            self.record_gap(from, cursor, GapReasonBody::Pruned).await?;
            gaps += 1;
        }
        // 3. The adapter's own extrinsics seen above the finality point: provisional.
        if best.number > fin.number {
            if let Some(best_view) = self.chain.instance(best, instance).await? {
                let ours: BTreeSet<String> = self
                    .store
                    .instructions()
                    .await?
                    .into_iter()
                    .map(|r| r.operation)
                    .collect();
                let mut seq = view.next_seq;
                while seq < best_view.next_seq {
                    let page = self
                        .chain
                        .inbox(best, instance, seq, self.config.page)
                        .await?;
                    if page.is_empty() {
                        break;
                    }
                    for (s, read) in page {
                        seq = s + 1;
                        let InboxRead::Entry(e) = read else { continue };
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
        // 4. The finalised head: expiry and non-inclusion are the core's to decide from it, with
        // the nonce of every signing key read at the same finalized block (the owner's ruling of
        // 2026-09-24).
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
            next_seq: view.next_seq,
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
        Ok((reported, gaps, taken))
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

    // ------------------------------------------------------------------------------ re-scan

    /// Read `[from, to)` again at the finalized head and have the core compare it with what it
    /// recorded (`REQ-IN-5`, `AC-D2.1`); compare it with the adapter's own store too. Every
    /// difference is an alert naming the position: never a repair.
    pub async fn rescan(&mut self, from: u64, to: u64) -> StepResult<Rescanned> {
        let instance = self.config.instance;
        let now = self.now();
        let fin = self.chain.finalized().await?;
        let mut layer = Vec::new();
        let mut seq = from;
        while seq < to {
            let count = u32::try_from((to - seq).min(u64::from(self.config.page))).unwrap_or(32);
            let page = self.chain.inbox(&fin, instance, seq, count).await?;
            if page.is_empty() {
                break;
            }
            for (s, read) in page {
                seq = s + 1;
                if let InboxRead::Entry(e) = read {
                    layer.push((s, hex::encode(e.digest())));
                }
            }
        }
        let held: BTreeSet<(u64, String)> = self
            .store
            .observations()
            .await?
            .into_iter()
            .filter(|o| o.finalized)
            .map(|o| (o.seq, o.digest))
            .collect();
        let mut out = Rescanned::default();
        for (s, d) in &layer {
            if !held.contains(&(*s, d.clone())) {
                out.adapter_missing.push(*s);
                self.store
                    .raise(
                        &format!("rescan:adapter:{instance}:{s}"),
                        "the layer holds a record the adapter never stored",
                        now,
                    )
                    .await?;
            }
        }
        let body = RescanBody {
            from,
            to,
            layer: layer
                .iter()
                .map(|(seq, digest)| LayerEntryBody {
                    seq: *seq,
                    digest: digest.clone(),
                })
                .collect(),
        };
        let key = format!("settle/rescan/{instance}/{from}/{to}/{}", fin.number);
        self.report_start(
            &key,
            "rescan",
            serde_json::to_value(&body).unwrap_or_default(),
        )
        .await?;
        let a = self.link.rescan(&key, &body).await?;
        self.report_end(&key, &a).await?;
        if let Answer::Decided { result, .. } = a {
            let r = result.rescan.unwrap_or_default();
            for m in &r.missing {
                self.store
                    .raise(
                        &format!("incident:missing:{instance}:{}", m.seq),
                        &format!(
                            "the core never ingested the record at {} ({})",
                            m.seq, m.digest
                        ),
                        now,
                    )
                    .await?;
            }
            for u in &r.unknown {
                self.store
                    .raise(
                        &format!("incident:unknown:{instance}:{}", u.seq),
                        &format!("the layer no longer shows the final record at {}", u.seq),
                        now,
                    )
                    .await?;
            }
            out.core = r;
        }
        Ok(out)
    }
}
