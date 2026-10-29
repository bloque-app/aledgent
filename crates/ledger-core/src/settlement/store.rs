// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement store (`F-010` §3, §7, §10): part of the service store
//! ([`crate::state::State`]). Its writes are [`Write::Settlement`](crate::state::Write)s, applied
//! in the same all-or-nothing batch as the operation store, the reservation index and the
//! attribution records, so an observation and the transition it causes commit together or not at
//! all (`REQ-RT-1`, in process).
//!
//! Its rules hold for any writer, whatever the application above does (the **store** layer of
//! SPEC §9):
//!
//! - **A coordinate is ingested once, permanently** (`INV-12`). A second ingest of a coordinate
//!   is refused, whoever writes it, and nothing is ever removed.
//! - **One final record per position.** At most one coordinate per `(instance, seq)` is ever
//!   marked final, and a coordinate is final or orphaned, never both; both marks absorb. A fork
//!   cannot make two records at one position count.
//! - **An instruction per operation, once** (`REQ-SO-7`). The outbox keeps it for ever; its
//!   acknowledgement is recorded once.
//! - **A signing record per instruction, once, after its acknowledgement** (owner ruling,
//!   2026-09-24): the one transaction signed for it, or the adapter's word that it never will
//!   sign one. **Its nonce fact is decided once** and never changes.
//! - **The finalised head never goes back within a sequence.** Only the start of a strictly later
//!   sequence of the layer's record moves it back, to that sequence's start (amendment 0041,
//!   [`SettlementWrite::Epoch`]); the store refuses any other head that goes back, and a sequence
//!   that is not later than the one it holds.
//! - **A gap end is cleared only by a start** (amendment 0041, ruling q28, made exact): the start
//!   of a strictly later sequence clears the untagged gap end and every gap end of an earlier
//!   sequence, and keeps those of the started sequence or later. Otherwise each only rises.
//! - **Gaps and incidents are appended, never removed.** A re-scan's difference (`Missing`,
//!   `Unknown`) is one incident however many re-scans find it: the store indexes the ones
//!   recorded by key, so a re-scan that finds nothing new reads that index, never the incidents.
//!   Re-scans, and the two indexes only they read (`RESCAN_RECORDED`, `INGESTED_AT`), belong to
//!   the generations before [`super::CHAINED_SETTLEMENT`] (`T-043-06`): their inputs replay as
//!   they were decided, and a later generation writes neither.
//! - **The latest attestation of each location** replaces the last (`DEC-20`).
//! - **One binding of an asset in flight at a time**, and a binding's outcome is decided once: a
//!   pending binding becomes bound or refused, and never changes after (`REQ-AS-8`).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use super::assets::{AssetBind, Attested, BindStatus};
use super::{
    AdapterStatus, Coordinate, FinalisedHead, Gap, Incident, NonceFact, Observation,
    OutboundInstruction, SettlementConfig, SettlementEpoch, SigningRecord, SourceId,
    SupportDeclaration,
};
use crate::asset::{AssetId, AssetLocation};
use crate::id::OperationId;
use crate::leaves::{LeafHost, LeafMap, Leaves, Slot, kind};

/// How an ingested record stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Observed; not yet shown final.
    Provisional,
    /// The finality point passed it and the canonical record matched.
    Final,
    /// The canonical record no longer matched.
    Orphaned,
}

/// One ingested record from the authoritative source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ingested {
    /// The observation as submitted.
    pub observation: Observation,
    /// The operation it was associated with, if any.
    pub op: Option<OperationId>,
    /// How it stands.
    pub standing: Standing,
}

/// An instruction in the outbox, with what aledgent knew when it assembled it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instructed {
    /// The instruction.
    pub instruction: OutboundInstruction,
    /// The inbox length of the last finalised head when it was assembled: every position below
    /// it was final before the instruction existed, so it cannot hold its execution.
    pub from_seq: u64,
    /// Whether the adapter acknowledged it durably (`REQ-SO-6`).
    pub acknowledged: bool,
    /// What the adapter recorded about signing it: the transaction's key, nonce and last valid
    /// height, or that it never will sign it. Recorded once, after the acknowledgement.
    pub signing: Option<SigningRecord>,
    /// What a finalised head showed about the signed transaction's nonce. Decided once.
    pub nonce: Option<NonceFact>,
}

