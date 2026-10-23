// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Anchoring and disclosure: the records (`F-017`, `DEC-12`, SPEC §4.6, §5.10, §7.E).
//!
//! ## Checkpoints
//!
//! A [`Checkpoint`] binds a stated entry sequence to the kernel's composite root at that
//! sequence, under a number, and (`v3`, `T-037-03`, `DEC-26` (a)) to the book's genesis, the code
//! version that produced that state, its construction and the upgrade-authority mode in force:
//!
//! ```text
//! commitment = blake2b-256("aledgent/anchor/v3" ‖ construction ‖ genesis ‖ code ‖ authority ‖ instance ‖ number ‖ entry_sequence ‖ kernel_root)
//! ```
//!
//! ([`verify::commitment_v3`], the verifier's own function). Every field is aledgent's own: the
//! genesis is the chainspec's seed digest, the code the digest of the writer's code manifest, the
//! mode a digest of the book's record; nothing a platform supplies (`REQ-RT-19`). `construction`
//! names the state commitment's construction ([`construction`], `REQ-PR-10`, `DEC-25`), and a
//! checkpoint carries its own, so [`Checkpoint::consistent`] checks it in the construction it was
//! made in, not the build's.
//!
//! A checkpoint with no [`Checkpoint::binding`] is a `v2` checkpoint, made before `v3`:
//! `blake2b-256("aledgent/anchor/v2" ‖ construction ‖ instance ‖ number ‖ entry_sequence ‖
//! kernel_root)` ([`verify::commitment`]), or for construction 1 (the opt-in feature `smt-v1`, owner
//! ruling cs-q04) the version-1 form, `"aledgent/anchor/v1"` without the byte. It still checks.
//! Every checkpoint created now is `v3`. It is created inside one core invocation, from the kernel's root and its
//! posting sequence read in the same committed state, so the two belong to each other (`INV-19`, `REQ-PR-7`). `entry_sequence` is the sequence of the
//! **last** entry it covers: every entry up to and including it, and nothing else.
//!
//! Creating a checkpoint puts its [`AnchorInstruction`] — a number and 32 bytes, nothing more
//! (`REQ-PR-2`, `AC-I1.1`, `INV-17`) — in the adapter's outbox in the same commit (`REQ-RT-1`).
//! The accepted proposal of the plan: the settlement layer's `number` is the anchor counter, and
//! the entry sequence sits inside the commitment, so the public sees how many anchors there are
//! and their order, not how many entries the book holds (`REQ-PV-2`).
//!
//! A checkpoint moves `Pending → Submitted → Final`. Only the settlement adapter's report of the
//! `Anchored` record at a **finalized** block, carrying the same commitment, makes it `Final`
//! (`REQ-PR-8`); a mismatch raises an [`AnchorIncident`] and it never becomes final. A checkpoint
//! that is not final cannot be cited (`ERR-AnchorNotFinal`, `AC-J1.3`). The pallet takes numbers
//! in strict order, so **one checkpoint is in flight at a time**: the next is created only once
//! the last is final.
//!
//! ## The store and its rules
//!
//! [`AnchorStore`] is part of the service store ([`crate::state::State`]); its writes are
//! [`Write::Anchor`](crate::state::Write)s. Whatever writes to it:
//!
//! - numbers run from 1 with no gap; a checkpoint is created only after the last is final, over a
//!   later entry sequence, and only with the commitment its own fields produce;
//! - a checkpoint's status only moves forward to `Final`, which absorbs; the one step back,
//!   `Submitted → Pending`, re-emits it after the adapter proved its extrinsic did not execute;
//! - incidents, comparisons and issued disclosures are appended, never removed.
//!
//! ## Map
//!
//! - [`clerk`]: the clerk key behind [`clerk::ClerkSigner`], and its in-process implementation.
//! - [`disclosure`]: the four assertions in the book's terms, and the request.

pub mod clerk;
pub mod disclosure;

use alloc::string::String;
use alloc::vec::Vec;

use crate::account::Time;
use crate::asset::{AssetId, Minor};
use crate::id::OperationId;
use crate::leaves::{LeafHost, LeafMap, Leaves, Slot, kind};
use crate::settlement::{Coordinate, Venue};

