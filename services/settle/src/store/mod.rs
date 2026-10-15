// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's durable state (`REQ-SA-5`, `REQ-SA-6`, `AC-K3.1`).
//!
//! From storage alone the adapter can state, at any later time, **what it was asked**
//! ([`InstructionRow`]), **what the network did** ([`ObservationRow`], [`ExtrinsicRow`] with its
//! inclusion) and **what it told the core** ([`ReportRow`], with the core's verdict). Besides
//! those: the signed bytes of every extrinsic, stored before they are broadcast (`REQ-SA-6`); the
//! nonce lanes; the inbox cursor; explicit gaps (`REQ-SA-7`); alerts; and the single-active
//! lease.
//!
//! Its rules hold whatever the adapter above does:
//!
//! - **One instruction per operation reference.** The same reference with another digest is
//!   refused ([`Inserted::Conflict`]) and changes nothing.
//! - **One extrinsic per operation reference, ever** (`REQ-SO-7`, the exit criterion). A second
//!   is refused, whoever writes it.
//! - **Fenced writes.** Storing an extrinsic and moving a lane need the current lease epoch: a
//!   second instance that lost the lease cannot sign into a lane (Kreivo's runtime).
//! - **The cursor never goes back**, and gaps and alerts are appended, never removed (an alert is
//!   cleared, and stays).
//!
//! Two implementations: [`mem::MemStore`] (tests; it outlives the adapters a test starts and
//! stops, as a database would) and `pg::PgStore` (feature `postgres`, the accepted proposal).

pub mod mem;
#[cfg(feature = "postgres")]
pub mod pg;

use std::future::Future;

use ledger_core::account::Time;
use serde::{Deserialize, Serialize};

use crate::chain::{ChainAsset, Included, Mortality, SignedExtrinsic, WithdrawCall};

/// Why the store refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The lease epoch is not the current one: another instance holds the lease.
    Fenced,
    /// A second extrinsic for one operation reference.
    Duplicate,
    /// No such row.
    Missing,
    /// The backend failed.
    Backend(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Fenced => f.write_str("the lease is held by another instance"),
            StoreError::Duplicate => f.write_str("a second extrinsic for one operation reference"),
            StoreError::Missing => f.write_str("no such row"),
            StoreError::Backend(e) => write!(f, "the store failed: {e}"),
        }
    }
}

/// A store result.
pub type StoreResult<T> = Result<T, StoreError>;

/// Where an instruction stands in the adapter (`F-016` plan: `Acknowledged → Parked | Signed →
/// Broadcast → Final | NotExecuted | Unknown`). The adapter's own record; never sent to the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstructionState {
    /// Stored; not yet acknowledged to the core.
    Received,
    /// Acknowledged to the core (`REQ-SO-6`); not yet signed.
    Acknowledged,
    /// Waiting for liquidity or fees (`REQ-SN-2`). Never signed while parked.
    Parked {
        /// Why.
        reason: ParkReason,
        /// Since when.
        since: Time,
    },
    /// Signed and stored; not yet broadcast.
    Signed,
    /// Broadcast at least once.
    Broadcast,
    /// The layer recorded its execution (`Withdrawn`, final).
    Executed,
    /// The layer recorded its refusal (`WithdrawRefused`, final).
    Refused {
        /// The chain's error.
        error: String,
    },
    /// Provably not executed (`REQ-SA-4`).
    NotExecuted {
        /// Why it provably did not execute.
        proof: NotExecuted,
    },
    /// Left unsubmitted for review (`F-027`): on its own, a batch carrying it fails its dry run,
    /// or needs more gas than any batch is given. Nothing moved and nothing was signed, and it is
    /// never signed after this. It is not a tombstone either: the core holds it acknowledged with
    /// no signing record, which proves nothing, so nothing is released and the operation reaches
    /// review at its deadline.
    Held {
        /// The dry run's error.
        error: String,
        /// Since when.
        since: Time,
    },
}