/// One write to the settlement store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettlementWrite {
    /// Set the deployment's settlement configuration. Once.
    Configure(SettlementConfig),
    /// Record the adapter's support declaration, replacing the last one.
    Declare(SupportDeclaration),
    /// Ingest a record from the authoritative source, associated with `op` if any (`INV-12`).
    Ingest {
        /// The observation.
        observation: Observation,
        /// The operation it is evidence for.
        op: Option<OperationId>,
        /// Whether the record is also indexed by its inbox position (`INGESTED_AT`), for a
        /// re-scan to read: by the code of a generation before
        /// [`super::CHAINED_SETTLEMENT`] only, which takes re-scans (`T-043-06`).
        by_position: bool,
    },
    /// Keep a record from a corroborating source (`REQ-IN-6`). It never writes to the book.
    Corroborate(Observation),
    /// Mark an ingested record final.
    MarkFinal(Coordinate),
    /// Mark an ingested record orphaned.
    MarkOrphaned(Coordinate),
    /// Make `coordinate` the record an operation's `Observed` state rests on, or clear it.
    Bind {
        /// The operation.
        op: OperationId,
        /// Its record, or none.
        coordinate: Option<Coordinate>,
    },
    /// Put an instruction in the outbox.
    Instruct {
        /// The instruction.
        instruction: OutboundInstruction,
        /// See [`Instructed::from_seq`].
        from_seq: u64,
    },
    /// Record the adapter's durable acknowledgement of an instruction.
    Acknowledge(OperationId),
    /// Record what the adapter did about signing an instruction. Once, after the acknowledgement.
    Sign {
        /// The operation.
        op: OperationId,
        /// The record.
        record: SigningRecord,
    },
    /// Record what a finalised head showed about a signed transaction's nonce. Once.
    Nonce {
        /// The operation.
        op: OperationId,
        /// The fact.
        fact: NonceFact,
    },
    /// Raise the finalised head.
    Head(FinalisedHead),
    /// Record the start of a new sequence of the layer's record (amendment 0041): refused unless
    /// its number is strictly later than the one the store holds. With `head`, the finalised head
    /// becomes it (refused unless it is the sequence's start), even below the one held: the one
    /// write that moves the head back. With or without `head`, the untagged gap end and every gap
    /// end of an earlier sequence are cleared, and those of this sequence or a later one kept
    /// (ruling q28, made exact).
    Epoch {
        /// The sequence.
        epoch: SettlementEpoch,
        /// The head it starts at, or none to keep the head (one at a later block is already the
        /// new sequence's).
        head: Option<FinalisedHead>,
    },
    /// Record a skipped range.
    Gap(Gap),
    /// Empty the gaps leaf (`T-043-08`): the gaps moved to the gap end and the audit trail in the
    /// same batch, at the switch to [`super::CHAINED_SETTLEMENT`].
    DropGaps,
    /// Take the first this many incidents out of the incidents leaf (`T-043-08`): moved to the
    /// audit trail in the same batch. Refused if the leaf holds fewer.
    DropIncidents(u32),
    /// Raise the end of the highest gap to at least this position (`T-043-07`): the one number
    /// the book keeps for gaps from [`super::CHAINED_SETTLEMENT`] on, each gap itself going to
    /// the audit trail. It never goes down but at a restart ([`SettlementWrite::Epoch`]).
    GapEnd(u64),
    /// Raise the end of the highest gap of `sequence` to at least `to` (amendment 0041, decision 6
    /// made exact): a gap the adapter named the sequence of. It never goes down but at a start.
    SequenceGapEnd {
        /// The sequence the gap is of.
        sequence: u32,
        /// The first position after it.
        to: u64,
    },
    /// Raise an incident.
    Incident(Incident),
    /// Record the settlement adapter's status declaration, replacing the last (`F-016`).
    Status(AdapterStatus),
    /// Record the adapter's attestation of a location, replacing the last (`DEC-20`).
    Attest(Attested),
    /// Put a binding of an asset in the call outbox (`REQ-AS-8`). Refused while another binding of
    /// the asset is pending, or once one is bound.
    AssetBind(AssetBind),
    /// Decide a pending binding: bound or refused, on the layer's final record at `coordinate`.
    /// Refused unless it is pending.
    AssetBindOutcome {
        /// The asset.
        asset: AssetId,
        /// Bound or refused.
        status: BindStatus,
        /// The final record.
        coordinate: Coordinate,
    },
    /// Record a head the adapter reported at a later block than the one held, with a lower
    /// position, which the book did not take (amendment 0041's guard, `T-043-17`), or clear it.
    Reported(Option<FinalisedHead>),
    /// Advance the positions' tally (`REQ-PR-5`) over every inbox position below `through`, all
    /// final: `counted` are the operations with a final deposit or withdrawal record in the
    /// positions passed, and `held` the tally afterwards. Refused if it would go back or count an
    /// operation twice.
    Tally {
        /// The new cursor.
        through: u64,
        /// The operations counted in the positions passed.
        counted: Vec<OperationId>,
        /// The book's record of value held on the layer, per asset, below `through`.
        held: BTreeMap<AssetId, i128>,
    },
}

/// Why the settlement store refused a write. The whole batch is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementRefusal {
    /// The configuration is already set.
    Configured,
    /// The coordinate was ingested before (`INV-12`).
    Duplicate,
    /// No such ingested record, instruction or operation.
    Missing,
    /// Another record is already final at this position, or the record is orphaned.
    PositionTaken,
    /// The record is already final.
    AlreadyFinal,
    /// The operation already has an instruction.
    Instructed,
    /// The head would go back.
    Backwards,
    /// A gap with no positions.
    EmptyGap,
    /// A signing record for an instruction not yet acknowledged.
    Unacknowledged,
    /// The instruction already has another signing record.
    SigningRecorded,
    /// A nonce fact for an instruction with no signed transaction, or one already decided.
    NonceDecided,
    /// A binding of the asset is pending, or it is bound already.
    BindInFlight,
    /// No binding of the asset is pending.
    NoBindPending,
    /// The tally would go back, or count an operation twice.
    Tally,
    /// A sequence not later than the one the store holds (amendment 0041).
    EpochNotLater,
    /// A start whose head is not the sequence's start (amendment 0041): the head moves back only
    /// to it.
    EpochHeadNotStart,
}

/// How to undo one applied write.
#[derive(Clone, Debug)]
pub(crate) enum SettlementUndo {
    Nothing,
    Unconfigure,
    Declared(Option<SupportDeclaration>),
    Uningest(Coordinate),
    Uncorroborate((SourceId, Coordinate)),
    Standing(Coordinate, Standing, bool),
    Bind(OperationId, Option<Coordinate>),
    Uninstruct(OperationId),
    Unacknowledge(OperationId),
    Unsign(OperationId),
    Undecide(OperationId),
    Head(Option<FinalisedHead>),
    /// The sequence, the head, the untagged gap end and the gap ends by sequence, before a start.
    Epoch(
        Option<SettlementEpoch>,
        Option<FinalisedHead>,
        u64,
        Vec<(u32, u64)>,
    ),
    GapEnds(Vec<(u32, u64)>),
    Reported(Option<FinalisedHead>),
    PopGap,
    GapEnd(u64),
    Gaps(Vec<Gap>),
    Incidents(Vec<Incident>),
    PopIncident,
    PopRescanned((u8, Coordinate)),
    Status(Option<AdapterStatus>),
    Attested(AssetLocation, Option<Attested>),
    AssetBind(AssetId, Option<AssetBind>),
    Tally((u64, BTreeMap<AssetId, i128>), Vec<OperationId>),
}

