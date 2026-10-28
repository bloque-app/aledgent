// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement boundary through ingress (`F-016`; `CTR-SET`, SPEC §7.F; `DEC-8`).
//!
//! Three surfaces meet here, each a command logged in the received-log like every other, so every
//! input the core receives is in the log (`F-016` plan: the adapter writes to the core only
//! through ingress):
//!
//! - **`submit_outbound`** on `CTR-CMD` (`POST /v1/withdrawals/{reference}/submit`): the caller
//!   names the destination of a reserved outbound leg; the core assembles the instruction
//!   (`REQ-SO-2`, `REQ-SO-5`) and puts it in the outbox.
//! - **`configure_settlement`** on `CTR-OPS` (`POST /v1/ops/settlement`): the operator names the
//!   adapter's identity as the authoritative source, the instance, and the timing estimates
//!   (`F-010` decision 25).
//! - **The adapter's surface** (`POST /v1/settlement/*`, `GET /v1/settlement/instructions`): its
//!   three capabilities — `declare_adapter`, `ack_instruction`, `record_observation` — and its one
//!   read, the outbox of instructions. The core resolves the caller as the adapter's principal and
//!   refuses anyone else; it refuses the adapter's principal on every other route
//!   ([`ledger_core::ledger::settle_adapter`], `REQ-SA-8`).
//!
//! The adapter also attests what the layer's metadata states about each asset location it binds
//! (`POST /v1/settlement/attestations`, `DEC-20`, `F-029`): registration reads it, and drift from
//! the registry holds the asset.
//!
//! Nothing here carries an outcome inwards (`INV-11`): an observation is a named event at a
//! coordinate with the layer's own payload and the finality as observed.

use ledger_core::Refusal;
use ledger_core::anchor::{AnchorOutbox, Comparison, PositionCut};
use ledger_core::asset::{AssetId, AssetLocation, Minor};
use ledger_core::id::{AccountId, IdentityId, OperationId};
use ledger_core::ledger::settle_adapter::{AdapterInput, AdapterReply};
use ledger_core::ledger::{Ctx, Ledger};
use ledger_core::principal::Caller;
use ledger_core::settlement::assets::{
    AssetAttestation, AttestationVerdict, LAYER_TEXT_MAX, LayerAssetFacts, LayerAssetStatus,
};
use ledger_core::settlement::{
    AdapterStatus, AnchorFact, Coordinate, Destination, EpochOutcome, EpochRefusal, EventKind,
    FinalisedHead, Gap, GapReason, HeadReport, KeyNonce, Movement, Observation, ObservedFinality,
    OutboundInstruction, OutboxCall, Payload, PreviousAnchor, RescanReport, SettlementCall,
    SettlementConfig, SettlementEpoch, Signing, SigningRecord, SourceId, SupportDeclaration, Venue,
    Verdict,
};
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::boundary::{Outcome, QueryReply};
use crate::command::{op_kind, op_kind_body};
use crate::wire::{AdapterLegBody, CommandBody, CommandResult, CoordinateBody, ShownBody};

// ------------------------------------------------------------------------------------ wire

/// Where an outbound instruction sends value, in the settlement layer's terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DestinationBody {
    /// A plain account on the settlement layer: 32 bytes of hex.
    Account(String),
    /// Another deployment on the same settlement layer (`REQ-SL-6`).
    Instance {
        /// Its instance.
        instance: u32,
        /// The opaque account reference it resolves: 32 bytes of hex.
        account_ref: String,
    },
}

/// `submit_outbound` of the reserved outbound leg in the path (SPEC §8.1, `REQ-SO-2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitOutboundBody {
    /// Where the value goes.
    pub destination: DestinationBody,
}

/// `submit_outbound` as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitOutboundCommand {
    /// The reserved outbound leg.
    pub operation: String,
    /// Where the value goes.
    pub destination: DestinationBody,
}

/// `configure_settlement` (`F-010` decision 25): the operator's, set once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureSettlementBody {
    /// The settlement adapter's identity: the one authoritative source (`REQ-IN-6`), and the only
    /// principal the adapter's surface accepts.
    pub adapter: String,
    /// The deployment's instance on the settlement layer.
    pub instance: u32,
    /// The Suspense account an excess execution is taken from (`F-010` §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspense: Option<String>,
    /// The deadline of an unoriginated arrival, from its observation, in milliseconds.
    pub inbound_deadline: u64,
    /// How many settlement-layer heights an instruction stays valid for.
    pub expiry_window: u64,
    /// The estimated time of one height, in milliseconds.
    pub block_time: u64,
    /// The time finality may take past an expiry, in milliseconds.
    pub finality_margin: u64,
}

/// `declare_adapter`: the adapter's status and, if given, what it supports (`REQ-SA-1`,
/// `REQ-SA-2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterDeclarationBody {
    /// Whether every start-up assertion held.
    pub serving: bool,
    /// How long the declaration holds, in milliseconds (at most five minutes).
    pub lease: u64,
    /// Every supported pair; absent leaves the last declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support: Option<Vec<AdapterLegBody>>,
}

/// `ack_instruction`: the instruction is durable in the adapter's store (`REQ-SO-6`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgementBody {
    /// The operation.
    pub operation_reference: String,
    /// The instruction's digest, 32 bytes of hex.
    pub instruction: String,
}

/// What the adapter did about signing an instruction (the owner's ruling of 2026-09-24).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SigningRecordBody {
    /// The one transaction it signed for the instruction, recorded before it submits it.
    Signed {
        /// The signing key's account on the settlement layer, 32 bytes of hex.
        key: String,
        /// The account nonce the transaction carries.
        nonce: u64,
        /// The last settlement-layer height the transaction can be included at.
        last_valid: u64,
    },
    /// It will never sign one: the instruction can no longer be signed within its expiry.
    NeverSigned,
}

/// `ack_instruction`, after the acknowledgement: what the adapter did about signing the
/// instruction (the owner's ruling of 2026-09-24).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SigningBody {
    /// The operation.
    pub operation_reference: String,
    /// The instruction's digest, 32 bytes of hex.
    pub instruction: String,
    /// What it did.
    pub record: SigningRecordBody,
}

/// A signing key's account nonce at a finalised head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyNonceBody {
    /// The signing key's account on the settlement layer, 32 bytes of hex.
    pub key: String,
    /// Its account nonce at the head.
    pub nonce: u64,
}

/// The event a record names, as the settlement layer names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventKindBody {
    /// Value arrived.
    Deposited,
    /// Value left by an instruction of the deployment.
    Withdrawn,
    /// The layer refused an instruction and moved nothing.
    WithdrawRefused,
    /// An anchor was recorded.
    Anchored,
    /// The layer refused an anchor call and stored nothing (`F-027`).
    AnchorRefused,
    /// The layer recorded a binding of a book-only asset at zero divergence (amendment 0011).
    Bound,
    /// The layer refused a binding and recorded nothing but the refusal.
    BindRefused,
}

/// Finality as the adapter observed it. Stored, never trusted (`REQ-IN-3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObservedFinalityBody {
    /// Above the finality point.
    Provisional,
    /// At or below it.
    Final,
}

/// The value a record moved, in the book's terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MovementBody {
    /// The asset.
    pub asset: String,
    /// Minor units, as a decimal-integer string.
    pub amount: String,
}

/// `record_observation` (`REQ-SI-1`): a named event at a coordinate. No field for an outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationBody {
    /// Where.
    pub coordinate: CoordinateBody,
    /// The height of the block that wrote the record.
    pub at: u64,
    /// What the layer says happened.
    pub kind: EventKindBody,
    /// The layer's own bytes, its error text included, hex.
    pub payload: String,
    /// Finality as observed.
    pub observed_finality: ObservedFinalityBody,
    /// aledgent's operation reference, for a movement it originated (`REQ-SI-3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_reference: Option<String>,
    /// The opaque account reference value was addressed to, 32 bytes of hex (`REQ-SL-7`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_ref: Option<String>,
    /// The value moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub movement: Option<MovementBody>,
    /// The anchor recorded, for an `Anchored` record (`F-017`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<AnchorFactBody>,
}