pub use verify::{
    CodeBinding, SIGNED_ASSERTION_LABEL, checkpoint_commitment, commitment, commitment_v3,
};

/// The construction of the book's state commitment (`REQ-PR-10`, `DEC-25`): the root version of
/// the build: [`verify::CONSTRUCTION_V2`] by default, [`verify::CONSTRUCTION_V1`] with the opt-in
/// feature `smt-v1` (owner ruling cs-q04). A deployment chooses it at genesis by how it is built;
/// every checkpoint it makes is in it, its anchor commitment binds it and its disclosures name it.
pub const fn construction() -> u8 {
    ledger_kernel::compact_smt::Version::ACTIVE.byte()
}

/// The default anchoring interval: one hour, as `REQ-PR-1` requires since amendment 0009 closed
/// `OQ-11`. It is configuration ([`AnchoringConfig::interval`]), and a deployment sets one hour.
pub const DEFAULT_ANCHOR_INTERVAL: Time = 3_600_000;

/// The deployment's assurance declaration, as disclosures carry it (`REQ-AR-5`, `REQ-AR-6`):
/// the digest of the machine-readable declaration `F-015` generates, and its summary.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssuranceDeclaration {
    /// The digest of the declaration document.
    pub digest: [u8; 32],
    /// Its summary, as a line of text. A disclosure prefixes it with [`SIGNED_ASSERTION_LABEL`].
    pub summary: String,
}

/// Anchoring configuration, part of the deployment's ([`crate::lifecycle::LifecycleConfig`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AnchoringConfig {
    /// The interval between checkpoints (`REQ-PR-1`, `AC-J1.1`): one hour (amendment 0009).
    pub interval: Time,
    /// The assurance declaration disclosures carry. Without one, no disclosure is issued
    /// (`ERR-DisclosureUnsupported`): a disclosure must carry the assurance level (`REQ-AR-5`).
    pub assurance: Option<AssuranceDeclaration>,
}

impl Default for AnchoringConfig {
    fn default() -> Self {
        AnchoringConfig {
            interval: DEFAULT_ANCHOR_INTERVAL,
            assurance: None,
        }
    }
}

/// Where a checkpoint's publication stands (`REQ-PR-8`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CheckpointStatus {
    /// Created; its instruction is in the outbox, not yet acknowledged by the adapter.
    Pending,
    /// The adapter holds the instruction durably and submits it.
    Submitted,
    /// Its `Anchored` record was reported at a finalized block with the same commitment. The only
    /// status that can be cited.
    Final,
}

impl CheckpointStatus {
    /// Its name.
    pub const fn name(self) -> &'static str {
        match self {
            CheckpointStatus::Pending => "pending",
            CheckpointStatus::Submitted => "submitted",
            CheckpointStatus::Final => "final",
        }
    }
}

/// A checkpoint of the book. See the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// Its number, from 1: the settlement layer's anchor number.
    pub number: u64,
    /// The deployment's instance on the settlement layer.
    pub instance: u32,
    /// The sequence of the last entry it covers.
    pub entry_sequence: u64,
    /// The kernel's composite root at that sequence.
    pub kernel_root: [u8; 32],
    /// The construction of the state commitment it was made in (`REQ-PR-10`, `T-037-03`).
    pub construction: u8,
    /// What a `v3` checkpoint binds beside (`T-037-03`): the book's genesis, the code version and
    /// the upgrade-authority mode. `None` for a `v2` checkpoint (module documentation).
    pub binding: Option<CodeBinding>,
    /// The commitment the anchor carries.
    pub commitment: [u8; 32],
    /// When the core created it.
    pub created_at: Time,
    /// Where its publication stands.
    pub status: CheckpointStatus,
    /// The coordinate of its `Anchored` record, once final.
    pub coordinate: Option<Coordinate>,
    /// The venue its anchor lives in, once final, as the settlement adapter read it there
    /// (`F-027` T-027-04, `DEC-17`): a deployment can change venue, and a disclosure resting on
    /// this checkpoint names where the verifier reads its anchor. `None` when final by a record
    /// that named no venue: the settlement pallet, under [`Checkpoint::instance`].
    pub venue: Option<Venue>,
    /// The height of the block that wrote its `Anchored` record, once final, as the record
    /// carries it (amendment 0011 R3, `AC-M2.4`): a notification names it with the coordinate, so
    /// a recipient can find the anchor on the settlement layer without reading the inbox.
    pub height: Option<u64>,
}

