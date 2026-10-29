// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement boundary, `CTR-SET` (`F-010` §2, SPEC §7.F): what the settlement adapter sends
//! aledgent, what aledgent sends it, and the verdicts in between.
//!
//! **No outcome crosses this boundary inwards** (`INV-11`, `REQ-IN-2`, `REQ-SA-3`). An
//! [`Observation`] names an event at a [`Coordinate`], with the layer's own payload and the
//! finality *as observed*; a [`CanonicalCheck`] reports two facts about a coordinate already
//! observed; a [`FinalisedHead`] reports how far the layer's finality point has reached. None of
//! them has a field for a status, a verdict or an outcome, and the library never branches on a
//! payload (`REQ-PO-4`). aledgent derives every state itself ([`crate::ledger::settlement`]).
//!
//! ```compile_fail,E0560
//! // INV-11: an observation cannot carry a status.
//! use ledger_core::settlement::Observation;
//! fn claim(o: Observation) -> Observation {
//!     Observation { status: "settled", ..o }
//! }
//! ```
//!
//! ## The coordinate
//!
//! A [`Coordinate`] is `(instance, seq, digest)`: the deployment's instance on the settlement
//! layer, the position in its sequenced inbox (`REQ-SL-9`), and the digest of the inbox entry at
//! that position, **which includes the height of the block that wrote it**. This is the format
//! `F-016` proposes and `F-019`'s inbox provides (`Inbox: (InstanceId, InboxSeq) → { at, event }`),
//! so a record re-written at the same position by a fork has another coordinate, and
//! `Observed → Orphaned` is reachable from the inbox (`REQ-IN-4`). The height itself travels
//! beside the coordinate ([`Observation::at`]), because the finality point is compared with it.
//!
//! ## Map
//!
//! - [`store`]: the settlement store, part of the service store: the unique coordinate set
//!   (`INV-12`), the one final record per position, the outbox of instructions, the finalised
//!   head, gaps and incidents.
//! - [`port`]: [`SettlementPort`], the adapter's and the operator's surface, object-safe.
//! - `sim` (feature `sim`): the simulated settlement layer and the driver that plays the adapter.

pub mod assets;
pub mod envelope;
pub mod evidence;
pub mod port;
pub mod store;

#[cfg(feature = "sim")]
pub mod driver;
#[cfg(feature = "sim")]
pub mod sim;

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

use crate::account::Time;
use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::authority::OperationKind;
use crate::id::{AccountId, OperationId};
use crate::lifecycle::Shown;
use crate::lifecycle::keys::{Canon, Evidence};
use crate::refusal::Refusal;
use crate::reservation::ReservationHandle;

pub use envelope::{CallEnvelope, CallReference, OutboxCall, SettlementCall, Target};
pub use evidence::{EvidenceFetch, Fetched, evidence_inputs};
pub use port::SettlementPort;

const INSTRUCTION_DOMAIN: &[u8] = b"aledgent/instruction/v1";

/// The code generation from which the book takes settlement as a chain of entries (amendment
/// 0040, `F-043`, `[PENDING AMENDMENT 0040]`): the settlement adapter checks completeness against
/// the head the layer holds, so the core takes no re-scan (`T-043-06`), and keeps no index by
/// position for one.
///
/// **The generation rule.** An input of an earlier generation is decided exactly as the code of
/// that generation decided it (`T-037-12`'s gate, the core release's `check-upgrade`): a book's
/// recorded re-scans replay, and the indexes they wrote are written again. Only an input decided
/// by this generation or a later one takes the new rules ([`crate::Ledger::set_generation`]).
///
/// It is 1: the test and the production books run their genesis code (generation 0), so the
/// first upgrade of either is generation 1. A book whose code is already past it when it takes
/// this build, or one founded on this build at generation 0, needs another ruling.
pub const CHAINED_SETTLEMENT: u64 = 1;