/// Why an instruction is parked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkReason {
    /// The instance's position, less pending withdrawals, does not cover it.
    Liquidity,
    /// The signer's native balance is below the fee floor.
    Fees,
}

/// A proof that an instruction did not execute (`REQ-SA-4`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proof", rename_all = "snake_case")]
pub enum NotExecuted {
    /// Its extrinsic failed in a finalized block: the nonce is spent and nothing moved.
    DispatchFailed {
        /// The block.
        block: u64,
        /// The chain's error.
        error: String,
    },
    /// Its era ended in a finalized block while the signer's finalized nonce was still at or
    /// below its own: it can never be included.
    EraEnded {
        /// The finalized height that proves it.
        finalized: u64,
    },
    /// Its nonce was spent by another extrinsic of the key: it can never be included (and the
    /// lane was used by someone else; an alert is raised).
    NonceSpent {
        /// The finalized height that proves it.
        finalized: u64,
    },
    /// It was never signed, and can no longer be signed within its expiry. A tombstone: it is
    /// never signed later.
    NeverSigned {
        /// The height by which no era could end before the expiry.
        at: u64,
    },
    /// Its transaction was included in a finalized block and the contract refused the call and
    /// stored nothing (`F-027`): an anchor that is not stored at the finalized head although the
    /// batch carrying it executed.
    CallRefused {
        /// The block that included it.
        block: u64,
    },
}

/// What the adapter was asked (`REQ-SA-5`): one instruction, as the core handed it, with the
/// adapter's disposition of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionRow {
    /// The operation reference, `did:`-shaped.
    pub operation: String,
    /// The core's digest of it: what the acknowledgement names.
    pub digest: String,
    /// The book's asset.
    pub asset: String,
    /// The minor-unit amount.
    pub amount: u64,
    /// The call it becomes on the chain.
    pub call: WithdrawCall,
    /// The settlement-layer height after which it can no longer take effect.
    pub expiry: u64,
    /// Whether the core has the acknowledgement.
    pub acknowledged: bool,
    /// Where it stands.
    pub state: InstructionState,
    /// When it was stored.
    pub received_at: Time,
    /// When it last changed.
    pub updated_at: Time,
}

/// What became of a signed extrinsic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExtrinsicStatus {
    /// Stored, not yet broadcast.
    Signed,
    /// Broadcast; uncertain. Its nonce stays reserved.
    Broadcast {
        /// How many times.
        attempts: u64,
    },
    /// In a finalized block.
    Included(Included),
    /// Can never be included (see [`NotExecuted`]).
    Dead(NotExecuted),
}

/// A signed extrinsic, stored before it is broadcast (`REQ-SA-6`): one row per operation it
/// executes. A batch (`F-027`) is one transaction carrying several calls, so its rows share the
/// lane, the nonce, the era and the bytes; each operation still has one transaction, ever.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtrinsicRow {
    /// The operation reference it executes.
    pub operation: String,
    /// The lane: the signer's account, hex.
    pub lane: String,
    /// Its nonce.
    pub nonce: u64,
    /// Its era.
    pub era: Mortality,
    /// The exact bytes; after a restart the same bytes are broadcast again (`REQ-SO-7`).
    pub extrinsic: SignedExtrinsic,
    /// What became of it.
    pub status: ExtrinsicStatus,
    /// When it was signed.
    pub signed_at: Time,
    /// The operations of every call the transaction carries, in order, this one included
    /// (`F-027`).
    #[serde(default)]
    pub batch: Vec<String>,
}

/// A signed anchor extrinsic (`F-017`), stored before it is broadcast (`REQ-SA-6`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorExtrinsic {
    /// The anchor key's account, hex.
    pub lane: String,
    /// Its nonce.
    pub nonce: u64,
    /// Its era.
    pub era: Mortality,
    /// The exact bytes.
    pub extrinsic: SignedExtrinsic,
    /// What became of it.
    pub status: ExtrinsicStatus,
    /// When it was signed.
    pub signed_at: Time,
}