impl Checkpoint {
    /// Where its anchor lives: the venue recorded at finality, or the settlement pallet under its
    /// instance when none was (`F-027` T-027-04).
    pub const fn anchored_in(&self) -> Venue {
        match self.venue {
            Some(v) => v,
            None => Venue::Pallet {
                instance: self.instance,
            },
        }
    }

    /// The instruction that publishes it.
    pub const fn instruction(&self) -> AnchorInstruction {
        AnchorInstruction {
            number: self.number,
            commitment: self.commitment,
        }
    }

    /// Whether its fields produce its commitment, in its own construction and form
    /// ([`checkpoint_commitment`]): a `v3` checkpoint's binds its code, construction and mode, and a
    /// `v2` checkpoint's still checks.
    pub fn consistent(&self) -> bool {
        checkpoint_commitment(
            self.construction,
            self.binding.as_ref(),
            self.instance,
            self.number,
            self.entry_sequence,
            &self.kernel_root,
        ) == self.commitment
    }
}

/// The instruction that publishes a checkpoint: **a number and 32 bytes, nothing else**
/// (`REQ-PR-2`, `AC-I1.1`, `INV-17`). It has no field an account, identity, amount, counterparty
/// or asset could go in, and no constructor but [`Checkpoint::instruction`].
///
/// ```compile_fail,E0451
/// // INV-17: an anchor instruction cannot be built with anything but a checkpoint's fields.
/// use ledger_core::anchor::AnchorInstruction;
/// let _ = AnchorInstruction { number: 1, commitment: [0; 32] };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorInstruction {
    number: u64,
    commitment: [u8; 32],
}

/// The build gate of `INV-17` for anchored values: the anchor instruction is exactly a `u64` and
/// 32 bytes. Adding a field fails the build here.
const _: () = assert!(core::mem::size_of::<AnchorInstruction>() == 8 + 32);

impl AnchorInstruction {
    /// The anchor number.
    pub const fn number(&self) -> u64 {
        self.number
    }

    /// The commitment.
    pub const fn commitment(&self) -> [u8; 32] {
        self.commitment
    }
}

/// What the settlement adapter reads of anchoring (its one read, beside the outbox of outbound
/// instructions): the checkpoint in flight, and whether a settlement comparison is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorOutbox {
    /// The in-flight checkpoint's instruction, and whether the adapter has acknowledged it.
    pub instruction: Option<(AnchorInstruction, bool)>,
    /// A final checkpoint has no settlement comparison yet (`AC-J3.1`): the adapter reports the
    /// positions at its finalized head.
    pub comparison_due: bool,
}

/// A condition anchoring detected and raises to its operators. A record, never a repair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorIncident {
    /// The settlement layer recorded the anchor of this number with another commitment. The
    /// checkpoint never becomes final (`REQ-PR-8`), and anchoring stops until an operator acts:
    /// the number is taken.
    CommitmentMismatch {
        /// The anchor number.
        number: u64,
        /// The commitment aledgent created.
        expected: [u8; 32],
        /// The commitment the layer recorded.
        observed: [u8; 32],
        /// Where.
        coordinate: Coordinate,
    },
    /// The settlement layer recorded an anchor aledgent has no checkpoint for.
    UnknownAnchor {
        /// The anchor number.
        number: u64,
        /// Its commitment.
        commitment: [u8; 32],
        /// Where.
        coordinate: Coordinate,
    },
    /// The book's record of value held on the settlement layer differs from the layer's own
    /// position at a cut (`REQ-PR-5`, `AC-J3.2`). Tolerance is zero (`NFR-9`).
    PositionDifference {
        /// The cut: the inbox length at the finalized block compared.
        cut: u64,
        /// The asset.
        asset: AssetId,
        /// What the book records.
        book: i128,
        /// What the layer holds.
        layer: Minor,
    },
}