/// The code generation from which the book follows a new sequence of the settlement layer's record
/// under it, without a re-founding (amendment 0041, `T-043-14`, `[PENDING AMENDMENT 0041]`): the
/// settlement adapter reports the start ([`SettlementEpoch`], [`crate::ledger::settle_adapter::AdapterInput::Epoch`]),
/// and the core restarts its finalised head there, or refuses it ([`EpochRefusal`]). Before it,
/// the input is refused with `ERR-MalformedCommand` and writes nothing (the generation rule, as
/// for [`CHAINED_SETTLEMENT`]).
///
/// It is [`CHAINED_SETTLEMENT`] (amendment 0041, decision 1): no book has decided an input at that
/// generation yet, so one in-place upgrade takes both. Once a book has, a change here needs a new
/// generation.
pub const SETTLEMENT_EPOCHS: u64 = CHAINED_SETTLEMENT;

/// Who submitted an observation: an opaque identity of an observing source. Exactly one source
/// is authoritative for the book ([`SettlementConfig::authoritative`], `REQ-IN-6`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SourceId(pub [u8; 16]);

impl SourceId {
    /// The source an identity speaks as (`F-016`): the settlement adapter reaches the core as a
    /// principal of the book, and the source of what it submits is that principal's identity.
    /// So the operator names the authoritative adapter by its identity
    /// ([`SettlementConfig::authoritative`]), and no caller can speak as another's source.
    pub const fn of_identity(identity: &crate::id::IdentityId) -> SourceId {
        SourceId(*identity.as_bytes())
    }

    /// The identity this source speaks as: the inverse of [`SourceId::of_identity`]. For a
    /// source that was never an identity's, an identifier no identity holds.
    pub const fn identity(&self) -> crate::id::IdentityId {
        crate::id::IdentityId::from_bytes(self.0)
    }
}

/// The longest a settlement adapter's status declaration stays fresh (`F-016`, `REQ-SA-2`): five
/// minutes. An adapter that asks for longer gets this; one that stops declaring is unavailable
/// once its last declaration is older.
pub const MAX_ADAPTER_LEASE: Time = 300_000;

/// The settlement adapter's last status declaration (`F-016`, `REQ-SA-2`, `AC-E4.2`): whether it
/// serves, and until when the declaration holds. While it refuses, or once it is stale, the core
/// refuses `withdraw` and `submit_outbound` with `ERR-SettlementLayerUnavailable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdapterStatus {
    /// Whether every start-up assertion held and the adapter serves.
    pub serving: bool,
    /// When the core recorded it.
    pub declared_at: Time,
    /// The first time it no longer holds.
    pub fresh_until: Time,
}

impl AdapterStatus {
    /// Whether the adapter serves at `now`.
    pub const fn available(&self, now: Time) -> bool {
        self.serving && now < self.fresh_until
    }
}

/// Where a record sits on the settlement layer: native, unique and stable (`REQ-SL-8`,
/// `REQ-IN-1`). See the module documentation for the format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Coordinate {
    /// The deployment's instance on the settlement layer.
    pub instance: u32,
    /// The position in the instance's sequenced inbox.
    pub seq: u64,
    /// The digest of the inbox entry at that position, block height included.
    pub digest: [u8; 32],
}

impl Coordinate {
    /// The canonical encoding: instance and position big-endian, then the digest.
    pub fn bytes(&self) -> [u8; 44] {
        let mut b = [0u8; 44];
        b[..4].copy_from_slice(&self.instance.to_be_bytes());
        b[4..12].copy_from_slice(&self.seq.to_be_bytes());
        b[12..].copy_from_slice(&self.digest);
        b
    }

    /// The position `(instance, seq)`: at most one record per position is ever final.
    pub const fn position(&self) -> (u32, u64) {
        (self.instance, self.seq)
    }
}