/// A binding of a book-only asset the adapter was asked to make (amendment 0011, `F-029`): what
/// it was asked, and every extrinsic that carried it. A bind moves no value and the contract
/// refuses a second one of the same asset (`AlreadyBound`), so a bind whose extrinsic provably did
/// not execute is signed again; the earlier extrinsics stay in `previous`. Once an extrinsic
/// carrying it executed, the contract's inbox holds its outcome and it is never signed again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindRow {
    /// The call's reference, 32 bytes of hex: the `bind_asset` operation's.
    pub reference: String,
    /// The book's asset.
    pub asset: String,
    /// Its location on the chain.
    pub location: String,
    /// The amount, in the asset's smallest unit. Never changes.
    pub amount: u128,
    /// The live extrinsic, if one is signed.
    pub extrinsic: Option<AnchorExtrinsic>,
    /// Extrinsics that provably did not execute.
    pub previous: Vec<AnchorExtrinsic>,
    /// Whether an extrinsic carrying it executed: its outcome is in the inbox.
    pub executed: bool,
    /// When it was stored.
    pub received_at: Time,
    /// When it last changed.
    pub updated_at: Time,
}

/// An anchor the adapter was asked to publish (`F-017`): what it was asked, whether the core has
/// its acknowledgement, and every extrinsic that carried it. An anchor moves no value and the
/// pallet takes a replay of one as a no-op, so an anchor whose extrinsic provably did not execute
/// is signed again; the earlier extrinsics stay in `previous`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorRow {
    /// The anchor number.
    pub number: u64,
    /// The commitment, hex. Never changes.
    pub commitment: String,
    /// Whether the core has the acknowledgement of the current emission.
    pub acknowledged: bool,
    /// The live extrinsic, if one is signed.
    pub extrinsic: Option<AnchorExtrinsic>,
    /// Extrinsics that provably did not execute.
    pub previous: Vec<AnchorExtrinsic>,
    /// When it was stored.
    pub received_at: Time,
    /// When it last changed.
    pub updated_at: Time,
}

/// A nonce lane: one key (`F-016` plan: one extrinsic in flight per lane; `F-027`: that extrinsic
/// is a batch).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneRow {
    /// The signer's account, hex.
    pub lane: String,
    /// The lowest nonce not reserved.
    pub next_nonce: u64,
    /// The first operation of the batch whose extrinsic holds the lane's reserved nonce, if any.
    pub in_flight: Option<String>,
}

/// What the network did: one inbox entry as read (`REQ-SA-5`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRow {
    /// The instance.
    pub instance: u32,
    /// Its position.
    pub seq: u64,
    /// The entry's digest, hex.
    pub digest: String,
    /// The block that wrote it.
    pub at: u64,
    /// The stored bytes, hex.
    pub raw: String,
    /// Whether it was read at or below the finalized head.
    pub finalized: bool,
    /// The operation reference it carries, if one of the deployment's.
    pub operation: Option<String>,
    /// A summary of the event, for operators.
    pub summary: String,
    /// When it was read.
    pub read_at: Time,
}

/// What the adapter told the core, and the core's verdict (`REQ-SA-5`, `REQ-SI-5`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRow {
    /// The client key it was sent under: the same on every retry.
    pub key: String,
    /// What kind of report: `acknowledge`, `sign`, `observe`, `check`, `head`, `gap`,
    /// `rescan`, `declare`.
    pub kind: String,
    /// The body sent.
    pub body: serde_json::Value,
    /// The core's answer, once there is one: a verdict or a refusal code.
    pub verdict: Option<String>,
    /// When it was first sent.
    pub sent_at: Time,
    /// When the answer was stored.
    pub answered_at: Option<Time>,
}

/// A range the adapter was forced to skip (`REQ-SA-7`, `AC-D2.2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapRow {
    /// The instance.
    pub instance: u32,
    /// The first position skipped.
    pub from: u64,
    /// The first position after it.
    pub to: u64,
    /// Why: `pruned` or `unreadable`.
    pub reason: String,
    /// When it was recorded.
    pub at: Time,
}