/// The settlement store's state. See the module documentation for its rules.
#[derive(Clone, Debug, Default)]
pub struct SettlementStore {
    config: Slot<Option<SettlementConfig>, { kind::SETTLEMENT_CONFIG }>,
    support: Slot<Option<SupportDeclaration>, { kind::SETTLEMENT_SUPPORT }>,
    ingested: Leaves<Coordinate, Ingested, { kind::INGESTED }>,
    corroborations: Leaves<(SourceId, Coordinate), Observation, { kind::CORROBORATION }>,
    finals: Leaves<(u32, u64), Coordinate, { kind::FINAL_POSITION }>,
    by_op: Leaves<OperationId, BTreeSet<Coordinate>, { kind::INGESTED_BY_OP }>,
    naming: Leaves<OperationId, BTreeSet<Coordinate>, { kind::NAMING }>,
    bound: Leaves<OperationId, Coordinate, { kind::BOUND }>,
    instructions: Leaves<OperationId, Instructed, { kind::INSTRUCTION }>,
    /// The instructed operations not known to be terminal: what a finalised head may still
    /// dispose of (`crate::state::State` retires one when its operation ends). Derived, never
    /// exported; so a head reads the legs in flight, not every leg ever instructed (`F-005`).
    live: Leaves<OperationId, (), { kind::LIVE_INSTRUCTION }>,
    /// The instructions signed whose nonce no head has decided yet: what a finalised head reads
    /// the nonces for. Derived, never exported (`F-005`).
    awaiting: Leaves<OperationId, (), { kind::AWAITING_NONCE }>,
    head: Slot<Option<FinalisedHead>, { kind::FINALISED_HEAD }>,
    /// The gaps recorded by the generations before [`super::CHAINED_SETTLEMENT`]. From it on a gap
    /// goes to the audit trail and raises `gap_end` instead (`T-043-07`).
    gaps: Slot<Vec<Gap>, { kind::GAPS }>,
    /// The incidents recorded by the generations before [`super::CHAINED_SETTLEMENT`]. From it on
    /// an incident goes to the audit trail only (`T-043-07`, ruling a40-q2).
    incidents: Slot<Vec<Incident>, { kind::SETTLEMENT_INCIDENTS }>,
    /// The end of the highest gap recorded from [`super::CHAINED_SETTLEMENT`] on, 0 for none
    /// (amendment 0040, ruling a40c-gaps-watermark): `proof` cannot show a range that starts
    /// below it complete. Absent while 0, so a book that records no gap holds no leaf for it.
    gap_end: Slot<u64, { kind::GAP_END }>,
    /// The end of the highest gap of each sequence the adapter named with it (amendment 0041,
    /// decision 6 made exact): `(sequence, end)`, in sequence order, one per sequence. From
    /// [`super::SETTLEMENT_EPOCHS`] on; absent while empty. [`SettlementStore::gap_end`] is the
    /// highest over it and the untagged `gap_end`, which counts as the sequence the book holds.
    gap_ends: Slot<Vec<(u32, u64)>, { kind::GAP_END_BY_SEQUENCE }>,
    /// The last sequence of the layer's record whose start the book took (amendment 0041), from
    /// [`super::SETTLEMENT_EPOCHS`] on. Absent until one is taken.
    epoch: Slot<Option<SettlementEpoch>, { kind::SETTLEMENT_EPOCH }>,
    /// The first head the adapter reported at a later block with a lower position that the book
    /// did not take, since the last head it took (amendment 0041's guard, `T-043-17`): while set,
    /// `withdraw` and `submit_outbound` are refused. Absent otherwise.
    reported: Slot<Option<FinalisedHead>, { kind::HEAD_REPORTED }>,
    status: Slot<Option<AdapterStatus>, { kind::ADAPTER_STATUS }>,
    /// The latest attestation of each location (`DEC-20`).
    attestations: Leaves<AssetLocation, Attested, { kind::ASSET_ATTESTATION }>,
    /// Each asset's latest binding (`REQ-AS-8`): pending, bound or refused.
    binds: Leaves<AssetId, AssetBind, { kind::ASSET_BIND }>,
    /// The asset of each binding in `binds`, by its call reference: how the layer's record of a
    /// binding, which names the reference only, finds it by key (finding F18's class, `F-032`).
    /// Derived, never exported.
    bind_refs: Leaves<OperationId, AssetId, { kind::ASSET_BIND_REF }>,
    /// The digests ingested at each inbox position: how a re-scan reads its range by position
    /// rather than every record ingested (finding F18's class). Derived, never exported.
    at_position: Leaves<(u32, u64), BTreeSet<[u8; 32]>, { kind::INGESTED_AT }>,
    /// The positions' tally (`REQ-PR-5`): the cursor `k`, every inbox position below it final,
    /// and the book's record of value held on the layer as of `k`, per asset. A comparison
    /// starts from it and reads only the positions between its cut and `k`, so it does not read
    /// the inbox's whole history (finding F18's class). Exported.
    tally: Slot<(u64, BTreeMap<AssetId, i128>), { kind::POSITION_TALLY }>,
    /// The operations the tally counts: those with a final deposit or withdrawal record below its
    /// cursor. A settling posting of one of them made later is added to the tally as it is
    /// written (`crate::state::State`). Derived, never exported.
    counted: Leaves<OperationId, (), { kind::POSITION_COUNTED }>,
    /// The re-scan differences recorded in `incidents`, by [`rescan_key`]: how a re-scan that
    /// finds a difference again knows it is recorded without reading every incident (`REQ-IN-5`;
    /// finding F18's class: before, each re-scan appended its differences again, and the one
    /// leaf holding every incident grew with every re-scan until the Agent's heap ran out).
    /// Derived, never exported.
    rescanned: Leaves<(u8, Coordinate), (), { kind::RESCAN_RECORDED }>,
}

/// The key of a re-scan difference in the index of those recorded, on instance `instance`;
/// `None` for an incident of another kind. A `Missing` record is keyed by the coordinate the
/// layer holds, an `Unknown` one by the coordinate aledgent holds.
pub(crate) fn rescan_key(instance: u32, i: &Incident) -> Option<(u8, Coordinate)> {
    match i {
        Incident::Missing { seq, digest } => Some((
            0,
            Coordinate {
                instance,
                seq: *seq,
                digest: *digest,
            },
        )),
        Incident::Unknown { coordinate } => Some((1, *coordinate)),
        _ => None,
    }
}

/// Take `c` out of `op`'s set in an index of coordinates by operation, and the set once empty.
fn unindex<const K: u8>(
    index: &mut Leaves<OperationId, BTreeSet<Coordinate>, K>,
    op: Option<OperationId>,
    c: &Coordinate,
) {
    if let Some(op) = op {
        index.remove_from(&op, c);
    }
}

