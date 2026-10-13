// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Resolving a `Needs-Review` operation (`F-013` §5, T-013-04, T-013-10; `REQ-RC-1`, `REQ-RC-5`,
//! `REQ-AT-3`, `REQ-OP-9`, `INV-10`, `INV-11`, `US-K1`).
//!
//! Amendment 0006, as the owner refined it on 2026-09-29, resolves an operation in up to **three
//! steps**, and a human decides only in the last:
//!
//! 1. **The settlement adapter's proof, on its own.** Whenever the records the adapter reported
//!    prove the outcome, the core applies it with no human (`Trigger::ProvenOutcome`,
//!    [`Ledger::resolve_by_proof`]): a final record of the leg naming the operation settles it; a
//!    proved rejection, proven non-inclusion (the key's nonce unconsumed), or an inbound record
//!    orphaned releases it.
//! 2. **Evidence one authority submits** (`submit_evidence`), when the adapter could not prove the
//!    outcome itself (it missed blocks, say): the settlement layer's block and transaction. The
//!    adapter fetches and verifies it on its own ([`crate::settlement::EvidenceFetch`]), reports
//!    what it found through its ordinary inputs, and then `evidence_checked`: if the records prove
//!    the outcome, step 1 applies it. One authority is enough, because the proof decides. An
//!    operator may also cite a proof the store holds (`resolve_operation` with
//!    [`ResolutionEvidence::Record`] or [`ResolutionEvidence::NonInclusion`]), checked the same way.
//! 3. **Two distinct principals** ([`ResolutionEvidence::NoProof`]), only once the adapter checked
//!    submitted evidence and found no proof: the first `resolve_operation` records the proposal,
//!    a second, **different** principal with its own resolution authority agrees, and an incident
//!    is raised. A release is then applied without mechanical proof. A settlement of an external
//!    leg is not: it would write against the issuance counterparty before finality, which `INV-10`
//!    forbids; that part of the ruling waits for the owner (`OQ-62`).
//!
//! An operation with **no external leg** (a reservation past its deadline, a command never
//! decided, a reversal never approved) has nothing outside the book that could take effect: one
//! principal releases it with no evidence (`REQ-OP-9`). It cannot be settled here: a capture needs
//! a destination a resolution does not carry.
//!
//! `F-007` checks every transition (`ERR-InvalidTransition`, `ERR-OperationTerminal`, and the proof
//! or the second principal in `next`). The frame's custody rule applies to the payer whose
//! reservation it ends.

use alloc::vec;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;
use ledger_kernel::posting::PostingKind;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::audit::{AuditSubject, Justification};
use crate::authority::AuthorityUsed;
use crate::id::{AccountId, OperationId};
use crate::ledger::lifecycle::{Attribution, Plan, Refusals, Stop};
use crate::ledger::{Accepted, Ctx, Ledger};
use crate::lifecycle::keys::Evidence;
use crate::lifecycle::{KindClass, Resolution, Shown, State, Trigger};
use crate::operator::OperatorOpKind;
use crate::principal::resolve;
use crate::refusal::Refusal;
use crate::reservation::{CHOSEN, ReservationBook, UnitBuilder};
use crate::settlement::Coordinate;
use crate::settlement::store::SettlementWrite;
use crate::state::{Command, OperationRecord, Write};

/// What an operator cites to resolve an operation: evidence the settlement store holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ResolutionEvidence {
    /// A record the settlement store ingested, by its coordinate.
    Record(Coordinate),
    /// Non-inclusion of the operation's instruction under the last finalised head
    /// (`REQ-SL-4`): the proof covers a range of positions, not one record.
    NonInclusion,
    /// No mechanical proof: a resolution two distinct principals agree on (`REQ-RC-5`, step 3),
    /// open only once the adapter checked submitted evidence and found no proof.
    NoProof,
}

/// `submit_evidence`'s request (`REQ-RC-5`, step 2): where on the settlement layer the proof is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SubmitEvidence {
    /// The operation in `Needs-Review`.
    pub op: OperationId,
    /// The settlement layer's block.
    pub block: u64,
    /// The transaction's hash in it.
    pub transaction: [u8; 32],
}

