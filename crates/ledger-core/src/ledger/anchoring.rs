// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Anchoring (`F-017` T-017-01 to T-017-03, T-017-07; `REQ-PR-1`, `REQ-PR-5`, `REQ-PR-7`–`PR-9`,
//! `INV-19`).
//!
//! - **Creation.** [`Ledger::tick`] creates a checkpoint once the interval has passed and new
//!   entries exist (`AC-J1.1`); the operator's `anchor_checkpoint` creates one on demand. Either
//!   way the kernel root and the entry sequence are read in the same committed state, inside one
//!   invocation, and the checkpoint and its instruction commit together (`REQ-RT-1`). With no new
//!   entry, nothing is created; with one in flight, nothing is created either (one anchor in
//!   flight, the pallet's strict order).
//! - **Off the critical path** (`REQ-PR-9`, `AC-J1.4`). No holder command reads or waits on
//!   anything here. A tick whose checkpoint cannot be made still moves its deadlines.
//! - **Finality** (`REQ-PR-8`). The adapter acknowledges the instruction (`Pending →
//!   Submitted`), may report that its extrinsic provably did not execute (`Submitted → Pending`,
//!   the same `(number, commitment)` emitted again — an anchor moves no value, so a second
//!   submission is safe: the pallet takes a replay as a no-op), and reports the `Anchored` record.
//!   Only the canonical check that finds that record final with the same commitment makes the
//!   checkpoint `Final`; another commitment raises an incident and it never becomes final.
//! - **The settlement comparison** (`REQ-PR-5`, `AC-J3.*`). At a cut `k` — the instance's inbox
//!   length at a finalized block — the book's record of value held on the settlement layer is
//!   compared with the layer's position, per asset, to the minor unit.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::operator::digest;
use super::{Accepted, Ctx, Ledger};
use crate::account::Time;
use crate::anchor::{
    AnchorIncident, AnchorOutbox, AnchorWrite, Checkpoint, CheckpointStatus, Comparison,
    PositionCut, PositionRow, commitment_v3,
};
use crate::asset::{AssetId, Minor};
use crate::audit::AuditSubject;
use crate::id::OperationId;
use crate::operator::OperatorOpKind;
use crate::principal::{Caller, resolve, resolve_adapter};
use crate::refusal::Refusal;
use crate::settlement::store::{Ingested, SettlementWrite, counts};
use crate::settlement::{Coordinate, EventKind, Observation};
use crate::state::{Command, Write};