impl SettlementStore {
    /// An empty store.
    pub fn new() -> SettlementStore {
        SettlementStore::default()
    }

    pub(crate) fn undo(&mut self, u: SettlementUndo) {
        match u {
            SettlementUndo::Nothing => {}
            SettlementUndo::Unconfigure => {
                self.config.set(None);
            }
            SettlementUndo::Declared(d) => {
                self.support.set(d);
            }
            SettlementUndo::Uningest(c) => {
                if let Some(i) = self.ingested.remove(&c) {
                    unindex(&mut self.by_op, i.op, &c);
                    unindex(&mut self.naming, i.observation.operation_ref, &c);
                    self.at_position
                        .remove_from(&(c.instance, c.seq), &c.digest);
                }
            }
            SettlementUndo::Uncorroborate(k) => {
                self.corroborations.remove(&k);
            }
            SettlementUndo::Standing(c, before, was_final) => {
                if let Some(i) = self.ingested.get_mut(&c) {
                    i.standing = before;
                }
                if was_final {
                    self.finals.remove(&c.position());
                }
            }
            SettlementUndo::Bind(op, before) => match before {
                Some(c) => {
                    self.bound.insert(op, c);
                }
                None => {
                    self.bound.remove(&op);
                }
            },
            SettlementUndo::Uninstruct(op) => {
                self.instructions.remove(&op);
                self.live.remove(&op);
            }
            SettlementUndo::Unacknowledge(op) => {
                if let Some(i) = self.instructions.get_mut(&op) {
                    i.acknowledged = false;
                }
            }
            SettlementUndo::Unsign(op) => {
                if let Some(i) = self.instructions.get_mut(&op) {
                    i.signing = None;
                }
                self.awaiting.remove(&op);
            }
            SettlementUndo::Undecide(op) => {
                if let Some(i) = self.instructions.get_mut(&op) {
                    i.nonce = None;
                }
                self.awaiting.insert(op, ());
            }
            SettlementUndo::Head(h) => {
                self.head.set(h);
            }
            SettlementUndo::Epoch(e, h, gap_end, gap_ends) => {
                self.epoch.set(e);
                self.head.set(h);
                self.gap_end.set(gap_end);
                self.gap_ends.set(gap_ends);
            }
            SettlementUndo::GapEnds(before) => {
                self.gap_ends.set(before);
            }
            SettlementUndo::Reported(r) => {
                self.reported.set(r);
            }
            SettlementUndo::PopGap => {
                self.gaps.get_mut().pop();
            }
            SettlementUndo::GapEnd(before) => {
                self.gap_end.set(before);
            }
            SettlementUndo::Gaps(gaps) => {
                self.gaps.set(gaps);
            }
            SettlementUndo::Incidents(incidents) => {
                self.incidents.set(incidents);
            }
            SettlementUndo::PopIncident => {
                self.incidents.get_mut().pop();
            }
            SettlementUndo::PopRescanned(k) => {
                self.incidents.get_mut().pop();
                self.rescanned.remove(&k);
            }
            SettlementUndo::Status(s) => {
                self.status.set(s);
            }
            SettlementUndo::Attested(l, before) => match before {
                Some(a) => {
                    self.attestations.insert(l, a);
                }
                None => {
                    self.attestations.remove(&l);
                }
            },
            SettlementUndo::AssetBind(a, before) => {
                self.set_bind(a, before);
            }
            SettlementUndo::Tally(before, counted) => {
                for op in &counted {
                    self.counted.remove(op);
                }
                self.tally.set(before);
            }
        }
    }

    /// Put `bind` as `asset`'s latest binding, or none, keeping the index by reference.
    fn set_bind(&mut self, asset: AssetId, bind: Option<AssetBind>) -> Option<AssetBind> {
        let before = match bind {
            Some(b) => self.binds.insert(asset, b),
            None => self.binds.remove(&asset),
        };
        if let Some(old) = before {
            self.bind_refs.remove(&old.reference);
        }
        if let Some(b) = bind {
            self.bind_refs.insert(b.reference, asset);
        }
        before
    }

    /// Add `deltas` to the tally: a settling posting of an operation it counts, written after the
    /// tally passed its record. The undo, if anything changed.
    pub(crate) fn tally_posting(&mut self, deltas: &[(AssetId, i128)]) -> Option<SettlementUndo> {
        if deltas.is_empty() {
            return None;
        }
        let before = self.tally.get().clone();
        let tally = self.tally.get_mut();
        for (asset, d) in deltas {
            *tally.1.entry(*asset).or_default() += d;
        }
        Some(SettlementUndo::Tally(before, Vec::new()))
    }