/// The settlement layer's positions at a finalized block, as the adapter reports them, in the
/// book's assets and minor units (`REQ-PR-5`). An observation of the layer, never an outcome
/// (`INV-11`): the core compares it with its own record.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PositionCut {
    /// The instance's inbox length at that block: the cut `k`.
    pub next_seq: u64,
    /// The block's height.
    pub height: u64,
    /// The instance's position in each asset the adapter serves.
    pub positions: Vec<(AssetId, Minor)>,
}

/// One asset of a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionRow {
    /// The asset.
    pub asset: AssetId,
    /// What the book records as held on the settlement layer at the cut.
    pub book: i128,
    /// What the layer holds.
    pub layer: Minor,
}

impl PositionRow {
    /// The layer's position less the book's, exact.
    pub fn difference(&self) -> i128 {
        i128::from(self.layer.get()) - self.book
    }
}

/// A settlement comparison at an inbox cut (`REQ-PR-5`, `AC-J3.1`, `AC-J3.2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The cut: the inbox length at the finalized block compared.
    pub cut: u64,
    /// The block's height.
    pub height: u64,
    /// When the core compared.
    pub at: Time,
    /// The final checkpoints this comparison is the recorded comparison of.
    pub anchors: Vec<u64>,
    /// The operator's `compare_settlement` that asked for it, if one did.
    pub requested: Option<OperationId>,
    /// Every asset either side holds.
    pub rows: Vec<PositionRow>,
}

impl Comparison {
    /// Whether any asset differs, by as little as one minor unit.
    pub fn differs(&self) -> bool {
        self.rows.iter().any(|r| r.difference() != 0)
    }
}

/// One write to the anchor store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorWrite {
    /// Create the next checkpoint, `Pending`, with its instruction in the outbox.
    Create(Checkpoint),
    /// The adapter holds the checkpoint's instruction durably: `Pending → Submitted`.
    Acknowledge(u64),
    /// The adapter proved its extrinsic did not execute: `Submitted → Pending`, emitted again.
    Reemit(u64),
    /// The `Anchored` record with the same commitment is final at `coordinate`, in `venue`.
    Final {
        /// The checkpoint.
        number: u64,
        /// Its record.
        coordinate: Coordinate,
        /// The venue the record was read in, if the adapter named one (`F-027`).
        venue: Option<Venue>,
        /// The height of the block that wrote the record (amendment 0011 R3).
        height: u64,
    },
    /// Raise an incident.
    Incident(AnchorIncident),
    /// Record a comparison in the history.
    Compare(Comparison),
    /// Replace the latest comparison.
    Latest(Comparison),
    /// Record the disclosure an operation issued, for its repeat (`REQ-OP-10`).
    Issued {
        /// The operation.
        op: OperationId,
        /// The disclosure's bytes.
        bytes: Vec<u8>,
    },
    /// Record what an `anchor_checkpoint` answered, for its repeat.
    OnDemand {
        /// The operation.
        op: OperationId,
        /// The checkpoint it answered with, if any.
        number: Option<u64>,
    },
}

/// Why the anchor store refused a write. The whole batch is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorRefusal {
    /// The number is not the next one.
    OutOfOrder,
    /// The last checkpoint is not final yet.
    InFlight,
    /// The entry sequence is not after the last checkpoint's.
    NotAfter,
    /// The commitment is not the one the checkpoint's fields produce.
    Commitment,
    /// A new checkpoint that is not `Pending`, or carries a coordinate.
    NotNew,
    /// No such checkpoint.
    Missing,
    /// The checkpoint's status does not allow the step.
    Status,
    /// A comparison names a checkpoint that is not final.
    NotFinal,
    /// The operation already has an entry.
    Duplicate,
}

/// How to undo one applied write.
#[derive(Clone, Debug)]
pub(crate) enum AnchorUndo {
    PopCheckpoint(u64),
    Status(
        u64,
        CheckpointStatus,
        Option<Coordinate>,
        Option<Venue>,
        Option<u64>,
    ),
    PopIncident,
    PopComparison(u64),
    Latest(Option<Comparison>),
    Unissue(OperationId),
    UnOnDemand(OperationId),
}