/// The most inbox positions one invocation reads to bring the positions' tally to a comparison's
/// cut, or passes it over on a tick (finding F18's class, `F-032`). Each position is a few leaves:
/// its final record, an operation's record and postings.
pub const MAX_TALLY_PER_INVOCATION: u64 = 16;

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ creation

    /// The checkpoint the current state makes, if one should be created now: `None` with no new
    /// entry since the last checkpoint, or with one in flight. `ERR-SettlementLayerUnavailable`
    /// before the settlement configuration names the instance, which the commitment binds.
    fn next_checkpoint(&self, now: Time) -> Result<Option<Checkpoint>, Refusal> {
        let instance = self
            .state
            .settlement()
            .config()
            .map(|c| c.instance)
            .ok_or(Refusal::SettlementLayerUnavailable)?;
        let store = self.state.anchoring();
        if store.in_flight().is_some() {
            return Ok(None);
        }
        let next = self
            .book
            .store()
            .next_posting_seq()
            .map_err(|_| Refusal::NotAuthorised)?;
        let Some(last) = next.checked_sub(1) else {
            return Ok(None);
        };
        if store.latest().is_some_and(|c| c.entry_sequence >= last) {
            return Ok(None);
        }
        // Read in the same state as `last`: nothing is written between the two reads.
        let kernel_root = self.book.root().map_err(|_| Refusal::NotAuthorised)?;
        let number = store.latest().map_or(1, |c| c.number + 1);
        // `v3` (`T-037-03`): the genesis, the code and the mode, beside the construction.
        let construction = crate::anchor::construction();
        let binding = self.code_binding();
        Ok(Some(Checkpoint {
            number,
            instance,
            entry_sequence: last,
            kernel_root,
            construction,
            binding: Some(binding),
            commitment: commitment_v3(construction, &binding, instance, number, last, &kernel_root),
            created_at: now,
            status: CheckpointStatus::Pending,
            coordinate: None,
            venue: None,
            height: None,
        }))
    }

    /// The cadence (`REQ-PR-1`, `AC-J1.1`), run by [`Ledger::tick`]: a checkpoint once the
    /// interval has passed since the last one was created, if new entries exist. Never fails the
    /// tick (`REQ-PR-9`): anything that stops a checkpoint leaves it for the next tick.
    /// When a tick next has work to do (`DEC-6`, `F-031`, stress finding F2): the earliest
    /// deadline of an open operation, or the time the next checkpoint is due, if one could be
    /// taken. `None` when nothing is scheduled: a tick before then fires nothing and anchors
    /// nothing, so the ticker may skip it. A host reads it after each input; it is not an input.
    pub fn next_due(&self) -> Option<Time> {
        let deadline = self.state.operations().next_deadline();
        let anchor = match self.next_checkpoint(0) {
            Ok(Some(_)) => Some(self.state.anchoring().latest().map_or(0, |c| {
                c.created_at.saturating_add(self.config.anchoring.interval)
            })),
            _ => None,
        };
        // The positions' tally behind a final position: due now.
        let tally = self.tally_behind().then_some(0);
        [deadline, anchor, tally].into_iter().flatten().min()
    }

    pub(super) fn anchor_on_tick(&mut self, now: Time) -> Option<u64> {
        let interval = self.config.anchoring.interval;
        if self
            .state
            .anchoring()
            .latest()
            .is_some_and(|c| now < c.created_at.saturating_add(interval))
        {
            return None;
        }
        // A checkpoint covers the book as the commands before it left it: commit an open batch
        // first, so a batched run anchors what a run one by one anchors (`T-031-19`, `REQ-RT-3`).
        self.book.flush().ok()?;
        let c = self.next_checkpoint(now).ok().flatten()?;
        self.commit(&[Write::Anchor(AnchorWrite::Create(c))]).ok()?;
        Some(c.number)
    }

    /// `anchor_checkpoint` on `CTR-OPS` (`REQ-PR-1` on demand, `REQ-OS-1`): through the operator
    /// frame (kind `AnchorCheckpoint`, a justification, the client key), with an audit record.
    /// Answers the checkpoint it created, or, with no new entry or one in flight, the latest one
    /// and creates nothing. `None` if there is none at all.
    pub fn anchor_checkpoint(
        &mut self,
        ctx: &Ctx,
        justification: &[u8],
    ) -> Result<Accepted<Option<Checkpoint>>, Refusal> {
        let top = self.enter(|| crate::call::Call::AnchorCheckpoint {
            ctx: *ctx,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<Option<Checkpoint>>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("anchor_checkpoint", &p, justification, |_| {});
            let store = self.state.anchoring();
            if let Some(a) = self.replay(ctx, &p, &d, |r| {
                store
                    .on_demand(&r.op)
                    .flatten()
                    .and_then(|n| store.checkpoint(n).copied())
            })? {
                return Ok(a);
            }
            let command = Command::AnchorCheckpoint;
            let (used, j) =
                match self.frame(&p, OperatorOpKind::AnchorCheckpoint, justification, ctx.now) {
                    Ok(x) => x,
                    Err(r) => return Err(self.refused(ctx, &p, command, Vec::new(), r)),
                };
            // As on the tick: an open batch is committed before the checkpoint reads the book.
            if self.book.flush().is_err() {
                return Err(self.refused(ctx, &p, command, Vec::new(), Refusal::NotAuthorised));
            }
            let planned = match self.next_checkpoint(ctx.now) {
                Ok(c) => c,
                Err(r) => return Err(self.refused(ctx, &p, command, Vec::new(), r)),
            };
            let answer = planned.or_else(|| self.state.anchoring().latest().copied());
            let mut writes = Vec::new();
            if let Some(c) = planned {
                writes.push(Write::Anchor(AnchorWrite::Create(c)));
            }
            writes.push(Write::Anchor(AnchorWrite::OnDemand {
                op: ctx.op,
                number: answer.map(|c| c.number),
            }));
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                Vec::new(),
                writes,
                answer,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ------------------------------------------------------------------ the outbox and finality

    /// What the adapter reads of anchoring: the checkpoint in flight and whether it has
    /// acknowledged it, and whether a settlement comparison is due.
    pub fn anchor_outbox(&self) -> AnchorOutbox {
        let store = self.state.anchoring();
        AnchorOutbox {
            instruction: store
                .in_flight()
                .map(|c| (c.instruction(), c.status == CheckpointStatus::Submitted)),
            comparison_due: !store.uncompared().is_empty(),
        }
    }

    /// The adapter's read of [`Ledger::anchor_outbox`]. Any other principal is
    /// `ERR-NotAuthorised`.
    pub fn adapter_anchor(&self, caller: &Caller) -> Result<AnchorOutbox, Refusal> {
        resolve_adapter(&self.state, caller)?;
        Ok(self.anchor_outbox())
    }

    /// The adapter holds checkpoint `number`'s instruction durably: `Pending → Submitted`. It must
    /// name the commitment the instruction carries (`ERR-EvidenceInsufficient` otherwise). A
    /// repeat, or an acknowledgement of a checkpoint already final, changes nothing.
    pub(super) fn acknowledge_anchor(
        &mut self,
        number: u64,
        commitment: [u8; 32],
    ) -> Result<CheckpointStatus, Refusal> {
        let c = self
            .state
            .anchoring()
            .checkpoint(number)
            .copied()
            .filter(|c| c.commitment == commitment)
            .ok_or(Refusal::EvidenceInsufficient)?;
        if c.status == CheckpointStatus::Pending {
            self.commit(&[Write::Anchor(AnchorWrite::Acknowledge(number))])?;
            return Ok(CheckpointStatus::Submitted);
        }
        Ok(c.status)
    }

    /// The adapter proved its extrinsic for checkpoint `number` did not execute: `Submitted →
    /// Pending`, and the same instruction is in the outbox again. Otherwise nothing changes.
    pub(super) fn anchor_not_executed(&mut self, number: u64) -> Result<CheckpointStatus, Refusal> {
        let c = self
            .state
            .anchoring()
            .checkpoint(number)
            .copied()
            .ok_or(Refusal::EvidenceInsufficient)?;
        if c.status == CheckpointStatus::Submitted {
            self.commit(&[Write::Anchor(AnchorWrite::Reemit(number))])?;
            return Ok(CheckpointStatus::Pending);
        }
        Ok(c.status)
    }

    /// What an `Anchored` record that the canonical check just found final means (`REQ-PR-8`):
    /// the checkpoint of its number becomes `Final` if the commitment is the one it carries; any
    /// other commitment, or a number aledgent has no checkpoint for, is an incident. Called by
    /// `F-010`'s canonical check, in its commit.
    pub(super) fn anchor_final_writes(&self, obs: &Observation) -> Vec<Write> {
        let coordinate: Coordinate = obs.coordinate;
        let Some(fact) = obs.anchor else {
            // No fact to check: nothing becomes final on it.
            return Vec::new();
        };
        let incident = |i| vec![Write::Anchor(AnchorWrite::Incident(i))];
        match self.state.anchoring().checkpoint(fact.number) {
            Some(c) if c.commitment == fact.commitment => {
                if c.status == CheckpointStatus::Final {
                    Vec::new()
                } else {
                    // The venue it was read in is recorded with it (`F-027` T-027-04).
                    vec![Write::Anchor(AnchorWrite::Final {
                        number: c.number,
                        coordinate,
                        venue: fact.venue,
                        height: obs.at,
                    })]
                }
            }
            Some(c) => incident(AnchorIncident::CommitmentMismatch {
                number: c.number,
                expected: c.commitment,
                observed: fact.commitment,
                coordinate,
            }),
            None => incident(AnchorIncident::UnknownAnchor {
                number: fact.number,
                commitment: fact.commitment,
                coordinate,
            }),
        }
    }

    /// Whether an ingested record is an `Anchored` one: the canonical check calls
    /// [`Ledger::anchor_final_writes`] for those.
    pub(super) fn is_anchor_record(obs: &Observation) -> bool {
        obs.kind == EventKind::Anchored
    }

    /// The checkpoint a disclosure may rest on (`REQ-PR-8`, `AC-J1.3`): the one named, or the
    /// latest final one. Anything not final — pending, submitted, or no such number — is
    /// `ERR-AnchorNotFinal`.
    pub fn final_checkpoint(&self, number: Option<u64>) -> Result<Checkpoint, Refusal> {
        let store = self.state.anchoring();
        let c = match number {
            Some(n) => store.checkpoint(n).copied(),
            None => store.latest_final().copied(),
        };
        match c {
            Some(c) if c.status == CheckpointStatus::Final => Ok(c),
            _ => Err(Refusal::AnchorNotFinal),
        }
    }

    /// The anchor store, for the operator (`REQ-PV-2`: what it needs to operate): the operator's
    /// organisation identity with an unscoped credential, or a principal holding a live
    /// operator-surface grant for `AnchorCheckpoint` or `CompareSettlement`. Anyone else is
    /// `ERR-NotAuthorised`. It holds no account's fact: roots, sequences, and per-asset aggregates
    /// of what the book holds on the settlement layer.
    pub fn anchoring_view(
        &self,
        caller: &Caller,
        now: Time,
    ) -> Result<&crate::anchor::AnchorStore, Refusal> {
        let p = resolve(&self.state, caller)?;
        let allowed = self.is_operator(&p)
            || [
                OperatorOpKind::AnchorCheckpoint,
                OperatorOpKind::CompareSettlement,
            ]
            .into_iter()
            .any(|k| self.operator_authority(&p, k, now).is_ok());
        if !allowed {
            return Err(Refusal::NotAuthorised);
        }
        Ok(self.state.anchoring())
    }

    // ------------------------------------------------------------------ the comparison

    /// Every posting sequence number `op` wrote ([`crate::state::State::posting_seqs_of`]).
    pub(super) fn posting_seqs_of(&self, op: &OperationId) -> BTreeSet<u64> {
        self.state.posting_seqs_of(op)
    }

    /// The positions' tally advanced from its cursor over final inbox positions, up to `to` and
    /// over at most `max` of them, stopping at the first position not final: the cursor reached,
    /// the write, and the sums there. `None` if it does not move. Each position is read by key:
    /// its final record, the operation of a deposit or withdrawal not yet counted and that
    /// operation's own postings, and a binding decided `Bound` there (finding F18's class,
    /// `REQ-PR-5`).
    fn tally_advance(&self, to: u64, max: u64) -> Option<(u64, Write, BTreeMap<AssetId, i128>)> {
        let store = self.state.settlement();
        let instance = store.config()?.instance;
        let (through, held) = store.tally();
        let mut held = held.clone();
        let end = to.min(through.saturating_add(max));
        let mut counted: BTreeSet<OperationId> = BTreeSet::new();
        let mut k = through;
        while k < end {
            let Some(i) = store.final_at(instance, k).and_then(|c| store.ingested(c)) else {
                break;
            };
            if counts(i) {
                if let Some(op) = i.op.filter(|op| !store.is_counted(op)) {
                    if counted.insert(op) {
                        for (a, d) in self.state.held_by_operation(&op) {
                            *held.entry(a).or_default() += d;
                        }
                    }
                }
            }
            if let Some((asset, amount)) = self.bound_at(i) {
                *held.entry(asset).or_default() += amount;
            }
            k += 1;
        }
        (k > through).then(|| {
            let w = Write::Settlement(SettlementWrite::Tally {
                through: k,
                counted: counted.into_iter().collect(),
                held: held.clone(),
            });
            (k, w, held)
        })
    }

    /// The binding a final record decided `Bound`, if it is one: its asset and the amount the
    /// layer recorded as the deployment's position (amendment 0011, `REQ-AS-8`). A bound asset's
    /// position starts at its binding.
    fn bound_at(&self, i: &Ingested) -> Option<(AssetId, i128)> {
        if i.observation.kind != EventKind::Bound {
            return None;
        }
        let b = self
            .state
            .settlement()
            .asset_bind_by_reference(&i.observation.operation_ref?)?;
        (b.status == crate::settlement::assets::BindStatus::Bound
            && b.coordinate == Some(i.observation.coordinate))
        .then(|| (b.asset, i128::from(b.amount.get())))
    }

    /// The book's record of value held on the settlement layer, per asset, as of cut `k`
    /// (`REQ-PR-5`): what the settling postings of the operations bound to final records at inbox
    /// positions below `k` moved through the issuance counterparty, and what bindings decided
    /// below `k` started at. A deposit's `Issue` adds what it credited; an outbound leg's `Redeem`
    /// takes away what it executed. Issuance under issuance authority has no record on the layer
    /// and is not counted (`REQ-AS-6`).
    ///
    /// **From the tally** (finding F18's class, `F-032`). The sums are kept at the tally's cursor
    /// ([`crate::settlement::store::SettlementStore::tally`]), advanced by ticks and by the
    /// comparisons themselves, and a settling posting written later is added as it is written. A
    /// cut is then at most [`MAX_TALLY_PER_INVOCATION`] positions from the cursor either way, and
    /// only those positions are read: ahead, the tally advances to the cut in this invocation (the
    /// write is returned to commit with the comparison); behind, what those positions added is
    /// taken away. A cut further away, or one whose positions are not all final, is
    /// `ERR-EvidenceInsufficient`; ticks bring the tally on, and the adapter reports again. Before,
    /// a comparison read every ingested record and every final position of the inbox.
    fn book_positions(
        &self,
        instance: u32,
        k: u64,
    ) -> Result<(BTreeMap<AssetId, i128>, Option<Write>), Refusal> {
        let store = self.state.settlement();
        let (through, held) = store.tally();
        if k >= through {
            if k - through > MAX_TALLY_PER_INVOCATION {
                return Err(Refusal::EvidenceInsufficient);
            }
            if k == through {
                return Ok((held.clone(), None));
            }
            return match self.tally_advance(k, MAX_TALLY_PER_INVOCATION) {
                Some((reached, w, held)) if reached == k => Ok((held, Some(w))),
                _ => Err(Refusal::EvidenceInsufficient),
            };
        }
        if through - k > MAX_TALLY_PER_INVOCATION {
            return Err(Refusal::EvidenceInsufficient);
        }
        let mut held = held.clone();
        let mut taken: BTreeSet<OperationId> = BTreeSet::new();
        for seq in k..through {
            let i = store
                .final_at(instance, seq)
                .and_then(|c| store.ingested(c))
                .ok_or(Refusal::Internal)?;
            if counts(i) {
                if let Some(op) = i.op {
                    // Still counted at `k` if another of its records is final below `k`.
                    let below = store.of_op(&op).any(|r| {
                        counts(r)
                            && r.observation.coordinate.instance == instance
                            && r.observation.coordinate.seq < k
                    });
                    if !below && taken.insert(op) {
                        for (a, d) in self.state.held_by_operation(&op) {
                            *held.entry(a).or_default() -= d;
                        }
                    }
                }
            }
            if let Some((asset, amount)) = self.bound_at(i) {
                *held.entry(asset).or_default() -= amount;
            }
        }
        Ok((held, None))
    }

    /// The tally advanced on a tick, as far as the positions are final and at most
    /// [`MAX_TALLY_PER_INVOCATION`] of them, so that a comparison finds it near its cut. Never
    /// fails the tick: anything that stops it leaves it for the next.
    pub(super) fn tally_on_tick(&mut self) {
        if let Some((_, w, _)) = self.tally_advance(u64::MAX, MAX_TALLY_PER_INVOCATION) {
            let _ = self.commit(&[w]);
        }
    }

    /// Whether the tally has a final position to pass: a tick has work to do.
    pub(super) fn tally_behind(&self) -> bool {
        let store = self.state.settlement();
        store
            .config()
            .is_some_and(|c| store.final_at(c.instance, store.tally().0).is_some())
    }

    /// Compare the book with the layer's positions at `cut`: the comparison, and the tally's
    /// write if it advanced, to commit with it.
    fn compare_at(
        &self,
        cut: &PositionCut,
        now: Time,
        requested: Option<OperationId>,
    ) -> Result<(Comparison, Option<Write>), Refusal> {
        let config = self
            .state
            .settlement()
            .config()
            .ok_or(Refusal::EvidenceInsufficient)?;
        // Every position below the cut must be ingested and final, or a record still on its way
        // would show as a difference: the tally passes only final positions.
        let mut layer: BTreeMap<AssetId, Minor> = BTreeMap::new();
        for (asset, amount) in &cut.positions {
            if layer.insert(*asset, *amount).is_some() {
                return Err(Refusal::EvidenceInsufficient);
            }
        }
        let (book, tally) = self.book_positions(config.instance, cut.next_seq)?;
        let assets: BTreeSet<AssetId> = book.keys().chain(layer.keys()).copied().collect();
        let rows = assets
            .into_iter()
            .map(|asset| PositionRow {
                asset,
                book: book.get(&asset).copied().unwrap_or(0),
                layer: layer.get(&asset).copied().unwrap_or(Minor::ZERO),
            })
            .collect();
        let store = self.state.anchoring();
        let anchors = store
            .uncompared_since()
            .into_iter()
            .filter(|n| {
                store
                    .checkpoint(*n)
                    .and_then(|c| c.coordinate)
                    .is_some_and(|c| c.seq < cut.next_seq)
            })
            .collect();
        Ok((
            Comparison {
                cut: cut.next_seq,
                height: cut.height,
                at: now,
                anchors,
                requested,
                rows,
            },
            tally,
        ))
    }

    /// The writes a comparison makes: it becomes the latest; it is recorded when it is some
    /// anchor's comparison, when an operator asked for it, or when it differs; and every asset
    /// that differs where the latest comparison did not differ the same way is an incident
    /// (`REQ-PR-5`: a divergence is an incident, not a report).
    fn comparison_writes(&self, c: &Comparison) -> Vec<Write> {
        let previous = self.state.anchoring().latest_comparison();
        let mut writes = Vec::new();
        for r in c.rows.iter().filter(|r| r.difference() != 0) {
            let known = previous.is_some_and(|p| {
                p.rows
                    .iter()
                    .any(|q| q.asset == r.asset && q.difference() == r.difference())
            });
            if !known {
                writes.push(Write::Anchor(AnchorWrite::Incident(
                    AnchorIncident::PositionDifference {
                        cut: c.cut,
                        asset: r.asset,
                        book: r.book,
                        layer: r.layer,
                    },
                )));
            }
        }
        if !c.anchors.is_empty() || c.requested.is_some() || c.differs() {
            writes.push(Write::Anchor(AnchorWrite::Compare(c.clone())));
        }
        writes.push(Write::Anchor(AnchorWrite::Latest(c.clone())));
        writes
    }

    /// The adapter's report of the layer's positions at a finalized block (`REQ-PR-5`): compared
    /// at once, continuously. `ERR-EvidenceInsufficient` while a position below the cut is not
    /// ingested and final, or if an asset is reported twice.
    pub(super) fn record_positions(
        &mut self,
        cut: PositionCut,
        now: Time,
    ) -> Result<Comparison, Refusal> {
        let (c, tally) = self.compare_at(&cut, now, None)?;
        let mut writes: Vec<Write> = tally.into_iter().collect();
        writes.extend(self.comparison_writes(&c));
        self.commit(&writes)?;
        Ok(c)
    }

    /// `compare_settlement` on `CTR-OPS` (`REQ-PR-5` on request): through the operator frame
    /// (kind `CompareSettlement`, a justification, the client key). Compares the book with the
    /// layer's positions at the latest cut the adapter reported, and records the comparison.
    /// `ERR-EvidenceInsufficient` if the adapter has reported none.
    pub fn compare_settlement(
        &mut self,
        ctx: &Ctx,
        justification: &[u8],
    ) -> Result<Accepted<Comparison>, Refusal> {
        let top = self.enter(|| crate::call::Call::CompareSettlement {
            ctx: *ctx,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<Comparison>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("compare_settlement", &p, justification, |_| {});
            let store = self.state.anchoring();
            let found = |r: &crate::state::OperationRecord| {
                store
                    .comparisons()
                    .iter()
                    .find(|c| c.requested == Some(r.op))
                    .cloned()
            };
            if let Some(a) = self.replay(ctx, &p, &d, found)? {
                return match a.value {
                    Some(value) => Ok(Accepted {
                        value,
                        record: a.record,
                        status: a.status,
                    }),
                    None => Err(Refusal::NotAuthorised),
                };
            }
            let command = Command::CompareSettlement;
            let (used, j) = match self.frame(
                &p,
                OperatorOpKind::CompareSettlement,
                justification,
                ctx.now,
            ) {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, Vec::new(), r)),
            };
            let run = |l: &Self| -> Result<(Comparison, Option<Write>), Refusal> {
                let latest = l
                    .state
                    .anchoring()
                    .latest_comparison()
                    .ok_or(Refusal::EvidenceInsufficient)?;
                let cut = PositionCut {
                    next_seq: latest.cut,
                    height: latest.height,
                    positions: latest.rows.iter().map(|r| (r.asset, r.layer)).collect(),
                };
                l.compare_at(&cut, ctx.now, Some(ctx.op))
            };
            let (c, tally) = match run(self) {
                Ok(c) => c,
                Err(r) => return Err(self.refused(ctx, &p, command, Vec::new(), r)),
            };
            let subjects = c
                .rows
                .iter()
                .map(|r| AuditSubject::Asset(r.asset))
                .collect();
            let mut writes: Vec<Write> = tally.into_iter().collect();
            writes.extend(self.comparison_writes(&c));
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                subjects,
                writes,
                c,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }
}