/// The event a record names, as the settlement layer names it (`REQ-SI-1`). A name, not an
/// outcome: `WithdrawRefused` is the layer's record that it refused an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EventKind {
    /// Value arrived at the deployment's position, addressed to an account reference.
    Deposited,
    /// Value left the deployment's position by an instruction of the deployment.
    Withdrawn,
    /// The layer refused an instruction of the deployment and moved nothing.
    WithdrawRefused,
    /// An anchor was recorded (`F-017`).
    Anchored,
    /// The layer refused an anchor call of the deployment and stored nothing (`F-027`: the
    /// settlement contract records every call's outcome, a refused anchor included). Like
    /// [`EventKind::Anchored`], it names no operation of the book; the adapter emits the anchor
    /// again once its call provably did not take effect.
    AnchorRefused,
    /// The layer recorded a binding of a book-only asset at zero divergence (amendment 0011,
    /// `REQ-AS-8`): its asset and amount, under the binding's reference.
    Bound,
    /// The layer refused a binding and recorded nothing but the refusal: its holding was not the
    /// amount, or the asset was bound before.
    BindRefused,
}

impl EventKind {
    /// A stable one-byte code.
    pub const fn code(self) -> u8 {
        match self {
            EventKind::Deposited => 1,
            EventKind::Withdrawn => 2,
            EventKind::WithdrawRefused => 3,
            EventKind::Anchored => 4,
            EventKind::AnchorRefused => 5,
            EventKind::Bound => 6,
            EventKind::BindRefused => 7,
        }
    }
}

/// The finality of a record **as the submitter observed it** (`REQ-SI-1`). It is stored and never
/// trusted: `Final` needs a [`CanonicalCheck`] (`REQ-IN-3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ObservedFinality {
    /// Seen above the finality point.
    Provisional,
    /// Seen at or below it.
    Final,
}

/// The settlement layer's own bytes for a record, its error text included: opaque, kept, never
/// parsed (`REQ-SO-3`, `REQ-PO-4`).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Payload(pub Vec<u8>);

/// The value a record moved, in the book's terms (the adapter maps the layer's asset and units).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Movement {
    /// The asset.
    pub asset: AssetId,
    /// The exact amount, in minor units.
    pub amount: Minor,
}

/// An inbound submission from the adapter (`REQ-SI-1`). There is no field for an outcome, a
/// status or a verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Observation {
    /// Who observed it.
    pub source: SourceId,
    /// Where.
    pub coordinate: Coordinate,
    /// The height of the block that wrote the record, as the record carries it.
    pub at: u64,
    /// What the layer says happened.
    pub kind: EventKind,
    /// The layer's own bytes.
    pub payload: Payload,
    /// Finality as observed. Stored, never trusted.
    pub observed_finality: ObservedFinality,
    /// aledgent's operation reference, for a movement it originated (`REQ-SI-3`, `REQ-SL-8`).
    pub operation_ref: Option<OperationId>,
    /// The opaque account reference value was addressed to (`REQ-SL-7`).
    pub account_ref: Option<[u8; 32]>,
    /// The value moved, for a movement.
    pub movement: Option<Movement>,
    /// The anchor recorded, for an `Anchored` record (`F-017`): its number and commitment, as the
    /// layer stored them. The adapter maps the record into the book's terms, as it maps a
    /// movement; the core checks it against its own checkpoint, and it is never an outcome.
    pub anchor: Option<AnchorFact>,
}

/// An anchor as the settlement layer recorded it (`F-017`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AnchorFact {
    /// The anchor number.
    pub number: u64,
    /// The commitment.
    pub commitment: [u8; 32],
    /// The venue the record was read in (`F-027` T-027-04): the settlement pallet or the
    /// settlement contract. The checkpoint records it when it becomes final, so a disclosure
    /// names where its anchor lives and the verifier reads it there after a change of venue.
    /// `None` from an adapter that does not say; such a checkpoint's venue is the pallet under
    /// its instance, as before `F-027`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub venue: Option<Venue>,
}

pub use verify::Venue;

impl Observation {
    /// The digest of the observation as a transition's evidence: its coordinate.
    pub fn evidence(&self) -> Evidence {
        let mut c = Canon::new("observation");
        c.bytes(&self.coordinate.bytes());
        Evidence(c.digest().0)
    }
}