/// An alert: one per position, never one per operation (`AC-E3.1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertRow {
    /// Its key: `position:<instance>:<asset>`, `fees:<lane>`, `digest:<operation>`, ...
    pub key: String,
    /// What it is about.
    pub detail: String,
    /// When it was raised.
    pub opened_at: Time,
    /// When it cleared, if it did.
    pub cleared_at: Option<Time>,
    /// How many times it was raised.
    pub raised: u64,
}

/// A runtime of the settlement layer the adapter has served (`T-027-12`, `REQ-SA-9`): the layer
/// confirmed a transaction signed in the format chosen for it. Kept, so a restart does not ask
/// again; never removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRow {
    /// The runtime's identity: `<spec_version>:<surface fingerprint, hex>:<format name>`.
    pub key: String,
    /// Its `spec_version`.
    pub spec_version: u32,
    /// Its `transaction_version`.
    pub transaction_version: u32,
    /// Its transaction extensions, in order.
    pub extensions: Vec<String>,
    /// The digest of the types of what a transaction carries, hex.
    pub types: String,
    /// The format confirmed.
    pub format: String,
    /// The hash of the transaction the layer found valid, hex.
    pub extrinsic: String,
    /// When.
    pub confirmed_at: Time,
}

/// What inserting an instruction found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inserted {
    /// A new row.
    New,
    /// The same instruction was stored before.
    Same(InstructionRow),
    /// The same operation reference with another digest: nothing was written.
    Conflict(InstructionRow),
}

/// The lease a running instance holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// Who holds it.
    pub holder: String,
    /// Its epoch: every take-over increments it.
    pub epoch: u64,
    /// When it runs out unless renewed.
    pub expires_at: Time,
}