/// An anchor as the settlement layer recorded it (`F-017`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchorFactBody {
    /// The anchor number.
    pub number: u64,
    /// The commitment, 32 bytes of hex.
    pub commitment: String,
    /// The venue the record was read in (`F-027`): absent for the settlement pallet under the
    /// deployment's instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue: Option<VenueBody>,
}

/// Where an anchor lives on the settlement layer (`F-027` T-027-04).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum VenueBody {
    /// The settlement pallet, under an instance.
    Pallet {
        /// The instance.
        instance: u32,
    },
    /// The settlement contract.
    Contract {
        /// The contract's account, 32 bytes of hex.
        account: String,
    },
}

/// A venue on the wire.
pub fn venue_body(v: &Venue) -> VenueBody {
    match v {
        Venue::Pallet { instance } => VenueBody::Pallet {
            instance: *instance,
        },
        Venue::Contract { account } => VenueBody::Contract {
            account: hex::encode(account),
        },
    }
}

/// A venue from the wire.
pub fn venue(v: &VenueBody) -> Result<Venue, Refusal> {
    Ok(match v {
        VenueBody::Pallet { instance } => Venue::Pallet {
            instance: *instance,
        },
        VenueBody::Contract { account } => Venue::Contract {
            account: hex32(account)?,
        },
    })
}

/// `ack_instruction` of an anchor (`F-017`): the checkpoint's instruction is durable in the
/// adapter's store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchorAcknowledgementBody {
    /// The anchor number.
    pub number: u64,
    /// The commitment the instruction carries, 32 bytes of hex.
    pub commitment: String,
}

/// The adapter's proof that its extrinsic for an anchor did not execute (`F-017`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchorFailureBody {
    /// The anchor number.
    pub number: u64,
}

/// The adapter has fetched and verified the evidence an authority submitted for an operation in
/// `Needs-Review`, and reported every record it found (`REQ-RC-5`, step 2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCheckedBody {
    /// The operation in `Needs-Review`.
    pub operation_reference: String,
    /// The submission's reference, as the pending evidence names it.
    pub submission: String,
}

/// An asset's status on the settlement layer (`DEC-20`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayerAssetStatusBody {
    /// It moves.
    Live,
    /// Frozen by its root authority.
    Frozen,
    /// Being destroyed.
    Destroying,
}

/// What the settlement layer states about the asset at a location (`DEC-20`): its own symbol and
/// decimals fields, and its status. No name: a name is never read (amendment 0011, `REQ-AS-8`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayerAssetFactsBody {
    /// Its symbol field, as the layer stores it (UTF-8; at most 64 bytes).
    pub symbol: String,
    /// Its decimals field.
    pub decimals: u8,
    /// Its status on the layer: a fact of the layer, never an outcome (`INV-11`).
    pub layer_status: LayerAssetStatusBody,
}

/// The adapter's attestation of one asset location (`DEC-20`, `F-029`): the asset its
/// configuration maps there, and what the layer states about it at a finalised height. A binding
/// reads it (`REQ-AS-8`); after the binding it is ignored (`REQ-AS-9`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttestationBody {
    /// The location, in the layer's textual form (`Here(101)`).
    pub location: String,
    /// The asset the adapter maps at the location.
    pub asset: String,
    /// The finalised height it was read at.
    pub height: u64,
    /// The facts; absent when the layer holds no asset, or no metadata, at the location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<LayerAssetFactsBody>,
    /// How long it holds, in milliseconds (capped by the core).
    pub lease: u64,
}

/// What the core made of an attestation (`DEC-20`). Informative: nothing is held on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttestationVerdictBody {
    /// `unregistered`, `book_only`, `binding` or `bound`: where the mapped asset stands.
    pub verdict: String,
    /// The registered asset the adapter maps there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// For `book_only`: why a binding to this location would be refused now (`absent`,
    /// `mapping`, `symbol`, `decimals`, `status`); absent when it would be admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<String>,
}

/// An attestation verdict on the wire.
pub fn attestation_verdict_body(v: &AttestationVerdict) -> AttestationVerdictBody {
    let (verdict, asset, mismatch) = match v {
        AttestationVerdict::Unregistered => ("unregistered", None, None),
        AttestationVerdict::BookOnly { asset, mismatch } => {
            ("book_only", Some(*asset), mismatch.map(|m| m.as_str()))
        }
        AttestationVerdict::Binding { asset } => ("binding", Some(*asset), None),
        AttestationVerdict::Bound { asset } => ("bound", Some(*asset), None),
    };
    AttestationVerdictBody {
        verdict: verdict.into(),
        asset: asset.map(|a| a.as_str().to_owned()),
        mismatch: mismatch.map(str::to_owned),
    }
}

fn layer_text(s: &str) -> Result<Vec<u8>, Refusal> {
    if s.len() > LAYER_TEXT_MAX {
        return Err(Refusal::EvidenceInsufficient);
    }
    Ok(s.as_bytes().to_vec())
}

fn attestation(a: &AttestationBody) -> Result<AssetAttestation, Refusal> {
    Ok(AssetAttestation {
        location: AssetLocation::parse(&a.location)?,
        asset: AssetId::parse(&a.asset)?,
        height: a.height,
        facts: match &a.facts {
            None => None,
            Some(f) => Some(LayerAssetFacts {
                symbol: layer_text(&f.symbol)?,
                decimals: f.decimals,
                status: match f.layer_status {
                    LayerAssetStatusBody::Live => LayerAssetStatus::Live,
                    LayerAssetStatusBody::Frozen => LayerAssetStatus::Frozen,
                    LayerAssetStatusBody::Destroying => LayerAssetStatus::Destroying,
                },
            }),
        },
        lease: a.lease,
    })
}

fn attestation_body(a: &AssetAttestation) -> AttestationBody {
    AttestationBody {
        location: a.location.as_str().to_owned(),
        asset: a.asset.as_str().to_owned(),
        height: a.height,
        facts: a.facts.as_ref().map(|f| LayerAssetFactsBody {
            symbol: String::from_utf8_lossy(&f.symbol).into_owned(),
            decimals: f.decimals,
            layer_status: match f.status {
                LayerAssetStatus::Live => LayerAssetStatusBody::Live,
                LayerAssetStatus::Frozen => LayerAssetStatusBody::Frozen,
                LayerAssetStatus::Destroying => LayerAssetStatusBody::Destroying,
            },
        }),
        lease: a.lease,
    }
}

/// Evidence submitted for an operation in `Needs-Review` that the adapter has not checked yet
/// (`REQ-RC-5`, step 2): where on the settlement layer to look.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PendingEvidenceBody {
    /// The submission's reference.
    pub submission: String,
    /// The operation in `Needs-Review`.
    pub operation_reference: String,
    /// The settlement layer's block.
    pub block: u64,
    /// The transaction's hash, hex.
    pub transaction: String,
}

/// One asset's position on the settlement layer, in the book's asset and minor units.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PositionBody {
    /// The asset.
    pub asset: String,
    /// Minor units, as a decimal-integer string.
    pub amount: String,
}

/// The settlement layer's positions at a finalized block (`F-017`, `REQ-PR-5`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PositionsBody {
    /// The instance's inbox length at that block: the cut.
    pub next_seq: u64,
    /// The block's height.
    pub height: u64,
    /// Each asset's position.
    pub positions: Vec<PositionBody>,
}

/// The checkpoint in flight, as the adapter reads it (`F-017`): a number and a commitment,
/// nothing else (`REQ-PR-2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchorInstructionBody {
    /// The anchor number.
    pub number: u64,
    /// The commitment, 32 bytes of hex.
    pub commitment: String,
    /// Whether the adapter has acknowledged it.
    pub acknowledged: bool,
}