/// Two facts about a coordinate already observed, reported once per observation cycle
/// (`REQ-IN-3`, `REQ-IN-4`, `REQ-SL-3`). Neither has a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CanonicalCheck {
    /// Who checked.
    pub source: SourceId,
    /// The coordinate.
    pub coordinate: Coordinate,
    /// The height the layer's finality point has reached.
    pub finalised_through: u64,
    /// Whether the layer's canonical record at the coordinate's position is still the one
    /// observed.
    pub still_matches: bool,
}

impl CanonicalCheck {
    /// The digest of the check as a transition's evidence: `(coordinate, finalised_through,
    /// still_matches)`, so a repeated report of one cycle is one transition (`F-010` §4).
    pub fn evidence(&self) -> Evidence {
        let mut c = Canon::new("canonical-check");
        c.bytes(&self.coordinate.bytes())
            .u64(self.finalised_through)
            .u8(u8::from(self.still_matches));
        Evidence(c.digest().0)
    }
}

/// How far the layer's finality point has reached: its height, and the inbox length at that
/// height. Every inbox position below `next_seq` is final.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FinalisedHead {
    /// The finalised height.
    pub height: u64,
    /// The instance's inbox length at that height.
    pub next_seq: u64,
}

impl FinalisedHead {
    /// The digest of the head as a transition's evidence.
    pub fn evidence(&self) -> Evidence {
        let mut c = Canon::new("finalised-head");
        c.u64(self.height).u64(self.next_seq);
        Evidence(c.digest().0)
    }
}

/// The latest anchor of the sequence before a new one started, as the settlement layer keeps it
/// (amendment 0039's `previous`): recorded with the start, never compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PreviousAnchor {
    /// Its number.
    pub number: u64,
    /// Its commitment.
    pub commitment: [u8; 32],
}

/// A new sequence of the settlement layer's record started under the book (amendment 0041,
/// `REQ-SA-10` as proposed): governance reset the instance, so its positions restart at
/// `from_seq` from block `at`. The adapter reports it as the layer holds it (amendment 0039's
/// `anchor_epoch()`); the core restarts its finalised head at [`SettlementEpoch::start`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SettlementEpoch {
    /// The sequence's number: 1 for the first one governance starts, one more for each later one.
    pub epoch: u32,
    /// The anchor number the sequence starts after (not a position).
    pub base: u64,
    /// The position of the sequence's first entry: 0 for every reset of the current contract.
    pub from_seq: u64,
    /// The block of the reset.
    pub at: u64,
    /// The book the sequence names: its chainspec (seed) digest, as governance gave it.
    pub book: [u8; 32],
    /// The latest anchor of the sequence before, if it had one.
    pub previous: Option<PreviousAnchor>,
}

impl SettlementEpoch {
    /// The finalised head the sequence starts at: its block, and its first position.
    pub fn start(&self) -> FinalisedHead {
        FinalisedHead {
            height: self.at,
            next_seq: self.from_seq,
        }
    }
}

/// Why the core refused a new sequence (amendment 0041): each leaves the book where it was. The
/// refusal is one audit record ([`Incident::EpochRefused`]) and the adapter's alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochRefusal {
    /// The sequence names another book than this one's genesis digest, or this book has none.
    OtherBook,
    /// The book holds this sequence number or a later one already, and this is not it.
    NotLater {
        /// The sequence the book holds.
        recorded: u32,
    },
    /// An outbound instruction is not yet decided: its first position and expiry come from an
    /// earlier sequence, and the new one's records prove nothing about it.
    InFlight {
        /// The first such operation.
        op: OperationId,
    },
    /// The book holds a final record of an earlier sequence at a position the new one reuses
    /// (observed at or before the reset's block).
    EarlierFinal {
        /// The first such position.
        seq: u64,
    },
}

/// What the core made of a new sequence (amendment 0041).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochOutcome {
    /// Taken: the sequence is recorded and the finalised head is now this one.
    Started(FinalisedHead),
    /// The same sequence again: nothing changed.
    Recorded,
    /// Refused, and recorded as an incident.
    Refused(EpochRefusal),
}