    pub(crate) fn apply(
        &mut self,
        w: &SettlementWrite,
    ) -> Result<SettlementUndo, SettlementRefusal> {
        match w {
            SettlementWrite::Configure(c) => {
                if self.config.get().is_some() {
                    return Err(SettlementRefusal::Configured);
                }
                self.config.set(Some(*c));
                Ok(SettlementUndo::Unconfigure)
            }
            SettlementWrite::Declare(d) => {
                let before = self.support.set(Some(d.clone()));
                Ok(SettlementUndo::Declared(before))
            }
            SettlementWrite::Ingest {
                observation,
                op,
                by_position,
            } => {
                let c = observation.coordinate;
                if self.ingested.contains_key(&c) {
                    return Err(SettlementRefusal::Duplicate);
                }
                self.ingested.insert(
                    c,
                    Ingested {
                        observation: observation.clone(),
                        op: *op,
                        standing: Standing::Provisional,
                    },
                );
                if let Some(op) = op {
                    self.by_op.entry_or_default(*op).insert(c);
                }
                if let Some(named) = observation.operation_ref {
                    self.naming.entry_or_default(named).insert(c);
                }
                if *by_position {
                    self.at_position
                        .entry_or_default((c.instance, c.seq))
                        .insert(c.digest);
                }
                Ok(SettlementUndo::Uningest(c))
            }
            SettlementWrite::Corroborate(o) => {
                let k = (o.source, o.coordinate);
                if self.corroborations.contains_key(&k) {
                    return Err(SettlementRefusal::Duplicate);
                }
                self.corroborations.insert(k, o.clone());
                Ok(SettlementUndo::Uncorroborate(k))
            }
            SettlementWrite::MarkFinal(c) => {
                let i = self.ingested.get(c).ok_or(SettlementRefusal::Missing)?;
                match i.standing {
                    Standing::Final => return Ok(SettlementUndo::Nothing),
                    Standing::Orphaned => return Err(SettlementRefusal::PositionTaken),
                    Standing::Provisional => {}
                }
                if self.finals.contains_key(&c.position()) {
                    return Err(SettlementRefusal::PositionTaken);
                }
                self.finals.insert(c.position(), *c);
                if let Some(i) = self.ingested.get_mut(c) {
                    i.standing = Standing::Final;
                }
                Ok(SettlementUndo::Standing(*c, Standing::Provisional, true))
            }
            SettlementWrite::MarkOrphaned(c) => {
                let i = self.ingested.get_mut(c).ok_or(SettlementRefusal::Missing)?;
                match i.standing {
                    Standing::Orphaned => Ok(SettlementUndo::Nothing),
                    Standing::Final => Err(SettlementRefusal::AlreadyFinal),
                    Standing::Provisional => {
                        i.standing = Standing::Orphaned;
                        Ok(SettlementUndo::Standing(*c, Standing::Provisional, false))
                    }
                }
            }
            SettlementWrite::Bind { op, coordinate } => {
                if let Some(c) = coordinate {
                    match self.ingested.get(c) {
                        Some(i) if i.op == Some(*op) => {}
                        _ => return Err(SettlementRefusal::Missing),
                    }
                }
                let before = match coordinate {
                    Some(c) => self.bound.insert(*op, *c),
                    None => self.bound.remove(op),
                };
                Ok(SettlementUndo::Bind(*op, before))
            }
            SettlementWrite::Instruct {
                instruction,
                from_seq,
            } => {
                let op = instruction.operation_ref();
                if self.instructions.contains_key(&op) {
                    return Err(SettlementRefusal::Instructed);
                }
                self.instructions.insert(
                    op,
                    Instructed {
                        instruction: *instruction,
                        from_seq: *from_seq,
                        acknowledged: false,
                        signing: None,
                        nonce: None,
                    },
                );
                self.live.insert(op, ());
                Ok(SettlementUndo::Uninstruct(op))
            }
            SettlementWrite::Acknowledge(op) => {
                let i = self
                    .instructions
                    .get_mut(op)
                    .ok_or(SettlementRefusal::Missing)?;
                if i.acknowledged {
                    return Ok(SettlementUndo::Nothing);
                }
                i.acknowledged = true;
                Ok(SettlementUndo::Unacknowledge(*op))
            }
            SettlementWrite::Sign { op, record } => {
                let i = self
                    .instructions
                    .get_mut(op)
                    .ok_or(SettlementRefusal::Missing)?;
                if !i.acknowledged {
                    return Err(SettlementRefusal::Unacknowledged);
                }
                match i.signing {
                    Some(r) if r == *record => Ok(SettlementUndo::Nothing),
                    Some(_) => Err(SettlementRefusal::SigningRecorded),
                    None => {
                        i.signing = Some(*record);
                        if matches!(record, SigningRecord::Signed(_)) {
                            self.awaiting.insert(*op, ());
                        }
                        Ok(SettlementUndo::Unsign(*op))
                    }
                }
            }
            SettlementWrite::Nonce { op, fact } => {
                let i = self
                    .instructions
                    .get_mut(op)
                    .ok_or(SettlementRefusal::Missing)?;
                if !matches!(i.signing, Some(SigningRecord::Signed(_))) || i.nonce.is_some() {
                    return Err(SettlementRefusal::NonceDecided);
                }
                i.nonce = Some(*fact);
                self.awaiting.remove(op);
                Ok(SettlementUndo::Undecide(*op))
            }
            SettlementWrite::Head(h) => {
                if let Some(before) = *self.head.get() {
                    if h.height < before.height || h.next_seq < before.next_seq {
                        return Err(SettlementRefusal::Backwards);
                    }
                }
                let before = self.head.set(Some(*h));
                Ok(SettlementUndo::Head(before))
            }
            SettlementWrite::Reported(r) => {
                let before = self.reported.set(*r);
                Ok(SettlementUndo::Reported(before))
            }
            SettlementWrite::Epoch { epoch, head } => {
                if self
                    .epoch
                    .get()
                    .is_some_and(|held| epoch.epoch <= held.epoch)
                {
                    return Err(SettlementRefusal::EpochNotLater);
                }
                if head.is_some_and(|h| h != epoch.start()) {
                    return Err(SettlementRefusal::EpochHeadNotStart);
                }
                let before = *self.head.get();
                if let Some(h) = head {
                    self.head.set(Some(*h));
                }
                // Decision 6, made exact (ruling q28, Pablo 28 Oct): the end of every gap of an
                // earlier sequence is cleared, whether or not the head is kept; that of the started
                // sequence or a later one is kept. The untagged end is a gap reported without its
                // sequence (the one the book held, earlier than this) or moved by `T-043-08`.
                let gap_end = self.gap_end.set(0);
                let kept: Vec<(u32, u64)> = self
                    .gap_ends
                    .get()
                    .iter()
                    .copied()
                    .filter(|(sequence, _)| *sequence >= epoch.epoch)
                    .collect();
                let gap_ends = self.gap_ends.set(kept);
                let held = self.epoch.set(Some(*epoch));
                Ok(SettlementUndo::Epoch(held, before, gap_end, gap_ends))
            }
            SettlementWrite::Gap(g) => {
                if g.to <= g.from {
                    return Err(SettlementRefusal::EmptyGap);
                }
                self.gaps.get_mut().push(*g);
                Ok(SettlementUndo::PopGap)
            }
            SettlementWrite::DropGaps => Ok(SettlementUndo::Gaps(self.gaps.set(Vec::new()))),
            SettlementWrite::DropIncidents(n) => {
                let n = *n as usize;
                if n > self.incidents.get().len() {
                    return Err(SettlementRefusal::Missing);
                }
                let rest = self.incidents.get()[n..].to_vec();
                Ok(SettlementUndo::Incidents(self.incidents.set(rest)))
            }
            SettlementWrite::GapEnd(to) => {
                let before = *self.gap_end.get();
                if *to <= before {
                    return Ok(SettlementUndo::Nothing);
                }
                self.gap_end.set(*to);
                Ok(SettlementUndo::GapEnd(before))
            }
            SettlementWrite::SequenceGapEnd { sequence, to } => {
                let mut ends = self.gap_ends.get().clone();
                match ends.binary_search_by_key(sequence, |(s, _)| *s) {
                    Ok(i) if ends[i].1 >= *to => return Ok(SettlementUndo::Nothing),
                    Ok(i) => ends[i].1 = *to,
                    Err(i) => ends.insert(i, (*sequence, *to)),
                }
                Ok(SettlementUndo::GapEnds(self.gap_ends.set(ends)))
            }
            SettlementWrite::Incident(i) => {
                let key = self
                    .config
                    .get()
                    .as_ref()
                    .and_then(|c| rescan_key(c.instance, i));
                if let Some(k) = key
                    && self.rescanned.contains_key(&k)
                {
                    return Err(SettlementRefusal::Duplicate);
                }
                self.incidents.get_mut().push(*i);
                Ok(match key {
                    Some(k) => {
                        self.rescanned.insert(k, ());
                        SettlementUndo::PopRescanned(k)
                    }
                    None => SettlementUndo::PopIncident,
                })
            }
            SettlementWrite::Status(s) => {
                let before = self.status.set(Some(*s));
                Ok(SettlementUndo::Status(before))
            }
            SettlementWrite::Attest(a) => {
                let l = a.attestation.location;
                let before = self.attestations.insert(l, a.clone());
                Ok(SettlementUndo::Attested(l, before))
            }
            SettlementWrite::AssetBind(b) => {
                if self
                    .binds
                    .get(&b.asset)
                    .is_some_and(|x| x.status != BindStatus::Refused)
                    || b.status != BindStatus::Pending
                {
                    return Err(SettlementRefusal::BindInFlight);
                }
                let before = self.set_bind(b.asset, Some(*b));
                Ok(SettlementUndo::AssetBind(b.asset, before))
            }
            SettlementWrite::AssetBindOutcome {
                asset,
                status,
                coordinate,
            } => {
                let Some(b) = self
                    .binds
                    .get(asset)
                    .copied()
                    .filter(|b| b.status == BindStatus::Pending)
                else {
                    return Err(SettlementRefusal::NoBindPending);
                };
                if *status == BindStatus::Pending {
                    return Err(SettlementRefusal::NoBindPending);
                }
                let mut after = b;
                after.status = *status;
                after.coordinate = Some(*coordinate);
                self.set_bind(*asset, Some(after));
                Ok(SettlementUndo::AssetBind(*asset, Some(b)))
            }
            SettlementWrite::Tally {
                through,
                counted,
                held,
            } => {
                let before = self.tally.get().clone();
                if *through < before.0
                    || counted.iter().any(|op| self.counted.contains_key(op))
                    || counted.windows(2).any(|w| w[0] >= w[1])
                {
                    return Err(SettlementRefusal::Tally);
                }
                for op in counted {
                    self.counted.insert(*op, ());
                }
                self.tally.set((*through, held.clone()));
                Ok(SettlementUndo::Tally(before, counted.clone()))
            }
        }
    }