/// The adapter's read of anchoring (`F-017`): the checkpoint in flight, and whether a settlement
/// comparison is due.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchorReadBody {
    /// The checkpoint in flight, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<AnchorInstructionBody>,
    /// Whether a final checkpoint has no settlement comparison yet (`AC-J3.1`).
    pub comparison_due: bool,
}

/// One asset of a settlement comparison (`F-017`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PositionRowBody {
    /// The asset.
    pub asset: String,
    /// What the book records as held on the settlement layer: minor units, a signed
    /// decimal-integer string.
    pub book: String,
    /// What the layer holds, minor units.
    pub layer: String,
    /// The layer's less the book's.
    pub difference: String,
}

/// A settlement comparison at an inbox cut (`F-017`, `REQ-PR-5`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComparisonBody {
    /// The cut: the inbox length at the finalized block compared.
    pub cut: u64,
    /// The block's height.
    pub height: u64,
    /// When the core compared.
    pub at: u64,
    /// The final checkpoints it is the recorded comparison of.
    pub anchors: Vec<u64>,
    /// Whether any asset differs.
    pub differs: bool,
    /// Every asset either side holds.
    pub rows: Vec<PositionRowBody>,
}

/// A canonical check of a coordinate already observed (`REQ-IN-3`, `REQ-IN-4`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckBody {
    /// The coordinate.
    pub coordinate: CoordinateBody,
    /// The height the layer's finality point has reached.
    pub finalised_through: u64,
    /// Whether the layer's record at the position is still the one observed.
    pub still_matches: bool,
}

/// The finalised head of an observation cycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadBody {
    /// The finalised height.
    pub height: u64,
    /// The instance's inbox length at that height.
    pub next_seq: u64,
    /// The account nonce of each signing key the adapter uses, read at that same finalised
    /// block (the owner's ruling of 2026-09-24).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nonces: Vec<KeyNonceBody>,
}

/// The latest anchor of the sequence before a new one (amendment 0039's `previous`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviousAnchorBody {
    /// Its number.
    pub number: u64,
    /// Its commitment, 32 bytes of hex.
    pub commitment: String,
}

/// A new sequence of the settlement layer's record started under the book (amendment 0041,
/// `[PENDING AMENDMENT 0041]`), as the layer holds it (`anchor_epoch()`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpochBody {
    /// The sequence's number.
    pub epoch: u32,
    /// The anchor number it starts after.
    pub base: u64,
    /// The position of its first entry.
    pub from_seq: u64,
    /// The block of the reset.
    pub at: u64,
    /// The book it names: its chainspec digest, 32 bytes of hex.
    pub book: String,
    /// The latest anchor of the sequence before, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PreviousAnchorBody>,
}

/// What the core made of a new sequence (amendment 0041).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpochResultBody {
    /// `started`, `recorded` or `refused`.
    pub outcome: String,
    /// For `started`: the finalised head the book holds now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<FinalisedHeadBody>,
    /// For `refused`: `other_book`, `not_later`, `in_flight` or `earlier_final`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// For `not_later`: the sequence the book holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<u32>,
    /// For `in_flight`: the first operation in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_reference: Option<String>,
    /// For `earlier_final`: the first position an earlier sequence's final record holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// A finalised head: a height and the position the record has reached there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinalisedHeadBody {
    /// The finalised height.
    pub height: u64,
    /// The position of the next entry at that height.
    pub next_seq: u64,
}

/// Why a range was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GapReasonBody {
    /// Pruned before it was read.
    Pruned,
    /// Unreadable for another reason.
    Unreadable,
}

/// A range `[from, to)` the adapter was forced to skip (`REQ-SA-7`, `AC-D2.2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GapBody {
    /// The first position skipped.
    pub from: u64,
    /// The first position after it.
    pub to: u64,
    /// Why.
    pub reason: GapReasonBody,
}

/// One position of a re-scan and the digest the layer holds there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayerEntryBody {
    /// The position.
    pub seq: u64,
    /// The digest, 32 bytes of hex.
    pub digest: String,
}

/// A re-scan of `[from, to)` (`REQ-IN-5`, `AC-D2.1`). Its route, `POST /v1/settlement/rescans`, is
/// withdrawn (`T-043-06`): the settlement adapter checks completeness against the layer's head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RescanBody {
    /// The first position.
    pub from: u64,
    /// The first position after the range.
    pub to: u64,
    /// Every position the layer holds in the range.
    pub layer: Vec<LayerEntryBody>,
}

/// One input of the settlement adapter, as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum AdapterCommandBody {
    /// `declare_adapter`.
    Declare(AdapterDeclarationBody),
    /// `ack_instruction`.
    Acknowledge(AcknowledgementBody),
    /// `ack_instruction`: what the adapter did about signing an instruction.
    Sign(SigningBody),
    /// `record_observation`: a record.
    Observe(ObservationBody),
    /// `record_observation`: a canonical check.
    Check(CheckBody),
    /// `record_observation`: the finalised head.
    Head(HeadBody),
    /// The evidence an authority submitted is checked (`REQ-RC-5`, step 2).
    EvidenceChecked(EvidenceCheckedBody),
    /// `record_observation`: a gap.
    Gap(GapBody),
    /// `record_observation`: a re-scan. No route submits one any more (`T-043-06`, amendment
    /// 0040): kept so a received-log entry from before still decodes, and the core decides it as
    /// the generation that runs it decides (from the chained generation on, refused).
    Rescan(RescanBody),
    /// `ack_instruction` of an anchor (`F-017`).
    AcknowledgeAnchor(AnchorAcknowledgementBody),
    /// An anchor's extrinsic provably did not execute (`F-017`).
    AnchorNotExecuted(AnchorFailureBody),
    /// The settlement layer's positions at a finalized block (`F-017`).
    Positions(PositionsBody),
    /// What the layer's metadata states about an asset location (`DEC-20`).
    Attest(AttestationBody),
    /// A new sequence of the layer's record started under the book (amendment 0041).
    Epoch(EpochBody),
}

/// An instruction in the outbox, as the adapter reads it (`REQ-SO-5`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstructionBody {
    /// The operation reference, which the layer's record of the movement carries.
    pub operation_reference: String,
    /// The asset.
    pub asset: String,
    /// The exact amount: minor units, as a decimal-integer string.
    pub amount: String,
    /// Where the value goes.
    pub destination: DestinationBody,
    /// The settlement-layer height after which it can no longer take effect (`REQ-SL-4`).
    pub expiry: u64,
    /// That a reservation exists for it: always true (`INV-13`).
    pub reserved: bool,
    /// Its identity, which the acknowledgement names: 32 bytes of hex.
    pub digest: String,
}

/// What a call does on the settlement layer (`F-027`, `DEC-18`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SettlementCallBody {
    /// Pay out a reservation.
    Withdraw {
        /// The asset.
        asset: String,
        /// The exact amount: minor units, as a decimal-integer string.
        amount: String,
        /// Where it goes.
        destination: DestinationBody,
    },
    /// Store an anchor: a number and 32 bytes.
    Anchor {
        /// The anchor number.
        number: u64,
        /// The commitment, 32 bytes of hex.
        commitment: String,
    },
    /// Bind a book-only asset (amendment 0011, `REQ-AS-8`): the layer records its holding of the
    /// asset at `location` as the deployment's position only if it equals `amount` exactly.
    Bind {
        /// The book's asset.
        asset: String,
        /// Its location on the layer, in the layer's textual form (`Here(100)`).
        location: String,
        /// The book's outstanding balance: minor units, as a decimal-integer string. May be zero.
        amount: String,
    },
}