/// A signing key's account nonce at a finalised head: how many of the key's transactions the
/// layer has included up to that block. The adapter reads it at the same finalised block as the
/// What one finalised head decides, chosen on the replica and recorded in its call (finding F18's
/// class, `F-032`, as a lifecycle tick's due deadlines are): the signed instructions whose nonce
/// fact the head decides, at most [`MAX_HEAD_NONCES`], and the instructed legs it may move, at
/// most [`MAX_HEAD_LEGS`], each in operation order. The core Agent reads each by key and refuses
/// one that is not in its index (`ERR-Internal`); the rest wait for the next head. Before, a head
/// read both indexes whole, and every leg's instruction and operation, so its witness grew with
/// every withdrawal in flight.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HeadWork {
    /// The instructions awaiting a nonce fact that the head decides.
    pub nonces: Vec<OperationId>,
    /// The live legs the head may move: dispose of, or resolve by proof in `Needs-Review`.
    pub legs: Vec<OperationId>,
}

/// The most nonce facts one finalised head decides ([`HeadWork`]). Measured natively
/// (`core-binding`'s `tests/heap.rs`), each is about 3.5 KB of the core Agent's heap.
pub const MAX_HEAD_NONCES: usize = 8;

/// The most legs one finalised head moves ([`HeadWork`]). Measured natively, a leg released on
/// a head (its expiry, its non-inclusion proven, the reservation released) is about 21 KB of the
/// core Agent's 256 KiB heap; a head deciding 8 nonces and moving 2 legs peaks at about 117 KB
/// with 600 legs in flight, and grows only with the depth of the proofs. The rest wait for the
/// next head: after a mass expiry, legs are disposed of at 2 a head.
pub const MAX_HEAD_LEGS: usize = 2;

/// head it sends it with (owner ruling, 2026-09-24).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct KeyNonce {
    /// The signing key's account on the layer.
    pub key: [u8; 32],
    /// Its account nonce at the head.
    pub nonce: u64,
}

/// The one transaction the adapter signed for an instruction: the key, the nonce it carries, and
/// the last layer height it can be included at (the end of its mortal era, at or before the
/// instruction's expiry, `REQ-SL-4`).
///
/// The adapter never signs a second transaction for an operation, and stores each one before it
/// submits it (`F-016`). So each nonce of a key belongs to one of the deployment's transactions,
/// and once the key's nonce at a finalised head has moved past `nonce`, the transaction that
/// carried it was included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Signing {
    /// The signing key's account on the layer.
    pub key: [u8; 32],
    /// The account nonce the transaction carries.
    pub nonce: u64,
    /// The last layer height the transaction can be included at.
    pub last_valid: u64,
}

/// What the adapter recorded about signing an instruction, once (owner ruling, 2026-09-24).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SigningRecord {
    /// It signed one transaction for the instruction, before submitting it.
    Signed(Signing),
    /// It will never sign one: the instruction can no longer be signed within its expiry (a
    /// tombstone, `F-016` decision 19). A later signing is refused.
    NeverSigned,
}

/// What a finalised head showed about a signed transaction's nonce. Decided once, at the first
/// head that shows it, and never changed: a nonce the adapter reuses after this transaction
/// provably died must not be read as this transaction's inclusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum NonceFact {
    /// The key's nonce had moved past the transaction's at this finalised height: it was
    /// included.
    Consumed {
        /// The finalised height.
        height: u64,
    },
    /// At this finalised height, at or past the transaction's last valid height, the key's nonce
    /// had not moved past the transaction's: it was not included, and never can be.
    Unconsumed {
        /// The finalised height.
        height: u64,
    },
}

/// What aledgent derived from a submission, returned to the submitter (`REQ-SI-5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Taken as evidence for an operation, which now shows this state.
    Applied(Shown),
    /// Stored, and caused no transition: a corroborating source, an anchor, a fact that moves
    /// nothing yet.
    Ignored,
    /// The coordinate was seen before (`ERR-ObservationDuplicate`, `INV-12`).
    AlreadyApplied,
    /// Recorded, but it relates to no operation or account aledgent holds
    /// (`ERR-ObservationUnassociated`).
    NotAssociated,
}