/// The adapter's store. See the module documentation.
pub trait Store: Clone + Send + Sync + 'static {
    /// Take the lease for `holder` if it is free, expired, or already `holder`'s. A take-over
    /// increments the epoch. `None` if another holder has it.
    fn acquire(
        &self,
        holder: &str,
        now: Time,
        ttl: Time,
    ) -> impl Future<Output = StoreResult<Option<Lease>>> + Send;

    /// Renew the lease at `epoch`. `false` if it was lost.
    fn renew(
        &self,
        holder: &str,
        epoch: u64,
        now: Time,
        ttl: Time,
    ) -> impl Future<Output = StoreResult<bool>> + Send;

    /// Insert an instruction.
    fn insert_instruction(
        &self,
        row: &InstructionRow,
    ) -> impl Future<Output = StoreResult<Inserted>> + Send;

    /// Replace an instruction's row.
    fn put_instruction(&self, row: &InstructionRow)
    -> impl Future<Output = StoreResult<()>> + Send;

    /// Every instruction, by operation reference.
    fn instructions(&self) -> impl Future<Output = StoreResult<Vec<InstructionRow>>> + Send;

    /// In one transaction, fenced by `epoch`: store the signed extrinsic, reserve its nonce in
    /// its lane, and move its instruction to `Signed`. [`StoreError::Duplicate`] if the operation
    /// already has an extrinsic.
    fn store_signed(
        &self,
        epoch: u64,
        extrinsic: &ExtrinsicRow,
        lane: &LaneRow,
        instruction: &InstructionRow,
    ) -> impl Future<Output = StoreResult<()>> + Send;

    /// [`Store::store_signed`] of a batch (`F-027`), all or nothing: one row per operation the
    /// transaction carries, `extrinsics[i]` for `instructions[i]`, and the lane.
    /// [`StoreError::Duplicate`] if any of the operations already has an extrinsic: then nothing
    /// is written.
    fn store_batch(
        &self,
        epoch: u64,
        extrinsics: &[ExtrinsicRow],
        lane: &LaneRow,
        instructions: &[InstructionRow],
    ) -> impl Future<Output = StoreResult<()>> + Send;

    /// Replace an extrinsic's row (its status).
    fn put_extrinsic(&self, row: &ExtrinsicRow) -> impl Future<Output = StoreResult<()>> + Send;

    /// Every extrinsic.
    fn extrinsics(&self) -> impl Future<Output = StoreResult<Vec<ExtrinsicRow>>> + Send;

    /// A lane, if it was ever used.
    fn lane(&self, lane: &str) -> impl Future<Output = StoreResult<Option<LaneRow>>> + Send;

    /// Replace a lane, fenced by `epoch`.
    fn put_lane(&self, epoch: u64, row: &LaneRow) -> impl Future<Output = StoreResult<()>> + Send;

    /// Insert an observation; `false` if it was stored before.
    fn insert_observation(
        &self,
        row: &ObservationRow,
    ) -> impl Future<Output = StoreResult<bool>> + Send;

    /// Every observation.
    fn observations(&self) -> impl Future<Output = StoreResult<Vec<ObservationRow>>> + Send;

    /// Insert or replace a report.
    fn put_report(&self, row: &ReportRow) -> impl Future<Output = StoreResult<()>> + Send;

    /// A report by key.
    fn report(&self, key: &str) -> impl Future<Output = StoreResult<Option<ReportRow>>> + Send;

    /// Every report.
    fn reports(&self) -> impl Future<Output = StoreResult<Vec<ReportRow>>> + Send;

    /// The next inbox position to read for `instance`.
    fn cursor(&self, instance: u32) -> impl Future<Output = StoreResult<u64>> + Send;

    /// Move the cursor forward; never back.
    fn advance_cursor(
        &self,
        instance: u32,
        to: u64,
    ) -> impl Future<Output = StoreResult<()>> + Send;

    /// Record a gap (idempotent on `(instance, from)`).
    fn insert_gap(&self, row: &GapRow) -> impl Future<Output = StoreResult<()>> + Send;

    /// Every gap.
    fn gaps(&self) -> impl Future<Output = StoreResult<Vec<GapRow>>> + Send;

    /// Raise the alert `key`. `true` if it was not already open (a new alert).
    fn raise(
        &self,
        key: &str,
        detail: &str,
        now: Time,
    ) -> impl Future<Output = StoreResult<bool>> + Send;

    /// Clear the alert `key` if open. `true` if it was open.
    fn clear(&self, key: &str, now: Time) -> impl Future<Output = StoreResult<bool>> + Send;

    /// Every alert.
    fn alerts(&self) -> impl Future<Output = StoreResult<Vec<AlertRow>>> + Send;

    /// Every anchor row, by number (`F-017`).
    fn anchors(&self) -> impl Future<Output = StoreResult<Vec<AnchorRow>>> + Send;

    /// Every bind row, by reference (amendment 0011, `F-029`).
    fn binds(&self) -> impl Future<Output = StoreResult<Vec<BindRow>>> + Send;

    /// Insert or replace a bind row, fenced by `epoch`. [`StoreError::Duplicate`] if the reference
    /// is stored with another asset, location or amount.
    fn put_bind(&self, epoch: u64, row: &BindRow) -> impl Future<Output = StoreResult<()>> + Send;

    /// Insert or replace an anchor row, fenced by `epoch`. [`StoreError::Duplicate`] if the
    /// number is stored with another commitment.
    fn put_anchor(
        &self,
        epoch: u64,
        row: &AnchorRow,
    ) -> impl Future<Output = StoreResult<()>> + Send;

    /// Every runtime the layer confirmed a format on (`T-027-12`).
    fn runtimes(&self) -> impl Future<Output = StoreResult<Vec<RuntimeRow>>> + Send;

    /// Record a runtime the layer confirmed a format on; a second record of one key keeps the
    /// first.
    fn confirm_runtime(&self, row: &RuntimeRow) -> impl Future<Output = StoreResult<()>> + Send;
}

/// The asset of a call, for the liquidity check.
pub fn call_asset(row: &InstructionRow) -> &ChainAsset {
    &row.call.asset
}