/// One call of the call outbox, as the executor reads it (`F-027` T-027-01, `DEC-18`): the
/// envelope — reference, target, call, expiry — and whether the executor holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallBody {
    /// What the layer records the call's outcome under: 32 bytes of hex.
    pub reference: String,
    /// The deployment's instance the call acts for.
    pub target: u32,
    /// What the call does.
    pub call: SettlementCallBody,
    /// The settlement-layer height after which it can no longer take effect; absent for an
    /// anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<u64>,
    /// Whether the executor has acknowledged it.
    pub acknowledged: bool,
    /// For a withdrawal: that a reservation exists for it, always true (`INV-13`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<bool>,
    /// For a withdrawal: the instruction's identity, which the acknowledgement names, 32 bytes of
    /// hex. The executor recomputes it from the envelope and refuses a call where they differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

/// A call of the call outbox on the wire.
pub fn call_body(c: &OutboxCall) -> CallBody {
    let e = &c.envelope;
    CallBody {
        reference: hex::encode(e.reference),
        target: e.target.instance,
        call: match e.call {
            SettlementCall::Withdraw { asset, amount, to } => SettlementCallBody::Withdraw {
                asset: asset.as_str().to_owned(),
                amount: amount.get().to_string(),
                destination: destination_body(&to),
            },
            SettlementCall::Anchor { number, commitment } => SettlementCallBody::Anchor {
                number,
                commitment: hex::encode(commitment),
            },
            SettlementCall::Bind {
                asset,
                location,
                amount,
            } => SettlementCallBody::Bind {
                asset: asset.as_str().to_owned(),
                location: location.as_str().to_owned(),
                amount: amount.get().to_string(),
            },
        },
        expiry: e.expiry,
        acknowledged: c.acknowledged,
        reserved: matches!(e.call, SettlementCall::Withdraw { .. }).then_some(true),
        instruction: e.instruction().map(hex::encode),
    }
}

/// The executor's read of the call outbox (`F-027` T-027-01), in the in-process core.
pub fn calls<S: LeafStore, V: VaultBackend>(l: &Ledger<S, V>, caller: &Caller) -> QueryReply {
    match l.adapter_calls(caller) {
        Ok(items) => QueryReply::Calls {
            items: items.iter().map(call_body).collect(),
        },
        Err(error) => QueryReply::Refused { error },
    }
}

/// The core's verdict on a submission (`REQ-SI-5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerdictBody {
    /// Taken as evidence; the operation's state is the reply's.
    Applied,
    /// Stored; no transition.
    Ignored,
    /// The coordinate was seen before (`ERR-ObservationDuplicate` on the refusal path).
    AlreadyApplied,
    /// It relates to nothing aledgent holds (`ERR-ObservationUnassociated` on the refusal path).
    NotAssociated,
}

/// An operation a finalised head moved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MovedBody {
    /// The operation.
    pub operation_reference: String,
    /// The state it entered.
    pub state: String,
}

/// What a re-scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RescanResultBody {
    /// Positions the layer holds that were never ingested.
    pub missing: Vec<LayerEntryBody>,
    /// Final coordinates the core holds that the layer no longer shows.
    pub unknown: Vec<CoordinateBody>,
    /// Unassociated records of the range.
    pub unassociated: u64,
}

/// The adapter's status as the core recorded it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterStatusBody {
    /// Whether it serves.
    pub serving: bool,
    /// When the core recorded it.
    pub declared_at: u64,
    /// The first time it no longer holds.
    pub fresh_until: u64,
}

/// What a settlement command produced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettlementResultBody {
    /// A submission's verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<VerdictBody>,
    /// A condition a check detected on the way (`ERR-AmountMismatch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Operations a finalised head moved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moved: Vec<MovedBody>,
    /// What a re-scan found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rescan: Option<RescanResultBody>,
    /// A declaration as recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AdapterStatusBody>,
    /// The instruction `submit_outbound` assembled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<InstructionBody>,
    /// Where an anchor's checkpoint stands: `pending`, `submitted` or `final` (`F-017`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// The comparison positions gave (`F-017`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<ComparisonBody>,
    /// What the core made of an attestation (`DEC-20`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<AttestationVerdictBody>,
    /// What the core made of a new sequence (amendment 0041).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<EpochResultBody>,
}

// ------------------------------------------------------------------------------------ routes

/// A settlement route of the surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementRoute {
    /// `POST /v1/withdrawals/{reference}/submit`.
    SubmitOutbound,
    /// `POST /v1/ops/settlement`.
    Configure,
    /// `POST /v1/settlement/declarations`.
    Declare,
    /// `POST /v1/settlement/acknowledgements`.
    Acknowledge,
    /// `POST /v1/settlement/signings`.
    Sign,
    /// `POST /v1/settlement/observations`.
    Observe,
    /// `POST /v1/settlement/checks`.
    Check,
    /// `POST /v1/settlement/heads`.
    Head,
    /// `POST /v1/settlement/gaps`.
    Gap,
    /// `POST /v1/settlement/anchor-acknowledgements` (`F-017`).
    AcknowledgeAnchor,
    /// `POST /v1/settlement/anchor-failures` (`F-017`).
    AnchorNotExecuted,
    /// `POST /v1/settlement/positions` (`F-017`).
    Positions,
    /// `POST /v1/settlement/evidence-checks` (`REQ-RC-5`).
    EvidenceChecked,
    /// `POST /v1/settlement/attestations` (`DEC-20`).
    Attest,
    /// `POST /v1/settlement/epochs` (amendment 0041).
    Epoch,
}

fn json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Refusal> {
    let bytes: &[u8] = if bytes.is_empty() { b"{}" } else { bytes };
    serde_json::from_slice(bytes).map_err(|_| Refusal::MalformedCommand)
}

impl SettlementRoute {
    /// The request body's schema name in the OpenAPI document.
    pub const fn schema(self) -> &'static str {
        match self {
            SettlementRoute::SubmitOutbound => "SubmitOutboundBody",
            SettlementRoute::Configure => "ConfigureSettlementBody",
            SettlementRoute::Declare => "AdapterDeclarationBody",
            SettlementRoute::Acknowledge => "AcknowledgementBody",
            SettlementRoute::Sign => "SigningBody",
            SettlementRoute::Observe => "ObservationBody",
            SettlementRoute::Check => "CheckBody",
            SettlementRoute::Head => "HeadBody",
            SettlementRoute::Gap => "GapBody",
            SettlementRoute::AcknowledgeAnchor => "AnchorAcknowledgementBody",
            SettlementRoute::AnchorNotExecuted => "AnchorFailureBody",
            SettlementRoute::Positions => "PositionsBody",
            SettlementRoute::EvidenceChecked => "EvidenceCheckedBody",
            SettlementRoute::Attest => "AttestationBody",
            SettlementRoute::Epoch => "EpochBody",
        }
    }

    /// The command a request on this route carries, from its path parameter and its body.
    pub fn build(self, param: Option<String>, bytes: &[u8]) -> Result<CommandBody, Refusal> {
        let adapter = |b: AdapterCommandBody| Ok(CommandBody::Adapter(b));
        match self {
            SettlementRoute::SubmitOutbound => {
                let b: SubmitOutboundBody = json(bytes)?;
                Ok(CommandBody::SubmitOutbound(SubmitOutboundCommand {
                    operation: param.ok_or(Refusal::MalformedCommand)?,
                    destination: b.destination,
                }))
            }
            SettlementRoute::Configure => Ok(CommandBody::ConfigureSettlement(json(bytes)?)),
            SettlementRoute::Declare => adapter(AdapterCommandBody::Declare(json(bytes)?)),
            SettlementRoute::Acknowledge => adapter(AdapterCommandBody::Acknowledge(json(bytes)?)),
            SettlementRoute::Sign => adapter(AdapterCommandBody::Sign(json(bytes)?)),
            SettlementRoute::Observe => adapter(AdapterCommandBody::Observe(json(bytes)?)),
            SettlementRoute::Check => adapter(AdapterCommandBody::Check(json(bytes)?)),
            SettlementRoute::Head => adapter(AdapterCommandBody::Head(json(bytes)?)),
            SettlementRoute::Gap => adapter(AdapterCommandBody::Gap(json(bytes)?)),
            SettlementRoute::AcknowledgeAnchor => {
                adapter(AdapterCommandBody::AcknowledgeAnchor(json(bytes)?))
            }
            SettlementRoute::AnchorNotExecuted => {
                adapter(AdapterCommandBody::AnchorNotExecuted(json(bytes)?))
            }
            SettlementRoute::Positions => adapter(AdapterCommandBody::Positions(json(bytes)?)),
            SettlementRoute::EvidenceChecked => {
                adapter(AdapterCommandBody::EvidenceChecked(json(bytes)?))
            }
            SettlementRoute::Attest => adapter(AdapterCommandBody::Attest(json(bytes)?)),
            SettlementRoute::Epoch => adapter(AdapterCommandBody::Epoch(json(bytes)?)),
        }
    }
}