impl Verdict {
    /// The verdict a submitter reads from a result: `Ok` carries it, and the two refusals that
    /// are verdicts rather than failures map to theirs (`REQ-SI-4`). Any other refusal is a
    /// failure, and has none.
    pub fn of(result: &Result<Verdict, Refusal>) -> Option<Verdict> {
        match result {
            Ok(v) => Some(*v),
            Err(Refusal::ObservationDuplicate) => Some(Verdict::AlreadyApplied),
            Err(Refusal::ObservationUnassociated) => Some(Verdict::NotAssociated),
            Err(_) => None,
        }
    }
}

/// What a canonical check did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The verdict.
    pub verdict: Verdict,
    /// A named condition the check detected on the way: `ERR-AmountMismatch` when an outbound leg
    /// executed for another amount than was reserved (`REQ-OP-2`). The difference was posted.
    pub condition: Option<Refusal>,
}

/// The adapter's declaration of what it supports (`REQ-SA-1`): pairs of operation kind and
/// asset. `Redeem` is an outbound leg, `Issue` an inbound one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SupportDeclaration {
    /// The supported (kind, asset) pairs.
    pub legs: BTreeSet<(OperationKind, AssetId)>,
}

impl SupportDeclaration {
    /// Whether `(kind, asset)` is supported.
    pub fn supports(&self, kind: OperationKind, asset: &AssetId) -> bool {
        self.legs.contains(&(kind, *asset))
    }
}

/// Where an outbound instruction sends value, in the settlement layer's terms (`F-019`'s
/// destinations).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Destination {
    /// A plain account on the layer.
    Account([u8; 32]),
    /// Another deployment: its instance and an opaque account reference it resolves (`REQ-SL-6`).
    Instance {
        /// The receiving instance.
        instance: u32,
        /// Its account reference.
        account_ref: [u8; 32],
    },
}

/// An outbound instruction to the adapter (`REQ-SO-5`), assembled by aledgent (`REQ-SO-2`).
///
/// It has one constructor, and it takes a [`ReservationHandle`], which only an open reservation
/// in the index yields (`F-004`). There is no instruction without a reservation (`INV-13`,
/// `REQ-SO-1`).
///
/// ```compile_fail,E0061
/// // INV-13, AC-E1.1: no instruction can be built without a reservation.
/// use ledger_core::settlement::{Destination, OutboundInstruction};
/// let _ = OutboundInstruction::new(Destination::Account([0; 32]), 10);
/// ```
///
/// ```compile_fail,E0451
/// // Nor can a handle be forged outside the reserve path.
/// use ledger_core::reservation::ReservationHandle;
/// let _ = ReservationHandle { op: todo!(), asset: todo!(), amount: todo!() };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutboundInstruction {
    operation_ref: OperationId,
    asset: AssetId,
    amount: NonZeroMinor,
    destination: Destination,
    expiry: u64,
}

impl OutboundInstruction {
    /// The instruction that pays out reservation `handle` to `destination`, valid until the
    /// layer's height `expiry` (`REQ-SL-4`). The amount and the asset are the reservation's.
    pub fn new(
        handle: &ReservationHandle,
        destination: Destination,
        expiry: u64,
    ) -> OutboundInstruction {
        OutboundInstruction {
            operation_ref: handle.op(),
            asset: handle.asset(),
            amount: handle.amount(),
            destination,
            expiry,
        }
    }

    /// The operation reference, which the layer's record of the movement carries (`REQ-SL-8`).
    pub const fn operation_ref(&self) -> OperationId {
        self.operation_ref
    }

    /// The asset.
    pub const fn asset(&self) -> AssetId {
        self.asset
    }

    /// The exact amount, in minor units: the reservation's.
    pub const fn amount(&self) -> NonZeroMinor {
        self.amount
    }

    /// The destination.
    pub const fn destination(&self) -> Destination {
        self.destination
    }