/// The anchor store. See the module documentation for its rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
///
/// Each collection is leaves of the service tree (`crate::leaves`). The last checkpoint's number
/// is kept apart, so creating a checkpoint reads the last one only.
pub struct AnchorStore {
    checkpoints: Leaves<u64, Checkpoint, { kind::ANCHOR_CHECKPOINT }>,
    last: Slot<u64, { kind::ANCHOR_LAST }>,
    incidents: Slot<Vec<AnchorIncident>, { kind::ANCHOR_INCIDENTS }>,
    comparisons: Slot<Vec<Comparison>, { kind::ANCHOR_COMPARISONS }>,
    latest: Slot<Option<Comparison>, { kind::ANCHOR_LATEST }>,
    issued: Leaves<OperationId, Vec<u8>, { kind::DISCLOSURE_ISSUED }>,
    on_demand: Leaves<OperationId, Option<u64>, { kind::DISCLOSURE_ON_DEMAND }>,
    /// The greatest checkpoint number a recorded comparison names: every final checkpoint up to
    /// it is compared ([`AnchorStore::uncompared_since`]). Derived from the comparisons.
    compared: Slot<u64, { kind::ANCHOR_COMPARED }>,
}

impl LeafHost for AnchorStore {
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>) {
        out.push(&self.checkpoints);
        out.push(&self.last);
        out.push(&self.incidents);
        out.push(&self.comparisons);
        out.push(&self.latest);
        out.push(&self.issued);
        out.push(&self.on_demand);
        out.push(&self.compared);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.checkpoints);
        out.push(&mut self.last);
        out.push(&mut self.incidents);
        out.push(&mut self.comparisons);
        out.push(&mut self.latest);
        out.push(&mut self.issued);
        out.push(&mut self.on_demand);
        out.push(&mut self.compared);
    }
}

impl AnchorStore {
    /// An empty store.
    pub fn new() -> AnchorStore {
        AnchorStore::default()
    }

    /// Every checkpoint, by number.
    pub fn checkpoints(&self) -> impl DoubleEndedIterator<Item = &Checkpoint> {
        self.checkpoints.values()
    }

    /// Checkpoint `number`.
    pub fn checkpoint(&self, number: u64) -> Option<&Checkpoint> {
        self.checkpoints.get(&number)
    }

    /// The last checkpoint created.
    pub fn latest(&self) -> Option<&Checkpoint> {
        match *self.last.get() {
            0 => None,
            n => self.checkpoints.get(&n),
        }
    }

    /// The last final checkpoint.
    ///
    /// One checkpoint is in flight at a time, so it is the last one created or the one before
    /// it: two reads by number, never a walk of the collection. Every committed transition reads
    /// it for its notification (amendment 0011 R3, `AC-M2.4`), so the read set a core invocation
    /// witnesses stays two leaves however many checkpoints the book has taken.
    pub fn latest_final(&self) -> Option<&Checkpoint> {
        let last = *self.last.get();
        [last, last.saturating_sub(1)]
            .into_iter()
            .filter(|n| *n > 0)
            .filter_map(|n| self.checkpoints.get(&n))
            .find(|c| c.status == CheckpointStatus::Final)
    }

    /// The checkpoint in flight: the last one, if it is not final.
    pub fn in_flight(&self) -> Option<&Checkpoint> {
        self.latest()
            .filter(|c| c.status != CheckpointStatus::Final)
    }

    /// Every incident, in order.
    pub fn incidents(&self) -> &[AnchorIncident] {
        self.incidents.get()
    }

    /// Every recorded comparison, in order.
    pub fn comparisons(&self) -> &[Comparison] {
        self.comparisons.get()
    }

    /// The latest comparison made, recorded in the history or not.
    pub fn latest_comparison(&self) -> Option<&Comparison> {
        self.latest.get().as_ref()
    }

    /// The final checkpoints no recorded comparison names yet, read by number from the greatest
    /// one a comparison names (finding F18's class, `F-032`): what [`AnchorStore::uncompared`]
    /// finds, without reading every checkpoint and every comparison. They are the same set
    /// because checkpoints become final in number order, one in flight at a time, their records
    /// in that order in the inbox, and a comparison names every uncompared final checkpoint
    /// below its cut: the ones it leaves out are those above it.
    pub fn uncompared_since(&self) -> Vec<u64> {
        let last = *self.last.get();
        (self.compared.get().saturating_add(1)..=last)
            .filter(|n| {
                self.checkpoints
                    .get(n)
                    .is_some_and(|c| c.status == CheckpointStatus::Final)
            })
            .collect()
    }