// ------------------------------------------------------------------------------------ parsing

/// A settlement command, validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettlementCommand {
    /// `submit_outbound`.
    SubmitOutbound {
        /// The reserved outbound leg.
        operation: OperationId,
        /// Where the value goes.
        destination: Destination,
    },
    /// `configure_settlement`.
    Configure(SettlementConfig),
    /// An input of the adapter's.
    Adapter(AdapterInput),
}

fn hex32(s: &str) -> Result<[u8; 32], Refusal> {
    hex::decode(s)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or(Refusal::MalformedCommand)
}

fn operation(s: &str) -> Result<OperationId, Refusal> {
    OperationId::parse(s).map_err(|_| Refusal::UnknownOperation)
}

fn minor(s: &str) -> Result<Minor, Refusal> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Refusal::PrecisionExceeded);
    }
    s.parse::<u64>()
        .map(Minor::new)
        .map_err(|_| Refusal::PrecisionExceeded)
}

fn coordinate(c: &CoordinateBody) -> Result<Coordinate, Refusal> {
    Ok(Coordinate {
        instance: c.instance,
        seq: c.seq,
        digest: hex32(&c.digest)?,
    })
}

/// A coordinate on the wire.
pub fn coordinate_body(c: &Coordinate) -> CoordinateBody {
    CoordinateBody {
        instance: c.instance,
        seq: c.seq,
        digest: hex::encode(c.digest),
    }
}

/// A destination from the wire.
pub fn destination(d: &DestinationBody) -> Result<Destination, Refusal> {
    Ok(match d {
        DestinationBody::Account(a) => Destination::Account(hex32(a)?),
        DestinationBody::Instance {
            instance,
            account_ref,
        } => Destination::Instance {
            instance: *instance,
            account_ref: hex32(account_ref)?,
        },
    })
}

/// A destination on the wire.
pub fn destination_body(d: &Destination) -> DestinationBody {
    match d {
        Destination::Account(a) => DestinationBody::Account(hex::encode(a)),
        Destination::Instance {
            instance,
            account_ref,
        } => DestinationBody::Instance {
            instance: *instance,
            account_ref: hex::encode(account_ref),
        },
    }
}

fn kind(k: EventKindBody) -> EventKind {
    match k {
        EventKindBody::Deposited => EventKind::Deposited,
        EventKindBody::Withdrawn => EventKind::Withdrawn,
        EventKindBody::WithdrawRefused => EventKind::WithdrawRefused,
        EventKindBody::Anchored => EventKind::Anchored,
        EventKindBody::AnchorRefused => EventKind::AnchorRefused,
        EventKindBody::Bound => EventKind::Bound,
        EventKindBody::BindRefused => EventKind::BindRefused,
    }
}

fn kind_body(k: EventKind) -> EventKindBody {
    match k {
        EventKind::Deposited => EventKindBody::Deposited,
        EventKind::Withdrawn => EventKindBody::Withdrawn,
        EventKind::WithdrawRefused => EventKindBody::WithdrawRefused,
        EventKind::Anchored => EventKindBody::Anchored,
        EventKind::AnchorRefused => EventKindBody::AnchorRefused,
        EventKind::Bound => EventKindBody::Bound,
        EventKind::BindRefused => EventKindBody::BindRefused,
    }
}

fn adapter_input(b: &AdapterCommandBody) -> Result<AdapterInput, Refusal> {
    Ok(match b {
        AdapterCommandBody::Declare(d) => AdapterInput::Declare {
            serving: d.serving,
            lease: d.lease,
            support: match &d.support {
                None => None,
                Some(legs) => {
                    let mut out = SupportDeclaration::default();
                    for l in legs {
                        out.legs
                            .insert((op_kind(l.kind), AssetId::parse(&l.asset)?));
                    }
                    Some(out)
                }
            },
        },
        AdapterCommandBody::Acknowledge(a) => AdapterInput::Acknowledge {
            op: operation(&a.operation_reference)?,
            instruction: hex32(&a.instruction)?,
        },
        AdapterCommandBody::Sign(b) => AdapterInput::Sign {
            op: operation(&b.operation_reference)?,
            instruction: hex32(&b.instruction)?,
            record: match &b.record {
                SigningRecordBody::Signed {
                    key,
                    nonce,
                    last_valid,
                } => SigningRecord::Signed(Signing {
                    key: hex32(key)?,
                    nonce: *nonce,
                    last_valid: *last_valid,
                }),
                SigningRecordBody::NeverSigned => SigningRecord::NeverSigned,
            },
        },
        AdapterCommandBody::Observe(o) => AdapterInput::Observe(Observation {
            // Replaced by the adapter principal's source in the core.
            source: SourceId([0; 16]),
            coordinate: coordinate(&o.coordinate)?,
            at: o.at,
            kind: kind(o.kind),
            payload: Payload(hex::decode(&o.payload).map_err(|_| Refusal::MalformedCommand)?),
            observed_finality: match o.observed_finality {
                ObservedFinalityBody::Provisional => ObservedFinality::Provisional,
                ObservedFinalityBody::Final => ObservedFinality::Final,
            },
            operation_ref: o
                .operation_reference
                .as_deref()
                .map(operation)
                .transpose()?,
            account_ref: o.account_ref.as_deref().map(hex32).transpose()?,
            movement: match &o.movement {
                None => None,
                Some(m) => Some(Movement {
                    asset: AssetId::parse(&m.asset)?,
                    amount: minor(&m.amount)?,
                }),
            },
            anchor: match &o.anchor {
                None => None,
                Some(a) => Some(AnchorFact {
                    number: a.number,
                    commitment: hex32(&a.commitment)?,
                    venue: a.venue.as_ref().map(venue).transpose()?,
                }),
            },
        }),
        AdapterCommandBody::Check(c) => AdapterInput::Check {
            coordinate: coordinate(&c.coordinate)?,
            finalised_through: c.finalised_through,
            still_matches: c.still_matches,
        },
        AdapterCommandBody::Head(h) => AdapterInput::Head {
            head: FinalisedHead {
                height: h.height,
                next_seq: h.next_seq,
            },
            nonces: h
                .nonces
                .iter()
                .map(|n| {
                    Ok(KeyNonce {
                        key: hex32(&n.key)?,
                        nonce: n.nonce,
                    })
                })
                .collect::<Result<Vec<_>, Refusal>>()?,
        },
        AdapterCommandBody::EvidenceChecked(e) => AdapterInput::EvidenceChecked {
            op: operation(&e.operation_reference)?,
            submission: operation(&e.submission)?,
        },
        AdapterCommandBody::Attest(a) => AdapterInput::Attest(attestation(a)?),
        AdapterCommandBody::Epoch(e) => AdapterInput::Epoch(SettlementEpoch {
            epoch: e.epoch,
            base: e.base,
            from_seq: e.from_seq,
            at: e.at,
            book: hex32(&e.book)?,
            previous: match &e.previous {
                Some(p) => Some(PreviousAnchor {
                    number: p.number,
                    commitment: hex32(&p.commitment)?,
                }),
                None => None,
            },
        }),
        AdapterCommandBody::Gap(g) => AdapterInput::Gap(Gap {
            from: g.from,
            to: g.to,
            reason: match g.reason {
                GapReasonBody::Pruned => GapReason::Pruned,
                GapReasonBody::Unreadable => GapReason::Unreadable,
            },
        }),
        AdapterCommandBody::Rescan(r) => AdapterInput::Rescan {
            from: r.from,
            to: r.to,
            layer: r
                .layer
                .iter()
                .map(|e| Ok((e.seq, hex32(&e.digest)?)))
                .collect::<Result<Vec<_>, Refusal>>()?,
        },
        AdapterCommandBody::AcknowledgeAnchor(a) => AdapterInput::AcknowledgeAnchor {
            number: a.number,
            commitment: hex32(&a.commitment)?,
        },
        AdapterCommandBody::AnchorNotExecuted(a) => {
            AdapterInput::AnchorNotExecuted { number: a.number }
        }
        AdapterCommandBody::Positions(p) => AdapterInput::Positions(PositionCut {
            next_seq: p.next_seq,
            height: p.height,
            positions: p
                .positions
                .iter()
                .map(|x| Ok((AssetId::parse(&x.asset)?, minor(&x.amount)?)))
                .collect::<Result<Vec<_>, Refusal>>()?,
        }),
    })
}