/// `resolve_operation`'s request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Resolve {
    /// The operation in `Needs-Review`.
    pub op: OperationId,
    /// What it resolves to.
    pub target: Resolution,
    /// The evidence cited.
    pub evidence: ResolutionEvidence,
}

/// The digest of the evidence a resolution cites: what its transition key and its audit record
/// name.
fn evidence_digest(op: &OperationId, e: &ResolutionEvidence) -> [u8; 32] {
    match e {
        ResolutionEvidence::Record(c) => blake2b_256(
            b"aledgent/resolution-evidence/v1",
            &[&op.to_ref().0, &[1], &c.bytes()],
        ),
        ResolutionEvidence::NonInclusion => {
            blake2b_256(b"aledgent/resolution-evidence/v1", &[&op.to_ref().0, &[2]])
        }
        ResolutionEvidence::NoProof => {
            blake2b_256(b"aledgent/resolution-evidence/v1", &[&op.to_ref().0, &[3]])
        }
    }
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `resolve_operation` (SPEC §8.1, `REQ-RC-1`, `AC-K1.1`, `AC-K1.2`). See the module
    /// documentation. Returns the state the operation entered.
    pub fn resolve_operation(
        &mut self,
        ctx: &Ctx,
        request: Resolve,
        justification: &[u8],
    ) -> Result<Accepted<State>, Refusal> {
        let top = self.enter(|| crate::call::Call::ResolveOperation {
            ctx: *ctx,
            request,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<State>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let cited = evidence_digest(&request.op, &request.evidence);
            let d = digest("resolve_operation", &p, justification, |c| {
                c.bytes(request.op.as_bytes());
                c.u8(request.target as u8);
                c.bytes(&cited);
            });
            // A proposal without proof (step 3) left the operation in `Needs-Review`; so does
            // its replay, until a second principal agrees.
            let target = match self.state.operations().get(&request.op).map(|o| o.state()) {
                Some(State::NeedsReview) if request.evidence == ResolutionEvidence::NoProof => {
                    State::NeedsReview
                }
                _ => request.target.target(),
            };
            if let Some(a) = self.replay(ctx, &p, &d, |_| target)? {
                return Ok(Accepted {
                    status: Some(Shown::of(target)),
                    ..a
                });
            }
            let command = Command::ResolveOperation;
            let subjects = vec![
                AuditSubject::Operation(request.op),
                AuditSubject::Evidence(cited),
            ];
            let prepared = self.prepare_resolution(ctx, &request, justification, cited);
            let (used, j, payer, plan, entered) = match prepared {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            let mut plan = plan;
            plan.writes.push(Self::justified(ctx, j, subjects.clone()));
            plan.attribution = Attribution::Put(OperationRecord {
                op: ctx.op,
                command,
                authorities: vec![used],
                accounts: payer.into_iter().collect(),
                postings: Vec::new(),
                at: ctx.now,
            });
            plan.request = Some(Self::operator_request(ctx, &p, d));
            match self.commit_plan(ctx.now, plan) {
                Ok(done) => Ok(Accepted {
                    value: entered,
                    record: done.record.ok_or(Refusal::Internal)?,
                    status: Some(Shown::of(entered)),
                }),
                Err(f) => Err(self.refused(ctx, &p, command, subjects, f.refusal)),
            }
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `submit_evidence` (SPEC §8.1, `REQ-RC-5` step 2, `AC-K1.4`): one principal with the
    /// `ResolveOperation` authority names where on the settlement layer the proof of a
    /// `Needs-Review` operation's outcome is — a block and a transaction — for the settlement
    /// adapter to fetch and verify on its own. Nothing moves here: the outcome follows the proof
    /// the adapter reports ([`Ledger::evidence_checked`]). Returns the submission's reference.
    pub fn submit_evidence(
        &mut self,
        ctx: &Ctx,
        request: SubmitEvidence,
        justification: &[u8],
    ) -> Result<Accepted<OperationId>, Refusal> {
        let top = self.enter(|| crate::call::Call::SubmitEvidence {
            ctx: *ctx,
            request,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<OperationId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("submit_evidence", &p, justification, |c| {
                c.bytes(request.op.as_bytes());
                c.u64(request.block);
                c.bytes(&request.transaction);
            });
            if let Some(a) = self.replay(ctx, &p, &d, |r| r.op)? {
                return Ok(a);
            }
            let command = Command::SubmitEvidence;
            let subjects = vec![AuditSubject::Operation(request.op)];
            let prepared = (|| {
                let (used, j) =
                    self.frame(&p, OperatorOpKind::ResolveOperation, justification, ctx.now)?;
                let o = self
                    .state
                    .operations()
                    .get(&request.op)
                    .ok_or(Refusal::UnknownOperation)?;
                if o.state() != State::NeedsReview {
                    return Err(if o.state().is_terminal() {
                        Refusal::OperationTerminal
                    } else {
                        Refusal::InvalidTransition
                    });
                }
                if !o.class().has_external_leg() {
                    // Nothing on the settlement layer to prove: it is released on request.
                    return Err(Refusal::InvalidTransition);
                }
                Ok((used, j))
            })();
            let (used, j) = match prepared {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            let submission = crate::operator::EvidenceSubmission {
                id: ctx.op,
                op: request.op,
                block: request.block,
                transaction: request.transaction,
                by: used,
                at: ctx.now,
                finding: None,
            };
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                subjects,
                vec![Write::Ops(crate::operator::OpsWrite::SubmitEvidence(
                    submission,
                ))],
                ctx.op,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Everything `resolve_operation` checks before it writes, and the plan it commits.
    #[allow(clippy::type_complexity)]
    fn prepare_resolution(
        &self,
        ctx: &Ctx,
        request: &Resolve,
        justification: &[u8],
        cited: [u8; 32],
    ) -> Result<(AuthorityUsed, Justification, Option<AccountId>, Plan, State), Refusal> {
        let p = resolve(&self.state, &ctx.caller)?;
        let (used, j) = self.frame(&p, OperatorOpKind::ResolveOperation, justification, ctx.now)?;
        let o = self
            .state
            .operations()
            .get(&request.op)
            .ok_or(Refusal::UnknownOperation)?
            .clone();
        if o.state() != State::NeedsReview {
            return Err(if o.state().is_terminal() {
                Refusal::OperationTerminal
            } else {
                Refusal::InvalidTransition
            });
        }
        // Operator authority over the payer whose reservation this ends (`REQ-AU-5`).
        let payer = self
            .state
            .reservations()
            .status(&request.op)
            .map(|s| s.reservation().payer.account);
        if let Some(a) = &payer {
            self.delegated(a)?;
        }
        // The evidence: records the settlement store holds, never an outcome asserted.
        let mut facts = self.facts_of(&o, ctx.now);
        facts.justified = true;
        facts.resolution = request.target;
        let mut settling: Option<(UnitBuilder, Option<crate::settlement::Incident>)> = None;
        let mut bind: Option<Coordinate> = None;
        match (request.target, request.evidence) {
            // Step 3 (`REQ-RC-5`): no mechanical proof. Open only once the adapter checked
            // submitted evidence and found none; then two distinct principals must agree.
            (target, ResolutionEvidence::NoProof) if o.class().has_external_leg() => {
                let case = self.state.ops().resolution(&request.op);
                if !case.is_some_and(|c| c.unprovable()) {
                    return Err(Refusal::EvidenceInsufficient);
                }
                if target == Resolution::Settle {
                    // `INV-10`: nothing against the issuance counterparty before finality. The
                    // ruling's settlement without proof waits for the owner (`OQ-62`).
                    return Err(Refusal::EvidenceInsufficient);
                }
                match case.and_then(|c| c.proposed) {
                    None => {
                        // The first principal: the proposal is recorded, nothing moves.
                        let proposed = crate::operator::ProposedResolution {
                            target,
                            by: used,
                            at: ctx.now,
                        };
                        let plan = Plan {
                            unit: None,
                            writes: vec![Write::Ops(
                                crate::operator::OpsWrite::ProposeResolution {
                                    op: request.op,
                                    proposed,
                                },
                            )],
                            open: None,
                            transitions: Vec::new(),
                            attribution: Attribution::None,
                            request: None,
                            refusals: Refusals::Transition,
                        };
                        return Ok((used, j, payer, plan, State::NeedsReview));
                    }
                    Some(first) => {
                        if first.by.principal == used.principal {
                            return Err(Refusal::ApprovalRequired);
                        }
                        if first.target != target {
                            return Err(Refusal::InvalidTransition);
                        }
                        facts.two_approvers = true;
                    }
                }
            }
            (Resolution::Settle, ResolutionEvidence::Record(c)) => {
                settling = Some(self.resolution_settling(&o, &c)?);
                bind = Some(c);
                facts.finality_passed = true;
                facts.canonical_match = true;
            }
            (
                Resolution::Settle,
                ResolutionEvidence::NonInclusion | ResolutionEvidence::NoProof,
            ) => {
                return Err(Refusal::EvidenceInsufficient);
            }
            (Resolution::Release, e) => {
                let proven = !o.class().has_external_leg()
                    || match e {
                        ResolutionEvidence::Record(c) => self.release_evidenced(&o, &c),
                        ResolutionEvidence::NonInclusion => {
                            o.class() == KindClass::Outbound
                                && self.non_inclusion_proven(&request.op)
                        }
                        ResolutionEvidence::NoProof => false,
                    };
                if !proven {
                    return Err(Refusal::ReleaseUnsafe);
                }
                facts.non_inclusion_proven = true;
            }
        }
        let planned = match self.plan_move(
            request.op,
            State::NeedsReview,
            Trigger::OperatorResolve,
            &facts,
            Evidence(cited),
            None,
            ctx.now,
            vec![used],
        ) {
            Ok(pl) => pl,
            Err(Stop::Refuse(r)) => return Err(r),
            Err(Stop::Stay(_)) => return Err(Refusal::InvalidTransition),
        };
        let mut writes = Vec::new();
        let unit = match planned.write.posting {
            None => None,
            Some(PostingKind::Release) => {
                let mut u = UnitBuilder::new();
                CHOSEN.release(self.state.reservations(), &mut u, &request.op)?;
                Some(u)
            }
            Some(PostingKind::Redeem | PostingKind::Issue) => {
                let (u, incident) = settling.ok_or(Refusal::EvidenceInsufficient)?;
                writes.extend(incident.map(|i| Write::Settlement(SettlementWrite::Incident(i))));
                // The settled leg rests on the cited record (`INV-11`, `INV-12`).
                writes.extend(bind.map(|c| {
                    Write::Settlement(SettlementWrite::Bind {
                        op: request.op,
                        coordinate: Some(c),
                    })
                }));
                Some(u)
            }
            // `Capture` needs a destination, which a resolution does not carry: a reservation
            // with no external leg that reached `Needs-Review` is released, not captured.
            Some(_) => return Err(Refusal::InvalidTransition),
        };
        if request.evidence == ResolutionEvidence::NoProof {
            // Humans decided without mechanical proof: the operator channel is told (`REQ-RC-5`).
            writes.push(Write::Ops(crate::operator::OpsWrite::Incident(
                crate::operator::Incident::new(
                    crate::operator::IncidentKind::ResolvedWithoutProof,
                    &request.op.to_ref().0,
                    Some(request.op),
                    ctx.now,
                ),
            )));
        }
        let entered = planned.write.to;
        let plan = Plan {
            unit,
            writes,
            open: None,
            transitions: vec![planned],
            attribution: Attribution::None,
            request: None,
            refusals: Refusals::Transition,
        };
        Ok((used, j, payer, plan, entered))
    }
}