    /// The configuration, once set.
    pub fn config(&self) -> Option<&SettlementConfig> {
        self.config.get().as_ref()
    }

    /// The adapter's last support declaration.
    pub fn support(&self) -> Option<&SupportDeclaration> {
        self.support.get().as_ref()
    }

    /// The adapter's last status declaration (`F-016`), if it ever declared one.
    pub fn status(&self) -> Option<&AdapterStatus> {
        self.status.get().as_ref()
    }

    /// The latest attestation of `location` (`DEC-20`), if the adapter ever attested it.
    pub fn attestation(&self, location: &AssetLocation) -> Option<&Attested> {
        self.attestations.get(location)
    }

    /// Every location's latest attestation, by location.
    pub fn attestations(&self) -> impl Iterator<Item = &Attested> {
        self.attestations.values()
    }

    /// An asset's latest binding, pending or decided (`REQ-AS-8`).
    pub fn asset_bind(&self, asset: &AssetId) -> Option<&AssetBind> {
        self.binds.get(asset)
    }

    /// Every asset's latest binding, by asset.
    pub fn asset_binds(&self) -> impl Iterator<Item = &AssetBind> {
        self.binds.values()
    }

    /// The binding a call reference names, if one does: two reads by key (finding F18's class;
    /// before, every asset's binding was read to find it).
    pub fn asset_bind_by_reference(&self, reference: &OperationId) -> Option<&AssetBind> {
        let asset = self.bind_refs.get(reference)?;
        self.binds.get(asset).filter(|b| b.reference == *reference)
    }

    /// The records ingested at inbox position `(instance, seq)`, read by position.
    pub fn ingested_at(&self, instance: u32, seq: u64) -> impl Iterator<Item = &Ingested> {
        self.at_position
            .get(&(instance, seq))
            .into_iter()
            .flatten()
            .filter_map(move |digest| {
                self.ingested.get(&Coordinate {
                    instance,
                    seq,
                    digest: *digest,
                })
            })
    }

    /// The positions' tally: its cursor, and the value held on the layer as of it, per asset.
    pub fn tally(&self) -> (u64, &BTreeMap<AssetId, i128>) {
        let t = self.tally.get();
        (t.0, &t.1)
    }

    /// Whether the tally counts `op`.
    pub fn is_counted(&self, op: &OperationId) -> bool {
        self.counted.contains_key(op)
    }

    /// Whether `op`'s instruction is live: its operation not known to be terminal. A read by key.
    pub fn is_live(&self, op: &OperationId) -> bool {
        self.live.contains_key(op)
    }

    /// Whether `op`'s instruction is signed and awaits its nonce fact. A read by key.
    pub fn is_awaiting(&self, op: &OperationId) -> bool {
        self.awaiting.contains_key(op)
    }

    /// The operations whose instruction is live, chosen without witnessing the index: for a
    /// choice the replica records in its call, never inside one (finding F18's class). Fails
    /// closed on a partial store.
    pub(crate) fn live_unrecorded(&self) -> impl Iterator<Item = &OperationId> {
        self.live.keys_unrecorded()
    }