    /// The final checkpoints no recorded comparison names yet (`AC-J3.1`). It reads every
    /// checkpoint and every comparison: for queries only.
    pub fn uncompared(&self) -> Vec<u64> {
        let compared: alloc::collections::BTreeSet<u64> = self
            .comparisons
            .get()
            .iter()
            .flat_map(|c| c.anchors.iter().copied())
            .collect();
        self.checkpoints
            .values()
            .filter(|c| c.status == CheckpointStatus::Final && !compared.contains(&c.number))
            .map(|c| c.number)
            .collect()
    }

    /// The disclosure operation `op` issued.
    pub fn issued(&self, op: &OperationId) -> Option<&[u8]> {
        self.issued.get(op).map(Vec::as_slice)
    }

    /// Every issued disclosure, by operation.
    pub fn all_issued(&self) -> impl Iterator<Item = (&OperationId, &Vec<u8>)> {
        self.issued.iter()
    }

    /// What `anchor_checkpoint` under `op` answered.
    pub fn on_demand(&self, op: &OperationId) -> Option<Option<u64>> {
        self.on_demand.get(op).copied()
    }

    /// Every on-demand answer, by operation.
    pub fn all_on_demand(&self) -> impl Iterator<Item = (&OperationId, &Option<u64>)> {
        self.on_demand.iter()
    }

    fn status_step(
        &mut self,
        number: u64,
        allowed: &[CheckpointStatus],
        to: CheckpointStatus,
        coordinate: Option<Coordinate>,
        venue: Option<Venue>,
        height: Option<u64>,
    ) -> Result<AnchorUndo, AnchorRefusal> {
        let c = self
            .checkpoints
            .get_mut(&number)
            .ok_or(AnchorRefusal::Missing)?;
        if !allowed.contains(&c.status) {
            return Err(AnchorRefusal::Status);
        }
        let undo = AnchorUndo::Status(number, c.status, c.coordinate, c.venue, c.height);
        c.status = to;
        if coordinate.is_some() {
            c.coordinate = coordinate;
        }
        if venue.is_some() {
            c.venue = venue;
        }
        if height.is_some() {
            c.height = height;
        }
        Ok(undo)
    }

    /// Apply one write under the store's rules.
    pub(crate) fn apply(&mut self, w: &AnchorWrite) -> Result<AnchorUndo, AnchorRefusal> {
        use CheckpointStatus::{Final, Pending, Submitted};
        match w {
            AnchorWrite::Create(c) => {
                let last = self.latest();
                if c.number != last.map_or(1, |l| l.number + 1) {
                    return Err(AnchorRefusal::OutOfOrder);
                }
                if let Some(l) = last {
                    if l.status != Final {
                        return Err(AnchorRefusal::InFlight);
                    }
                    if c.entry_sequence <= l.entry_sequence || c.instance != l.instance {
                        return Err(AnchorRefusal::NotAfter);
                    }
                }
                if c.status != Pending
                    || c.coordinate.is_some()
                    || c.venue.is_some()
                    || c.height.is_some()
                {
                    return Err(AnchorRefusal::NotNew);
                }
                if !c.consistent() {
                    return Err(AnchorRefusal::Commitment);
                }
                self.checkpoints.insert(c.number, *c);
                self.last.set(c.number);
                Ok(AnchorUndo::PopCheckpoint(c.number))
            }
            AnchorWrite::Acknowledge(n) => {
                self.status_step(*n, &[Pending], Submitted, None, None, None)
            }
            AnchorWrite::Reemit(n) => self.status_step(*n, &[Submitted], Pending, None, None, None),
            AnchorWrite::Final {
                number,
                coordinate,
                venue,
                height,
            } => self.status_step(
                *number,
                &[Pending, Submitted],
                Final,
                Some(*coordinate),
                *venue,
                Some(*height),
            ),
            AnchorWrite::Incident(i) => {
                self.incidents.get_mut().push(*i);
                Ok(AnchorUndo::PopIncident)
            }
            AnchorWrite::Compare(c) => {
                if c.anchors
                    .iter()
                    .any(|n| self.checkpoints.get(n).is_none_or(|k| k.status != Final))
                {
                    return Err(AnchorRefusal::NotFinal);
                }
                self.comparisons.get_mut().push(c.clone());
                let before = *self.compared.get();
                let named = c.anchors.iter().copied().max().unwrap_or(0);
                if named > before {
                    self.compared.set(named);
                }
                Ok(AnchorUndo::PopComparison(before))
            }
            AnchorWrite::Latest(c) => Ok(AnchorUndo::Latest(self.latest.set(Some(c.clone())))),
            AnchorWrite::Issued { op, bytes } => {
                if self.issued.contains_key(op) {
                    return Err(AnchorRefusal::Duplicate);
                }
                self.issued.insert(*op, bytes.clone());
                Ok(AnchorUndo::Unissue(*op))
            }
            AnchorWrite::OnDemand { op, number } => {
                if self.on_demand.contains_key(op) {
                    return Err(AnchorRefusal::Duplicate);
                }
                self.on_demand.insert(*op, *number);
                Ok(AnchorUndo::UnOnDemand(*op))
            }
        }
    }