fn adapter_body(i: &AdapterInput) -> AdapterCommandBody {
    match i {
        AdapterInput::Declare {
            serving,
            lease,
            support,
        } => AdapterCommandBody::Declare(AdapterDeclarationBody {
            serving: *serving,
            lease: *lease,
            support: support.as_ref().map(|d| {
                d.legs
                    .iter()
                    .map(|(k, a)| AdapterLegBody {
                        kind: op_kind_body(*k),
                        asset: a.as_str().to_owned(),
                    })
                    .collect()
            }),
        }),
        AdapterInput::Acknowledge { op, instruction } => {
            AdapterCommandBody::Acknowledge(AcknowledgementBody {
                operation_reference: op.to_did(),
                instruction: hex::encode(instruction),
            })
        }
        AdapterInput::Observe(o) => AdapterCommandBody::Observe(ObservationBody {
            coordinate: coordinate_body(&o.coordinate),
            at: o.at,
            kind: kind_body(o.kind),
            payload: hex::encode(&o.payload.0),
            observed_finality: match o.observed_finality {
                ObservedFinality::Provisional => ObservedFinalityBody::Provisional,
                ObservedFinality::Final => ObservedFinalityBody::Final,
            },
            operation_reference: o.operation_ref.map(|op| op.to_did()),
            account_ref: o.account_ref.map(hex::encode),
            movement: o.movement.map(|m| MovementBody {
                asset: m.asset.as_str().to_owned(),
                amount: m.amount.get().to_string(),
            }),
            anchor: o.anchor.map(|a| AnchorFactBody {
                number: a.number,
                commitment: hex::encode(a.commitment),
                venue: a.venue.as_ref().map(venue_body),
            }),
        }),
        AdapterInput::Check {
            coordinate,
            finalised_through,
            still_matches,
        } => AdapterCommandBody::Check(CheckBody {
            coordinate: coordinate_body(coordinate),
            finalised_through: *finalised_through,
            still_matches: *still_matches,
        }),
        AdapterInput::Sign {
            op,
            instruction,
            record,
        } => AdapterCommandBody::Sign(SigningBody {
            operation_reference: op.to_did(),
            instruction: hex::encode(instruction),
            record: match record {
                SigningRecord::Signed(s) => SigningRecordBody::Signed {
                    key: hex::encode(s.key),
                    nonce: s.nonce,
                    last_valid: s.last_valid,
                },
                SigningRecord::NeverSigned => SigningRecordBody::NeverSigned,
            },
        }),
        AdapterInput::Head { head, nonces } => AdapterCommandBody::Head(HeadBody {
            height: head.height,
            next_seq: head.next_seq,
            nonces: nonces
                .iter()
                .map(|n| KeyNonceBody {
                    key: hex::encode(n.key),
                    nonce: n.nonce,
                })
                .collect(),
        }),
        AdapterInput::Gap(g) => AdapterCommandBody::Gap(GapBody {
            from: g.from,
            to: g.to,
            reason: match g.reason {
                GapReason::Pruned => GapReasonBody::Pruned,
                GapReason::Unreadable => GapReasonBody::Unreadable,
            },
        }),
        AdapterInput::Rescan { from, to, layer } => AdapterCommandBody::Rescan(RescanBody {
            from: *from,
            to: *to,
            layer: layer
                .iter()
                .map(|(seq, d)| LayerEntryBody {
                    seq: *seq,
                    digest: hex::encode(d),
                })
                .collect(),
        }),
        AdapterInput::AcknowledgeAnchor { number, commitment } => {
            AdapterCommandBody::AcknowledgeAnchor(AnchorAcknowledgementBody {
                number: *number,
                commitment: hex::encode(commitment),
            })
        }
        AdapterInput::AnchorNotExecuted { number } => {
            AdapterCommandBody::AnchorNotExecuted(AnchorFailureBody { number: *number })
        }
        AdapterInput::EvidenceChecked { op, submission } => {
            AdapterCommandBody::EvidenceChecked(EvidenceCheckedBody {
                operation_reference: op.to_did(),
                submission: submission.to_did(),
            })
        }
        AdapterInput::Attest(a) => AdapterCommandBody::Attest(attestation_body(a)),
        AdapterInput::Epoch(e) => AdapterCommandBody::Epoch(epoch_body(e)),
        AdapterInput::Positions(c) => AdapterCommandBody::Positions(PositionsBody {
            next_seq: c.next_seq,
            height: c.height,
            positions: c
                .positions
                .iter()
                .map(|(a, m)| PositionBody {
                    asset: a.as_str().to_owned(),
                    amount: m.get().to_string(),
                })
                .collect(),
        }),
    }
}

/// A comparison on the wire (`F-017`).
pub fn comparison_body(c: &Comparison) -> ComparisonBody {
    ComparisonBody {
        cut: c.cut,
        height: c.height,
        at: c.at,
        anchors: c.anchors.clone(),
        differs: c.differs(),
        rows: c
            .rows
            .iter()
            .map(|r| PositionRowBody {
                asset: r.asset.as_str().to_owned(),
                book: r.book.to_string(),
                layer: r.layer.get().to_string(),
                difference: r.difference().to_string(),
            })
            .collect(),
    }
}

/// The adapter's read of anchoring on the wire (`F-017`).
pub fn anchor_instruction_body(o: &AnchorOutbox) -> Option<AnchorInstructionBody> {
    o.instruction
        .map(|(i, acknowledged)| AnchorInstructionBody {
            number: i.number(),
            commitment: hex::encode(i.commitment()),
            acknowledged,
        })
}

/// The adapter's read of anchoring (`F-017`), in the in-process core.
pub fn anchor<S: LeafStore, V: VaultBackend>(l: &Ledger<S, V>, caller: &Caller) -> QueryReply {
    match l.adapter_anchor(caller) {
        Ok(o) => QueryReply::Anchor(AnchorReadBody {
            instruction: anchor_instruction_body(&o),
            comparison_due: o.comparison_due,
        }),
        Err(error) => QueryReply::Refused { error },
    }
}