    /// The operations awaiting a nonce fact, chosen as [`SettlementStore::live_unrecorded`] is.
    pub(crate) fn awaiting_unrecorded(&self) -> impl Iterator<Item = &OperationId> {
        self.awaiting.keys_unrecorded()
    }

    /// Derive the index by reference, by position, the operations the tally counts and the
    /// re-scan differences recorded again after an import: every binding's reference, every
    /// ingested record's position, every operation with a final deposit or withdrawal record
    /// below the tally's cursor, and every `Missing` and `Unknown` incident.
    pub(crate) fn rebuild_indexes(&mut self) {
        let binds: Vec<AssetBind> = self.binds.values().copied().collect();
        for b in binds {
            self.bind_refs.insert(b.reference, b.asset);
        }
        let coordinates: Vec<Coordinate> = self.ingested.keys().copied().collect();
        for c in coordinates {
            self.at_position
                .entry_or_default((c.instance, c.seq))
                .insert(c.digest);
        }
        for op in self.counted_below(self.tally.get().0) {
            self.counted.insert(op, ());
        }
        if let Some(instance) = self.config().map(|c| c.instance) {
            let keys: Vec<(u8, Coordinate)> = self
                .incidents
                .get()
                .iter()
                .filter_map(|i| rescan_key(instance, i))
                .collect();
            for k in keys {
                self.rescanned.insert(k, ());
            }
        }
    }

    /// The operations with a final deposit or withdrawal record below inbox position `k` of the
    /// configured instance. A scan of every final position: for an import, never inside a
    /// recorded call.
    pub(crate) fn counted_below(&self, k: u64) -> BTreeSet<OperationId> {
        let Some(instance) = self.config().map(|c| c.instance) else {
            return BTreeSet::new();
        };
        (0..k)
            .filter_map(|seq| self.final_at(instance, seq))
            .filter_map(|c| self.ingested(c))
            .filter(|i| counts(i))
            .filter_map(|i| i.op)
            .collect()
    }

    /// An ingested record.
    pub fn ingested(&self, c: &Coordinate) -> Option<&Ingested> {
        self.ingested.get(c)
    }

    /// Every ingested record, in coordinate order.
    pub fn all_ingested(&self) -> impl Iterator<Item = &Ingested> {
        self.ingested.values()
    }

    /// Every corroborating record.
    pub fn corroborations(&self) -> impl Iterator<Item = &Observation> {
        self.corroborations.values()
    }

    /// The record final at `(instance, seq)`, if any.
    pub fn final_at(&self, instance: u32, seq: u64) -> Option<&Coordinate> {
        self.finals.get(&(instance, seq))
    }

    /// How many positions of `instance` in `[from, to)` hold a final record.
    pub fn finals_in(&self, instance: u32, from: u64, to: u64) -> u64 {
        if to <= from {
            return 0;
        }
        self.finals.range((instance, from)..(instance, to)).count() as u64
    }

    /// The records associated with `op`.
    pub fn of_op(&self, op: &OperationId) -> impl Iterator<Item = &Ingested> {
        self.by_op
            .get(op)
            .into_iter()
            .flatten()
            .filter_map(|c| self.ingested.get(c))
    }

    /// Every ingested record that carries `op` as its operation reference, associated or not.
    pub fn naming(&self, op: &OperationId) -> impl Iterator<Item = &Ingested> {
        self.naming
            .get(op)
            .into_iter()
            .flatten()
            .filter_map(|c| self.ingested.get(c))
    }

    /// The record an operation's `Observed` state rests on.
    pub fn bound(&self, op: &OperationId) -> Option<&Coordinate> {
        self.bound.get(op)
    }

    /// An operation's instruction.
    pub fn instruction(&self, op: &OperationId) -> Option<&Instructed> {
        self.instructions.get(op)
    }

    /// Every instruction, acknowledged or not, by operation.
    pub fn instructions(&self) -> impl Iterator<Item = &Instructed> {
        self.instructions.values()
    }

    /// The instructions whose operation is not known to be terminal, in operation order: every
    /// leg a finalised head may still dispose of, and perhaps some it may not.
    pub fn live_instructions(&self) -> impl Iterator<Item = &Instructed> {
        self.live.keys().filter_map(|op| self.instructions.get(op))
    }

    /// The instructions signed whose nonce no head has decided, in operation order: exactly
    /// the instructions with a signing and no nonce fact, read from their index.
    pub fn awaiting_nonce(&self) -> impl Iterator<Item = &Instructed> {
        self.awaiting
            .keys()
            .filter_map(|op| self.instructions.get(op))
    }

    /// Derive the instructions awaiting a nonce again after an import.
    pub(crate) fn rebuild_awaiting(&mut self) {
        let ops: Vec<OperationId> = self
            .instructions
            .iter()
            .filter(|(_, i)| {
                i.nonce.is_none() && matches!(i.signing, Some(SigningRecord::Signed(_)))
            })
            .map(|(op, _)| *op)
            .collect();
        for op in ops {
            self.awaiting.insert(op, ());
        }
    }

    /// Whether `source` corroborated the observation at `coordinate`.
    pub fn corroborated(&self, source: &SourceId, coordinate: &Coordinate) -> bool {
        self.corroborations.contains_key(&(*source, *coordinate))
    }

    /// `op` ended: its instruction, if any, is no longer live. Whether it was.
    pub(crate) fn retire(&mut self, op: &OperationId) -> bool {
        self.live.remove(op).is_some()
    }

    /// Take a [`SettlementStore::retire`] back.
    pub(crate) fn unretire(&mut self, op: OperationId) {
        self.live.insert(op, ());
    }

    /// Derive the live instructions again after an import: every instruction whose operation
    /// `ended` does not say is terminal.
    pub(crate) fn rebuild_live(&mut self, ended: impl Fn(&OperationId) -> bool) {
        let ops: Vec<OperationId> = self.instructions.keys().copied().collect();
        for op in ops {
            if !ended(&op) {
                self.live.insert(op, ());
            }
        }
    }

    /// Instructions not yet acknowledged: what the outbox delivers, again after a restart
    /// (`REQ-SO-7`).
    pub fn pending(&self) -> impl Iterator<Item = &OutboundInstruction> {
        self.instructions
            .values()
            .filter(|i| !i.acknowledged)
            .map(|i| &i.instruction)
    }