    /// The layer height after which the instruction can no longer take effect.
    pub const fn expiry(&self) -> u64 {
        self.expiry
    }

    /// That a reservation exists for it (`REQ-SO-5`): always, by construction.
    pub const fn reserved(&self) -> bool {
        true
    }

    /// The canonical bytes handed to the adapter. After a restart the outbox delivers the same
    /// bytes under the same identity (`REQ-SO-7`).
    pub fn bytes(&self) -> Vec<u8> {
        instruction_bytes(
            &self.operation_ref,
            &self.asset,
            self.amount,
            self.destination,
            self.expiry,
        )
    }

    /// The instruction's identity: `H("aledgent/instruction/v1", bytes)`. The adapter's
    /// acknowledgement names it.
    pub fn digest(&self) -> [u8; 32] {
        blake2b_256(INSTRUCTION_DOMAIN, &[&self.bytes()])
    }
}

/// The canonical bytes of an instruction with these fields: what [`OutboundInstruction::bytes`]
/// returns. Public so that the settlement adapter, which receives an instruction's fields over
/// the wire, can recompute its identity and refuse one whose fields and digest disagree
/// (`F-016`). Building an [`OutboundInstruction`] still needs a reservation (`INV-13`).
pub fn instruction_bytes(
    operation_ref: &OperationId,
    asset: &AssetId,
    amount: NonZeroMinor,
    destination: Destination,
    expiry: u64,
) -> Vec<u8> {
    let mut c = Canon::new("outbound-instruction");
    c.bytes(operation_ref.as_bytes())
        .bytes(asset.as_str().as_bytes())
        .u64(amount.get().get());
    match destination {
        Destination::Account(a) => {
            c.u8(1).bytes(&a);
        }
        Destination::Instance {
            instance,
            account_ref,
        } => {
            c.u8(2).bytes(&instance.to_be_bytes()).bytes(&account_ref);
        }
    }
    c.u64(expiry).u8(1);
    let mut out = Vec::new();
    out.extend_from_slice(&c.digest().0);
    out.extend_from_slice(operation_ref.as_bytes());
    out
}

/// The identity of an instruction with these fields: [`OutboundInstruction::digest`].
pub fn instruction_digest(
    operation_ref: &OperationId,
    asset: &AssetId,
    amount: NonZeroMinor,
    destination: Destination,
    expiry: u64,
) -> [u8; 32] {
    let bytes = instruction_bytes(operation_ref, asset, amount, destination, expiry);
    blake2b_256(INSTRUCTION_DOMAIN, &[&bytes])
}

/// The deployment's settlement configuration, set by the operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SettlementConfig {
    /// The one source whose observations write to the book (`REQ-IN-6`).
    pub authoritative: SourceId,
    /// The deployment's instance on the settlement layer.
    pub instance: u32,
    /// The deployment's Suspense account, which an outbound leg that executed for more than was
    /// reserved takes the excess from (`F-010` §9).
    pub suspense: Option<AccountId>,
    /// The deadline of an unoriginated inbound arrival, from its observation.
    pub inbound_deadline: Time,
    /// How many layer heights an instruction stays valid for, from the last finalised height.
    pub expiry_window: u64,
    /// An estimate of the time one layer height takes (`F-016` measures it; research 05 §8.7).
    pub block_time: Time,
    /// The time finality may take past an expiry, on top of the estimate.
    pub finality_margin: Time,
}

/// Why a range of inbox positions was never ingested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum GapReason {
    /// The layer no longer serves it: it was pruned before it was read (`REQ-SA-7`).
    Pruned,
    /// It could not be read for another reason.
    Unreadable,
}

/// A range of inbox positions `[from, to)` the adapter was forced to skip, explicit and visible
/// (`REQ-SA-7`, `AC-D2.2`). Its positions are never marked ingested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Gap {
    /// The first position skipped.
    pub from: u64,
    /// The first position after it.
    pub to: u64,
    /// Why.
    pub reason: GapReason,
}