    pub(crate) fn undo(&mut self, u: AnchorUndo) {
        match u {
            AnchorUndo::PopCheckpoint(n) => {
                self.checkpoints.remove(&n);
                self.last.set(n - 1);
            }
            AnchorUndo::Status(n, s, c, v, h) => {
                if let Some(k) = self.checkpoints.get_mut(&n) {
                    k.status = s;
                    k.coordinate = c;
                    k.venue = v;
                    k.height = h;
                }
            }
            AnchorUndo::PopIncident => {
                self.incidents.get_mut().pop();
            }
            AnchorUndo::PopComparison(before) => {
                self.comparisons.get_mut().pop();
                self.compared.set(before);
            }
            AnchorUndo::Latest(c) => {
                self.latest.set(c);
            }
            AnchorUndo::Unissue(op) => {
                self.issued.remove(&op);
            }
            AnchorUndo::UnOnDemand(op) => {
                self.on_demand.remove(&op);
            }
        }
    }

    /// Rebuild a store from its exported records, through its own rules (`F-014`).
    pub(crate) fn restore(
        checkpoints: Vec<Checkpoint>,
        incidents: Vec<AnchorIncident>,
        comparisons: Vec<Comparison>,
        latest: Option<Comparison>,
        issued: Vec<(OperationId, Vec<u8>)>,
        on_demand: Vec<(OperationId, Option<u64>)>,
    ) -> Result<AnchorStore, AnchorRefusal> {
        let mut s = AnchorStore::new();
        for c in checkpoints {
            let status = c.status;
            let coordinate = c.coordinate;
            let venue = c.venue;
            let height = c.height;
            s.apply(&AnchorWrite::Create(Checkpoint {
                status: CheckpointStatus::Pending,
                coordinate: None,
                venue: None,
                height: None,
                ..c
            }))?;
            match (status, coordinate, venue, height) {
                (CheckpointStatus::Pending, None, None, None) => {}
                (CheckpointStatus::Submitted, None, None, None) => {
                    s.apply(&AnchorWrite::Acknowledge(c.number))?;
                }
                (CheckpointStatus::Final, Some(coordinate), venue, Some(height)) => {
                    s.apply(&AnchorWrite::Final {
                        number: c.number,
                        coordinate,
                        venue,
                        height,
                    })?;
                }
                _ => return Err(AnchorRefusal::Status),
            }
        }
        for i in incidents {
            s.apply(&AnchorWrite::Incident(i))?;
        }
        for c in comparisons {
            s.apply(&AnchorWrite::Compare(c))?;
        }
        s.latest.set(latest);
        for (op, bytes) in issued {
            s.apply(&AnchorWrite::Issued { op, bytes })?;
        }
        for (op, number) in on_demand {
            s.apply(&AnchorWrite::OnDemand { op, number })?;
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests;