    /// The last finalised head.
    pub fn head(&self) -> Option<FinalisedHead> {
        *self.head.get()
    }

    /// The last sequence of the layer's record whose start the book took (amendment 0041).
    pub fn epoch(&self) -> Option<SettlementEpoch> {
        *self.epoch.get()
    }

    /// The head the adapter reported and the book did not take, if any (amendment 0041's guard).
    pub fn reported(&self) -> Option<FinalisedHead> {
        *self.reported.get()
    }

    /// Whether any outbound instruction is not yet decided (amendment 0041): a read of the
    /// instructions in flight, whole.
    pub fn any_live(&self) -> Option<OperationId> {
        self.live.first_key_value().map(|(op, _)| *op)
    }

    /// The final records of `instance` at positions at or above `from`, whole, in position order.
    pub fn finals_from(&self, instance: u32, from: u64) -> impl Iterator<Item = &Coordinate> {
        self.finals
            .range((instance, from)..=(instance, u64::MAX))
            .map(|(_, c)| c)
    }

    /// Every recorded gap.
    pub fn gaps(&self) -> &[Gap] {
        self.gaps.get()
    }

    /// Whether a recorded gap covers any position of `[from, to)`: one in the list the
    /// generations before [`super::CHAINED_SETTLEMENT`] kept.
    pub fn gap_within(&self, from: u64, to: u64) -> bool {
        self.gaps.get().iter().any(|g| g.from < to && from < g.to)
    }

    /// The end of the highest gap recorded from [`super::CHAINED_SETTLEMENT`] on; 0 for none
    /// (`T-043-07`).
    pub fn gap_end(&self) -> u64 {
        // The highest over the untagged end and every sequence's (kind 34, read every time:
        // `proof` calls this only from `CHAINED_SETTLEMENT` on, where kind 34 is known).
        let ends = self.gap_ends.get();
        ends.iter()
            .map(|(_, end)| *end)
            .fold(*self.gap_end.get(), u64::max)
    }

    /// The end of the highest gap of each sequence the adapter named (amendment 0041).
    pub fn gap_ends(&self) -> &[(u32, u64)] {
        self.gap_ends.get()
    }

    /// The untagged gap end: gaps reported without their sequence, or moved by `T-043-08`.
    pub fn untagged_gap_end(&self) -> u64 {
        *self.gap_end.get()
    }

    /// Whether a range that starts at `from` starts below the end of the highest gap recorded
    /// from the chained generation on (`proof`, amendment 0040). Never less cautious than the
    /// list it replaces: once the head has passed the gap they answer alike, and before that it
    /// may answer that a range is unprovable where the list would not.
    pub fn gap_starts_below(&self, from: u64) -> bool {
        from < self.gap_end()
    }

    /// Every incident, in order.
    pub fn incidents(&self) -> &[Incident] {
        self.incidents.get()
    }

    /// Whether the re-scan difference `i` on instance `instance` is recorded already: a read by
    /// key, never of the incidents whole. `false` for an incident of another kind.
    pub fn rescan_recorded(&self, instance: u32, i: &Incident) -> bool {
        rescan_key(instance, i).is_some_and(|k| self.rescanned.contains_key(&k))
    }

    /// The unassociated bucket: ingested records that relate to no operation, anchors (and
    /// refused anchor calls, `F-027`) aside. It reads every record: for queries only.
    pub fn unassociated(&self) -> impl Iterator<Item = &Ingested> {
        self.ingested.values().filter(|i| unassociated(i))
    }
}

/// Whether `i` is in the unassociated bucket ([`SettlementStore::unassociated`]).
pub fn unassociated(i: &Ingested) -> bool {
    i.op.is_none()
        && !matches!(
            i.observation.kind,
            super::EventKind::Anchored
                | super::EventKind::AnchorRefused
                | super::EventKind::Bound
                | super::EventKind::BindRefused
        )
}

/// Whether `i`, once final, is a record whose operation the positions' tally counts: a deposit or
/// a withdrawal (`REQ-PR-5`).
pub fn counts(i: &Ingested) -> bool {
    i.standing == Standing::Final
        && matches!(
            i.observation.kind,
            super::EventKind::Deposited | super::EventKind::Withdrawn
        )
}

impl LeafHost for SettlementStore {
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>) {
        out.push(&self.config);
        out.push(&self.support);
        out.push(&self.ingested);
        out.push(&self.corroborations);
        out.push(&self.finals);
        out.push(&self.by_op);
        out.push(&self.naming);
        out.push(&self.bound);
        out.push(&self.instructions);
        out.push(&self.live);
        out.push(&self.awaiting);
        out.push(&self.head);
        out.push(&self.gaps);
        out.push(&self.incidents);
        out.push(&self.gap_end);
        out.push(&self.gap_ends);
        out.push(&self.epoch);
        out.push(&self.reported);
        out.push(&self.status);
        out.push(&self.attestations);
        out.push(&self.binds);
        out.push(&self.bind_refs);
        out.push(&self.at_position);
        out.push(&self.tally);
        out.push(&self.counted);
        out.push(&self.rescanned);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.config);
        out.push(&mut self.support);
        out.push(&mut self.ingested);
        out.push(&mut self.corroborations);
        out.push(&mut self.finals);
        out.push(&mut self.by_op);
        out.push(&mut self.naming);
        out.push(&mut self.bound);
        out.push(&mut self.instructions);
        out.push(&mut self.live);
        out.push(&mut self.awaiting);
        out.push(&mut self.head);
        out.push(&mut self.gaps);
        out.push(&mut self.incidents);
        out.push(&mut self.gap_end);
        out.push(&mut self.gap_ends);
        out.push(&mut self.epoch);
        out.push(&mut self.reported);
        out.push(&mut self.status);
        out.push(&mut self.attestations);
        out.push(&mut self.binds);
        out.push(&mut self.bind_refs);
        out.push(&mut self.at_position);
        out.push(&mut self.tally);
        out.push(&mut self.counted);
        out.push(&mut self.rescanned);
    }
}

/// The store's records in an export, and its rebuild from them (`F-014`).
mod canon;