/// A condition aledgent detected and raises to its operators. An incident is a record, never a
/// repair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Incident {
    /// An outbound leg executed for another amount than was reserved (`REQ-OP-2`,
    /// `ERR-AmountMismatch`). The difference was posted, or the operation waits for review.
    AmountMismatch {
        /// The operation.
        op: OperationId,
        /// What was reserved.
        reserved: Minor,
        /// What the layer moved.
        executed: Minor,
    },
    /// A record relates to no operation or account aledgent holds (`ERR-ObservationUnassociated`).
    Unassociated {
        /// Its coordinate.
        coordinate: Coordinate,
    },
    /// A record could not be taken as evidence (`ERR-EvidenceInsufficient`): it names another
    /// instance, lacks the value it moved, or does not fit its operation.
    Unusable {
        /// Its coordinate.
        coordinate: Coordinate,
    },
    /// A record of an operation that no longer waits for one: execution after an expiry, or a
    /// second execution.
    LateEvidence {
        /// The operation.
        op: OperationId,
        /// The record.
        coordinate: Coordinate,
    },
    /// The layer refused an instruction (`WithdrawRefused`): reported so an operator sees why.
    /// Once the record is final and the key's nonce agrees, it is a proved rejection and the
    /// reservation is released at once (amendment 0006 item 3).
    FinalFailure {
        /// The operation.
        op: OperationId,
        /// The record.
        coordinate: Coordinate,
    },
    /// The settling posting at `Final` was refused. The operation stays `Final` and waits for
    /// review (`INV-23`).
    SettlementRefused {
        /// The operation.
        op: OperationId,
        /// The refusal.
        refusal: Refusal,
    },
    /// A range was skipped (`REQ-SA-7`).
    Gap(Gap),
    /// A re-scan found a record on the layer that aledgent did not ingest (`REQ-IN-5`).
    Missing {
        /// The position.
        seq: u64,
        /// The digest the layer holds there.
        digest: [u8; 32],
    },
    /// A re-scan found that aledgent holds a final record the layer no longer shows.
    Unknown {
        /// The coordinate aledgent holds as final there.
        coordinate: Coordinate,
    },
    /// **The transaction was included, but no inbox record names it.** The signing key's nonce
    /// at a finalised head moved past the transaction's, while every inbox position it could
    /// have written is ingested, final, and names something else. The inbox and the nonce
    /// disagree; nothing is released, and the operation is in `Needs-Review` (owner ruling,
    /// 2026-09-24).
    IncludedWithoutRecord {
        /// The operation.
        op: OperationId,
        /// The signing key.
        key: [u8; 32],
        /// The transaction's nonce.
        nonce: u64,
    },
    /// A binding of a book-only asset did not bind (amendment 0011, `REQ-AS-8`): the layer
    /// refused it (its holding was not the book's outstanding balance, or the asset was bound
    /// before), or recorded another asset or amount under its reference. The record says which.
    BindRefused {
        /// The asset.
        asset: AssetId,
        /// The layer's final record.
        coordinate: Coordinate,
    },
    /// A new sequence of the layer's record was refused (amendment 0041): the book stays on the
    /// head it holds, and the adapter alerts.
    EpochRefused {
        /// The sequence's number.
        epoch: u32,
        /// Why.
        refusal: EpochRefusal,
    },
}

/// What a re-scan found (`REQ-IN-5`, library side of `REQ-SA-7`). Every difference is also an
/// incident.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RescanReport {
    /// Positions the layer holds that were never ingested, with their digests.
    pub missing: Vec<(u64, [u8; 32])>,
    /// Final coordinates aledgent holds that the layer no longer shows.
    pub unknown: Vec<Coordinate>,
    /// How many records of the range are unassociated.
    pub unassociated: usize,
}

/// What an observation cycle's finalised head did to outbound operations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeadReport {
    /// Operations moved, and to where.
    pub moved: Vec<(OperationId, crate::lifecycle::State)>,
}

/// The canonical form of an outbound instruction in an export (`F-014`).
mod canon;