impl SettlementCommand {
    /// Validate the settlement commands of `body`; `None` if it is another command.
    pub fn parse(body: &CommandBody) -> Option<Result<SettlementCommand, Refusal>> {
        Some(match body {
            CommandBody::SubmitOutbound(s) => (|| {
                Ok(SettlementCommand::SubmitOutbound {
                    operation: operation(&s.operation)?,
                    destination: destination(&s.destination)?,
                })
            })(),
            CommandBody::ConfigureSettlement(c) => (|| {
                let adapter =
                    IdentityId::parse(&c.adapter).map_err(|_| Refusal::MalformedCommand)?;
                let suspense = c
                    .suspense
                    .as_deref()
                    .map(|s| AccountId::parse(s).map_err(|_| Refusal::UnknownAccount))
                    .transpose()?;
                Ok(SettlementCommand::Configure(SettlementConfig {
                    authoritative: SourceId::of_identity(&adapter),
                    instance: c.instance,
                    suspense,
                    inbound_deadline: c.inbound_deadline,
                    expiry_window: c.expiry_window,
                    block_time: c.block_time,
                    finality_margin: c.finality_margin,
                }))
            })(),
            CommandBody::Adapter(a) => adapter_input(a).map(SettlementCommand::Adapter),
            _ => return None,
        })
    }

    /// The command in its canonical wire form.
    pub fn to_body(&self) -> CommandBody {
        match self {
            SettlementCommand::SubmitOutbound {
                operation,
                destination,
            } => CommandBody::SubmitOutbound(SubmitOutboundCommand {
                operation: operation.to_did(),
                destination: destination_body(destination),
            }),
            SettlementCommand::Configure(c) => {
                CommandBody::ConfigureSettlement(ConfigureSettlementBody {
                    adapter: c.authoritative.identity().to_did(),
                    instance: c.instance,
                    suspense: c.suspense.map(|a| a.to_did()),
                    inbound_deadline: c.inbound_deadline,
                    expiry_window: c.expiry_window,
                    block_time: c.block_time,
                    finality_margin: c.finality_margin,
                })
            }
            SettlementCommand::Adapter(i) => CommandBody::Adapter(adapter_body(i)),
        }
    }

    /// Whether it arrives on the operator surface (`configure_settlement`).
    pub fn is_operator(&self) -> bool {
        matches!(self, SettlementCommand::Configure(_))
    }

    /// The operation it acts under: the reserved leg of `submit_outbound`, otherwise the
    /// reference ingress minted.
    pub fn operation(&self, minted: OperationId) -> OperationId {
        match self {
            SettlementCommand::SubmitOutbound { operation, .. } => *operation,
            _ => minted,
        }
    }
}

// ------------------------------------------------------------------------------------ core side

/// An instruction on the wire.
pub fn instruction_body(i: &OutboundInstruction) -> InstructionBody {
    InstructionBody {
        operation_reference: i.operation_ref().to_did(),
        asset: i.asset().as_str().to_owned(),
        amount: i.amount().get().to_string(),
        destination: destination_body(&i.destination()),
        expiry: i.expiry(),
        reserved: i.reserved(),
        digest: hex::encode(i.digest()),
    }
}

fn verdict_body(v: &Verdict) -> VerdictBody {
    match v {
        Verdict::Applied(_) => VerdictBody::Applied,
        Verdict::Ignored => VerdictBody::Ignored,
        Verdict::AlreadyApplied => VerdictBody::AlreadyApplied,
        Verdict::NotAssociated => VerdictBody::NotAssociated,
    }
}

fn status_body(s: &AdapterStatus) -> AdapterStatusBody {
    AdapterStatusBody {
        serving: s.serving,
        declared_at: s.declared_at,
        fresh_until: s.fresh_until,
    }
}

fn moved(h: &HeadReport) -> Vec<MovedBody> {
    h.moved
        .iter()
        .map(|(op, s)| MovedBody {
            operation_reference: op.to_did(),
            state: s.name().to_owned(),
        })
        .collect()
}

fn rescan_body(r: &RescanReport) -> RescanResultBody {
    RescanResultBody {
        missing: r
            .missing
            .iter()
            .map(|(seq, d)| LayerEntryBody {
                seq: *seq,
                digest: hex::encode(d),
            })
            .collect(),
        unknown: r.unknown.iter().map(coordinate_body).collect(),
        unassociated: r.unassociated as u64,
    }
}

fn decided(op: &OperationId, state: Option<ShownBody>, result: SettlementResultBody) -> Outcome {
    Outcome::Decided {
        operation_reference: op.to_did(),
        state,
        result: CommandResult {
            settlement: Some(Box::new(result)),
            ..CommandResult::default()
        },
    }
}

/// Run a settlement command in the in-process core: its semantics are the ledger's; this only
/// dispatches.
pub fn execute<S: LeafStore, V: VaultBackend>(
    l: &mut Ledger<S, V>,
    ctx: &Ctx,
    c: SettlementCommand,
) -> Result<Outcome, Refusal> {
    let op = &ctx.op;
    Ok(match c {
        SettlementCommand::SubmitOutbound { destination, .. } => {
            let a = l.submit_outbound(ctx, destination)?;
            decided(
                op,
                a.status.map(ShownBody::from),
                SettlementResultBody {
                    instruction: Some(instruction_body(&a.value)),
                    ..SettlementResultBody::default()
                },
            )
        }
        SettlementCommand::Configure(config) => {
            l.configure_settlement(ctx, config)?;
            decided(op, None, SettlementResultBody::default())
        }
        SettlementCommand::Adapter(input) => {
            let acked = match &input {
                AdapterInput::Acknowledge { op, .. }
                | AdapterInput::Sign { op, .. }
                | AdapterInput::EvidenceChecked { op, .. } => Some(*op),
                _ => None,
            };
            match l.adapter(ctx, input)? {
                AdapterReply::Declared(s) => decided(
                    op,
                    None,
                    SettlementResultBody {
                        status: Some(status_body(&s)),
                        ..SettlementResultBody::default()
                    },
                ),
                AdapterReply::Acknowledged(shown) | AdapterReply::Signed(shown) => decided(
                    &acked.unwrap_or(*op),
                    Some(shown.into()),
                    SettlementResultBody::default(),
                ),
                AdapterReply::Verdict(v) => decided(
                    op,
                    match v {
                        Verdict::Applied(s) => Some(s.into()),
                        _ => None,
                    },
                    SettlementResultBody {
                        verdict: Some(verdict_body(&v)),
                        ..SettlementResultBody::default()
                    },
                ),
                AdapterReply::Checked(v) => decided(
                    op,
                    match v.verdict {
                        Verdict::Applied(s) => Some(s.into()),
                        _ => None,
                    },
                    SettlementResultBody {
                        verdict: Some(verdict_body(&v.verdict)),
                        condition: v.condition.map(|r| r.code().to_owned()),
                        ..SettlementResultBody::default()
                    },
                ),
                AdapterReply::Head(h) => decided(
                    op,
                    None,
                    SettlementResultBody {
                        moved: moved(&h),
                        ..SettlementResultBody::default()
                    },
                ),
                AdapterReply::Gap => decided(op, None, SettlementResultBody::default()),
                AdapterReply::Rescan(r) => decided(
                    op,
                    None,
                    SettlementResultBody {
                        rescan: Some(rescan_body(&r)),
                        ..SettlementResultBody::default()
                    },
                ),
                AdapterReply::Anchor(s) => decided(
                    op,
                    None,
                    SettlementResultBody {
                        anchor: Some(s.name().to_owned()),
                        ..SettlementResultBody::default()
                    },
                ),
                AdapterReply::Compared(c) => decided(
                    op,
                    None,
                    SettlementResultBody {
                        comparison: Some(comparison_body(&c)),
                        ..SettlementResultBody::default()
                    },
                ),
                AdapterReply::Evidence(s) => decided(
                    &acked.unwrap_or(*op),
                    Some(ledger_core::lifecycle::Shown::of(s).into()),
                    SettlementResultBody::default(),
                ),
                AdapterReply::Attested(v) => decided(
                    op,
                    None,
                    SettlementResultBody {
                        attestation: Some(attestation_verdict_body(&v)),
                        ..SettlementResultBody::default()
                    },
                ),
                AdapterReply::Epoch(o) => decided(
                    op,
                    None,
                    SettlementResultBody {
                        epoch: Some(epoch_result_body(&o)),
                        ..SettlementResultBody::default()
                    },
                ),
            }
        }
    })
}

/// A new sequence on the wire (amendment 0041).
pub fn epoch_body(e: &SettlementEpoch) -> EpochBody {
    EpochBody {
        epoch: e.epoch,
        base: e.base,
        from_seq: e.from_seq,
        at: e.at,
        book: hex::encode(e.book),
        previous: e.previous.map(|p| PreviousAnchorBody {
            number: p.number,
            commitment: hex::encode(p.commitment),
        }),
    }
}

/// A finalised head on the wire.
pub fn finalised_head_body(h: &FinalisedHead) -> FinalisedHeadBody {
    FinalisedHeadBody {
        height: h.height,
        next_seq: h.next_seq,
    }
}

/// What the core made of a new sequence, on the wire (amendment 0041).
pub fn epoch_result_body(o: &EpochOutcome) -> EpochResultBody {
    let mut b = EpochResultBody {
        outcome: String::new(),
        head: None,
        refusal: None,
        recorded: None,
        operation_reference: None,
        seq: None,
    };
    match o {
        EpochOutcome::Started(h) => {
            b.outcome = "started".into();
            b.head = Some(finalised_head_body(h));
        }
        EpochOutcome::Recorded => b.outcome = "recorded".into(),
        EpochOutcome::Refused(r) => {
            b.outcome = "refused".into();
            b.refusal = Some(
                match r {
                    EpochRefusal::OtherBook => "other_book",
                    EpochRefusal::NotLater { recorded } => {
                        b.recorded = Some(*recorded);
                        "not_later"
                    }
                    EpochRefusal::InFlight { op } => {
                        b.operation_reference = Some(op.to_did());
                        "in_flight"
                    }
                    EpochRefusal::EarlierFinal { seq } => {
                        b.seq = Some(*seq);
                        "earlier_final"
                    }
                }
                .into(),
            );
        }
    }
    b
}

/// Pending evidence on the wire (`REQ-RC-5`, step 2).
pub fn pending_evidence_body(e: &ledger_core::operator::EvidenceSubmission) -> PendingEvidenceBody {
    PendingEvidenceBody {
        submission: e.id.to_did(),
        operation_reference: e.op.to_did(),
        block: e.block,
        transaction: hex::encode(e.transaction),
    }
}

/// The adapter's read of the evidence it has not checked (`REQ-RC-5`), in the in-process core.
pub fn evidence<S: LeafStore, V: VaultBackend>(l: &Ledger<S, V>, caller: &Caller) -> QueryReply {
    match l.adapter_evidence(caller) {
        Ok(items) => QueryReply::Evidence {
            items: items.iter().map(pending_evidence_body).collect(),
        },
        Err(error) => QueryReply::Refused { error },
    }
}

/// The adapter's read of the outbox (`REQ-SO-7`), in the in-process core.
pub fn instructions<S: LeafStore, V: VaultBackend>(
    l: &Ledger<S, V>,
    caller: &Caller,
) -> QueryReply {
    match l.adapter_instructions(caller) {
        Ok(items) => QueryReply::Instructions {
            items: items.iter().map(instruction_body).collect(),
        },
        Err(error) => QueryReply::Refused { error },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_adapter_input_round_trips_through_its_canonical_form() {
        let body = CommandBody::Adapter(AdapterCommandBody::Observe(ObservationBody {
            anchor: None,
            coordinate: CoordinateBody {
                instance: 1,
                seq: 7,
                digest: hex::encode([3u8; 32]),
            },
            at: 12,
            kind: EventKindBody::Deposited,
            payload: String::new(),
            observed_finality: ObservedFinalityBody::Final,
            operation_reference: None,
            account_ref: Some(hex::encode([9u8; 32])),
            movement: Some(MovementBody {
                asset: "USD".into(),
                amount: "0010".into(),
            }),
        }));
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["command"], "adapter");
        assert_eq!(json["input"], "observe");
        let back: CommandBody = serde_json::from_value(json).unwrap();
        let c = SettlementCommand::parse(&back).unwrap().unwrap();
        let again = SettlementCommand::parse(&c.to_body()).unwrap().unwrap();
        assert_eq!(c, again);
    }

    /// Amendment 0041: the adapter's report of a new sequence round-trips, on its route and
    /// through the canonical form, and a book that is not 32 bytes of hex is malformed.
    #[test]
    fn t043_14_a_new_sequence_round_trips() {
        let e = SettlementEpoch {
            epoch: 2,
            base: 0,
            from_seq: 0,
            at: 40_195_735,
            book: [0x1B; 32],
            previous: Some(PreviousAnchor {
                number: 1,
                commitment: [0x91; 32],
            }),
        };
        let bytes = serde_json::to_vec(&epoch_body(&e)).unwrap();
        let body = SettlementRoute::Epoch.build(None, &bytes).unwrap();
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["command"], "adapter");
        assert_eq!(json["input"], "epoch");
        let c = SettlementCommand::parse(&body).unwrap().unwrap();
        assert_eq!(c, SettlementCommand::Adapter(AdapterInput::Epoch(e)));
        let again = SettlementCommand::parse(&c.to_body()).unwrap().unwrap();
        assert_eq!(c, again);
        let mut bad = epoch_body(&e);
        bad.book = "1b".into();
        let body = SettlementRoute::Epoch
            .build(None, &serde_json::to_vec(&bad).unwrap())
            .unwrap();
        assert_eq!(
            SettlementCommand::parse(&body).unwrap(),
            Err(Refusal::MalformedCommand)
        );
        // Each answer on the wire.
        let op = OperationId::from_ref(ledger_kernel::posting::OperationRef([6; 16]));
        let r = epoch_result_body(&EpochOutcome::Refused(EpochRefusal::InFlight { op }));
        assert_eq!(
            (r.outcome.as_str(), r.refusal.as_deref()),
            ("refused", Some("in_flight"))
        );
        assert_eq!(r.operation_reference, Some(op.to_did()));
        let r = epoch_result_body(&EpochOutcome::Started(FinalisedHead {
            height: 40_195_735,
            next_seq: 0,
        }));
        assert_eq!(r.outcome, "started");
        assert_eq!(r.head.map(|h| h.height), Some(40_195_735));
    }

    /// `REQ-RC-5`: the adapter's report that it checked submitted evidence round-trips.
    #[test]
    fn an_evidence_check_round_trips() {
        let op = OperationId::from_ref(ledger_kernel::posting::OperationRef([6; 16]));
        let submission = OperationId::from_ref(ledger_kernel::posting::OperationRef([7; 16]));
        let body = CommandBody::Adapter(AdapterCommandBody::EvidenceChecked(EvidenceCheckedBody {
            operation_reference: op.to_did(),
            submission: submission.to_did(),
        }));
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["input"], "evidence_checked");
        let back: CommandBody = serde_json::from_value(json).unwrap();
        let c = SettlementCommand::parse(&back).unwrap().unwrap();
        assert_eq!(
            c,
            SettlementCommand::Adapter(AdapterInput::EvidenceChecked { op, submission })
        );
        assert_eq!(SettlementCommand::parse(&c.to_body()).unwrap().unwrap(), c);
    }

    #[test]
    fn a_configuration_names_the_adapter_by_identity() {
        let adapter = IdentityId::mint(
            &OperationId::from_ref(ledger_kernel::posting::OperationRef([5; 16])),
            0,
        );
        let body = CommandBody::ConfigureSettlement(ConfigureSettlementBody {
            adapter: adapter.to_did(),
            instance: 1,
            suspense: None,
            inbound_deadline: 1,
            expiry_window: 2,
            block_time: 3,
            finality_margin: 4,
        });
        let c = SettlementCommand::parse(&body).unwrap().unwrap();
        let SettlementCommand::Configure(cfg) = &c else {
            panic!("a configuration")
        };
        assert_eq!(cfg.authoritative, SourceId::of_identity(&adapter));
        assert_eq!(c.to_body(), body);
    }
}
