// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `propose_reversal` / `approve_reversal` (`F-013` §5, T-013-05, `REQ-RC-2`, `REQ-OS-3`,
//! `REQ-PO-3`, `INV-2`, `INV-21`, `US-K2`).
//!
//! **A reversal is an operation of its own** (amendment 0006 items 1 and 4, ruled 2026-09-29).
//! It is maker-checker: one principal proposes it and a **different** principal approves it,
//! each under its **own** reversal authority on the operator surface (`ProposeReversal`,
//! `ApproveReversal`; `REQ-OS-1`, `REQ-OS-3`). Two checks by one principal are not two approvers:
//! an approval by the proposer is `ERR-ApprovalRequired`, and no code path, timer, tick or
//! observation approves one (`REQ-RC-2`).
//!
//! - **Propose.** The reversal operation opens under the proposal's reference, kind
//!   [`OperationKind::Reversal`](crate::authority::OperationKind::Reversal), and is decided at once:
//!   `Received → Accepted`, with the review service level as its deadline to be approved. An
//!   incident is raised on the operator channel.
//! - **Approve.** `Accepted → Settled`, with one `Reversal` posting that mirrors the postings it
//!   reverses and names them (`REQ-PO-3`). The reversed operation **stays `Settled`**: nothing
//!   leaves a terminal state (`INV-21`), and its postings are never touched (`INV-2`).
//! - **Unapproved** past its deadline, the reversal operation goes to `Needs-Review` like any
//!   other (amendment 0006 item 5), where an operator resolves it to `Released`: nothing was
//!   posted, so nothing returns.
//!
//! At most one reversal per posting (`REQ-PO-3`) holds at the kernel: the `Reversal` posting's
//! identity is `(reversed operation, Reversal, 0)`, so the kernel refuses a second one for ever
//! (`REQ-KER-6`), and a proposal for an operation already reversed is refused up front
//! (`ERR-InvalidTransition`). The posting is recorded in the reversal operation's `Settled`
//! transition.
//!
//! ## What is posted
//!
//! Only a `Settled` operation is reversed. `Needs-Review` resolves to `Settled` or `Released`;
//! value to return after that is a reversal of the settled operation.
//!
//! - **Settled by a capture** (`F-004`): under `DEC-2` Option A, the capture closed the holding
//!   position, and the kernel refuses any entry on a closed account, so the mirror of the
//!   capture — and of the reserve — cannot be posted. The reversal is the capture's mirror with the
//!   holding position replaced by the payer it was reserved from: the linked chain the plan
//!   describes (mirror of the capture, then mirror of the reserve), with the two holding legs,
//!   which cancel, left out. It is complete: every entry of the capture is reversed, from the
//!   book's own records, never from a caller's amount.
//! - **Settled by `Redeem` or `Issue`** (an external leg): the reversal would move value through
//!   the issuance counterparty, which no authority reaches outside `issue` and `redeem` (`F-003`):
//!   `ERR-NotAuthorised`. A correction there is an `issue` or a `redeem` (`US-K4`).
//! - **Settled by a `Transfer`**: `TODO(F-008)`: the transfer's posting is rebuilt from `F-008`'s
//!   records and mirrored with `void_of`; until then `ERR-InvalidTransition`.
//!
//! Every posting a reversal is built from is first rebuilt through `F-004`'s seam and checked
//! against the kernel's stored transfer, commitment by commitment: a disagreement fails closed.

use alloc::vec;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;
use ledger_kernel::account::KernelAccountSpec;
use ledger_kernel::build::build_transfer;
use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::{KeySet, LeafStore};
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::audit::AuditSubject;
use crate::authority::OperationKind;
use crate::id::{AccountId, OperationId};
use crate::ledger::lifecycle::{Attribution, Plan, Refusals, Stop};
use crate::ledger::{Accepted, Ctx, Ledger};
use crate::lifecycle::keys::Evidence;
use crate::lifecycle::store::NewOperation;
use crate::lifecycle::store::Operation;
use crate::lifecycle::{Facts, KindClass, Origin, Shown, State, Trigger};
use crate::operator::{Incident, IncidentKind, OperatorOpKind, OpsWrite, PendingReversal};
use crate::principal::resolve;
use crate::refusal::Refusal;
use crate::reservation::{
    CHOSEN, Closing, IndexChange, Position, Reservation, ReservationBook, ReservationIndex, Status,
    UnitBuilder,
};
use crate::state::{Command, OperationRecord, Write};

/// What reversing one operation posts, and what it touches.
#[derive(Clone, Debug)]
pub(crate) struct Reversing {
    /// The one `Reversal` posting.
    pub(crate) draft: PostingDraft,
    /// The postings it reverses, by sequence number (`REQ-PO-3`).
    pub(crate) reverses: Vec<u64>,
    /// The ledger accounts it touches.
    pub(crate) accounts: Vec<AccountId>,
    /// The ledger accounts whose value it moves out of the holder's hands: the operator needs
    /// authority over each (`REQ-AU-5`), and each must accept a debit.
    pub(crate) moves: Vec<AccountId>,
    /// The reservation it ends, if one is open.
    pub(crate) close: Option<IndexChange>,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `propose_reversal` (SPEC §8.1, `REQ-RC-2`, `AC-K2.1`): open a reversal operation of the
    /// settled operation `op`, under the proposal's reference `ctx.op`, decided at once into
    /// `Accepted` with the review service level as its deadline, and raise an incident.
    /// `approve_reversal` names the same reference. Checks everything the approval will need but
    /// the second principal, so a proposal that could never be approved is refused now.
    pub fn propose_reversal(
        &mut self,
        ctx: &Ctx,
        op: OperationId,
        justification: &[u8],
    ) -> Result<Accepted<OperationId>, Refusal> {
        let top = self.enter(|| crate::call::Call::ProposeReversal {
            ctx: *ctx,
            op,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<OperationId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("propose_reversal", &p, justification, |c| {
                c.bytes(op.as_bytes());
            });
            if let Some(a) = self.replay(ctx, &p, &d, |r| r.op)? {
                return Ok(a);
            }
            let command = Command::ProposeReversal;
            let mut subjects = vec![AuditSubject::Operation(op)];
            let prepared = (|| {
                let (used, j) =
                    self.frame(&p, OperatorOpKind::ProposeReversal, justification, ctx.now)?;
                let o = self
                    .state
                    .operations()
                    .get(&op)
                    .ok_or(Refusal::UnknownOperation)?;
                let reversing = self.reversal_of(o)?;
                self.may_reverse(&reversing)?;
                if self.reversed_already(&reversing)? {
                    return Err(Refusal::InvalidTransition);
                }
                if self.state.operations().get(&ctx.op).is_some() {
                    return Err(Refusal::ClientKeyReused);
                }
                let mut facts = Facts::new(Origin::Command, KindClass::Reversal);
                facts.accepts = true;
                let accepted = self
                    .plan_move(
                        ctx.op,
                        State::Received,
                        Trigger::Decide,
                        &facts,
                        Evidence::request(&d),
                        Some(ctx.now.saturating_add(self.config.review_service_level)),
                        ctx.now,
                        vec![used],
                    )
                    .map_err(|s| match s {
                        Stop::Refuse(r) => r,
                        Stop::Stay(_) => Refusal::InvalidTransition,
                    })?;
                Ok((used, j, o.state(), reversing, accepted))
            })();
            let (used, j, from, reversing, accepted) = match prepared {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            subjects.extend(
                reversing
                    .reverses
                    .iter()
                    .copied()
                    .map(AuditSubject::Posting),
            );
            let pending = PendingReversal {
                proposal: ctx.op,
                op,
                from,
                reverses: reversing.reverses.clone(),
                proposer: used,
                proposed_at: ctx.now,
                approval: None,
            };
            let incident = Incident::new(
                IncidentKind::ReversalProposed,
                &ctx.op.to_ref().0,
                Some(op),
                ctx.now,
            );
            let plan = Plan {
                unit: None,
                writes: vec![
                    Write::Ops(OpsWrite::Propose(pending)),
                    Write::Ops(OpsWrite::Incident(incident)),
                    Self::justified(ctx, j, subjects.clone()),
                ],
                open: Some(NewOperation {
                    reference: ctx.op,
                    kind: OperationKind::Reversal,
                    class: KindClass::Reversal,
                    origin: Origin::Command,
                    initiator: Some(p.identity),
                    state: State::Received,
                    deadline: Some(self.received_deadline(ctx.now)),
                    at: ctx.now,
                }),
                transitions: vec![accepted],
                attribution: Attribution::Put(OperationRecord {
                    op: ctx.op,
                    command,
                    authorities: vec![used],
                    accounts: reversing.accounts,
                    postings: Vec::new(),
                    at: ctx.now,
                }),
                request: Some(Self::operator_request(ctx, &p, d)),
                refusals: Refusals::Transition,
            };
            match self.commit_plan(ctx.now, plan) {
                Ok(done) => Ok(Accepted {
                    value: ctx.op,
                    record: done.record.ok_or(Refusal::Internal)?,
                    status: Some(Shown::of(State::Accepted)),
                }),
                Err(f) => Err(self.refused(ctx, &p, command, subjects, f.refusal)),
            }
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `approve_reversal` (SPEC §8.1, `REQ-OS-3`, `AC-K2.1`, `AC-K2.2`): a second, distinct
    /// principal, under its own `ApproveReversal` authority, approves the reversal operation
    /// `proposal`: it settles (`Accepted → Settled`) with its one `Reversal` posting, all or
    /// nothing. The reversed operation stays `Settled`. The proposer approving its own proposal is
    /// `ERR-ApprovalRequired`. Returns the reversal posting's sequence number.
    pub fn approve_reversal(
        &mut self,
        ctx: &Ctx,
        proposal: OperationId,
        justification: &[u8],
    ) -> Result<Accepted<u64>, Refusal> {
        let top = self.enter(|| crate::call::Call::ApproveReversal {
            ctx: *ctx,
            proposal,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<u64>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("approve_reversal", &p, justification, |c| {
                c.bytes(proposal.as_bytes());
            });
            if let Some(a) =
                self.replay(ctx, &p, &d, |r| r.postings.first().copied().unwrap_or(0))?
            {
                return Ok(a);
            }
            let command = Command::ApproveReversal;
            let target = self.state.ops().reversal(&proposal).map(|r| r.op);
            let mut subjects: Vec<AuditSubject> =
                target.map(AuditSubject::Operation).into_iter().collect();
            let prepared = (|| {
                let (used, j) =
                    self.frame(&p, OperatorOpKind::ApproveReversal, justification, ctx.now)?;
                let pending = self
                    .state
                    .ops()
                    .reversal(&proposal)
                    .ok_or(Refusal::UnknownOperation)?
                    .clone();
                if pending.approval.is_some() {
                    return Err(Refusal::OperationTerminal);
                }
                if pending.proposer.principal == used.principal {
                    return Err(Refusal::ApprovalRequired);
                }
                let o = self
                    .state
                    .operations()
                    .get(&pending.op)
                    .ok_or(Refusal::UnknownOperation)?
                    .clone();
                if o.state() != pending.from {
                    return Err(Refusal::InvalidTransition);
                }
                // The reversal operation itself: approvable only while `Accepted`. Unapproved
                // past its deadline it went to `Needs-Review` (item 5), and is resolved there.
                let rv = self
                    .state
                    .operations()
                    .get(&proposal)
                    .ok_or(Refusal::UnknownOperation)?
                    .clone();
                if rv.state() != State::Accepted {
                    return Err(if rv.state().is_terminal() {
                        Refusal::OperationTerminal
                    } else {
                        Refusal::InvalidTransition
                    });
                }
                let reversing = self.reversal_of(&o)?;
                self.may_reverse(&reversing)?;
                let mut facts = self.facts_of(&rv, ctx.now);
                facts.justified = true;
                facts.two_approvers = true;
                let evidence = Evidence(blake2b_256(
                    b"aledgent/reversal/v1",
                    &[&proposal.to_ref().0],
                ));
                let planned = match self.plan_move(
                    proposal,
                    State::Accepted,
                    Trigger::ApproveReversal,
                    &facts,
                    evidence,
                    None,
                    ctx.now,
                    vec![pending.proposer, used],
                ) {
                    Ok(pl) => pl,
                    Err(Stop::Refuse(r)) => return Err(r),
                    Err(Stop::Stay(_)) => return Err(Refusal::InvalidTransition),
                };
                Ok((used, j, pending, reversing, planned))
            })();
            let (used, j, pending, reversing, planned) = match prepared {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            subjects.extend(pending.reverses.iter().copied().map(AuditSubject::Posting));
            let mut u = UnitBuilder::new();
            u.post(reversing.draft.clone());
            if let Some(c) = reversing.close {
                u.change(c);
            }
            let plan = Plan {
                unit: Some(u),
                writes: vec![
                    Write::Ops(OpsWrite::Approve {
                        proposal,
                        approver: used,
                        at: ctx.now,
                    }),
                    Self::justified(ctx, j, subjects.clone()),
                ],
                open: None,
                transitions: vec![planned],
                attribution: Attribution::Put(OperationRecord {
                    op: ctx.op,
                    command,
                    authorities: vec![used, pending.proposer],
                    accounts: reversing.accounts.clone(),
                    postings: Vec::new(),
                    at: ctx.now,
                }),
                request: Some(Self::operator_request(ctx, &p, d)),
                refusals: Refusals::Transition,
            };
            match self.commit_plan(ctx.now, plan) {
                Ok(done) => Ok(Accepted {
                    value: done.postings.first().copied().unwrap_or(0),
                    record: done.record.ok_or(Refusal::NotAuthorised)?,
                    status: Some(Shown::of(State::Settled)),
                }),
                // The proposal stays pending: a kernel refusal (the destination no longer holds the
                // value, `F-013` Open points) moves nothing.
                Err(f) => Err(self.refused(ctx, &p, command, subjects, f.refusal)),
            }
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The operator's authority over what a reversal moves (`REQ-AU-5`, `REQ-AU-6`): each account
    /// whose value it takes must be in delegated custody and accept a debit.
    fn may_reverse(&self, r: &Reversing) -> Result<(), Refusal> {
        for a in &r.moves {
            self.delegated(a)?.require_open()?;
        }
        for a in &r.accounts {
            self.state
                .account(a)
                .ok_or(Refusal::UnknownAccount)?
                .require_creditable()?;
        }
        Ok(())
    }

    /// Whether the operation `r` reverses has been reversed already: the kernel holds `r`'s
    /// posting, of identity `(op, Reversal, 0)` (`REQ-PO-3`), in its home partition.
    fn reversed_already(&self, r: &Reversing) -> Result<bool, Refusal> {
        let id = r.draft.transfer_id();
        let mut keys = KeySet::default();
        keys.transfers.insert(id);
        let loaded = self
            .book
            .store()
            .load(&keys)
            .map_err(|_| Refusal::Internal)?;
        Ok(loaded.transfers.get(&id).cloned().flatten().is_some())
    }

    /// What reversing the settled operation `o` posts. See the module documentation.
    pub(crate) fn reversal_of(&self, o: &Operation) -> Result<Reversing, Refusal> {
        let op = o.reference();
        let meta = PostingMeta::new(PostingKind::Reversal, op.to_ref(), 0);
        let posted = |kind: PostingKind| -> Vec<u64> {
            o.transitions()
                .iter()
                .filter(|t| t.posting == Some(kind))
                .flat_map(|t| t.postings.iter().copied())
                .collect()
        };
        match o.state() {
            State::Settled => {
                let settled_by = o
                    .transitions()
                    .iter()
                    .rev()
                    .find(|t| t.to == State::Settled)
                    .and_then(|t| t.posting)
                    .ok_or(Refusal::InvalidTransition)?;
                match settled_by {
                    PostingKind::Capture => {
                        let Some(Status::Closed(r, closing)) =
                            self.state.reservations().status(&op)
                        else {
                            return Err(Refusal::InvalidTransition);
                        };
                        let (_, capture) = self.rebuilt(&r, Some(closing))?;
                        let capture = capture.ok_or(Refusal::InvalidTransition)?;
                        let Closing::Captured { destination, .. } = closing else {
                            return Err(Refusal::InvalidTransition);
                        };
                        let draft = fold(&capture, &r, meta)?;
                        // The check no approver overrides (`REQ-RC-5`, `REQ-PO-3`): the posting
                        // mirrors the capture exactly and whole.
                        mirrors(&capture, &draft, &r)?;
                        let mut accounts = vec![r.payer.account];
                        let mut moves = Vec::new();
                        if destination != r.payer.account {
                            accounts.push(destination);
                            moves.push(destination);
                        }
                        Ok(Reversing {
                            draft,
                            reverses: posted(PostingKind::Capture),
                            accounts,
                            moves,
                            close: None,
                        })
                    }
                    // Through the issuance counterparty: only `issue` and `redeem` move value
                    // there (`F-003`).
                    PostingKind::Redeem | PostingKind::Issue => Err(Refusal::NotAuthorised),
                    // TODO(F-008): mirror the transfer's posting with `void_of`.
                    _ => Err(Refusal::InvalidTransition),
                }
            }
            s if s.is_terminal() => Err(Refusal::OperationTerminal),
            _ => Err(Refusal::InvalidTransition),
        }
    }

    /// The `Reserve` posting of reservation `r`, and the posting that ended it as `closing` says,
    /// rebuilt through `F-004`'s seam from the index's own records and checked against the
    /// kernel's stored transfers.
    pub(crate) fn rebuilt(
        &self,
        r: &Reservation,
        closing: Option<Closing>,
    ) -> Result<(PostingDraft, Option<PostingDraft>), Refusal> {
        let mut index = ReservationIndex::new();
        let mut u = UnitBuilder::new();
        CHOSEN.reserve(&index, &mut u, r.op, r.payer, r.asset, r.amount)?;
        let reserve = u.chain().first().cloned().ok_or(Refusal::NotAuthorised)?;
        index
            .apply_all(u.changes())
            .map_err(|_| Refusal::NotAuthorised)?;
        let mut u = UnitBuilder::new();
        match closing {
            None => {}
            Some(Closing::Captured {
                destination,
                cleared,
            }) => {
                let dest = self
                    .state
                    .account(&destination)
                    .ok_or(Refusal::UnknownAccount)?;
                let asset = self.state.registry().get(&r.asset)?;
                let position = Position {
                    account: destination,
                    spec: dest.sub_account(asset),
                };
                CHOSEN.capture(&index, &mut u, &r.op, position, cleared)?;
            }
            Some(Closing::Released) => CHOSEN.release(&index, &mut u, &r.op)?,
            Some(Closing::SettledOutbound) => return Err(Refusal::NotAuthorised),
        }
        let ended = u.chain().first().cloned();
        self.verify_posting(&reserve)?;
        if let Some(e) = &ended {
            self.verify_posting(e)?;
        }
        Ok((reserve, ended))
    }

    /// Whether `draft` is exactly the posting the kernel holds under its identity: the same
    /// accounts, sides and commitments, entry by entry. Anything else fails closed.
    fn verify_posting(&self, draft: &PostingDraft) -> Result<(), Refusal> {
        let id = draft.meta().transfer_id_in(draft.home());
        let mut keys = KeySet::default();
        keys.transfers.insert(id);
        let loaded = self
            .book
            .store()
            .load(&keys)
            .map_err(|_| Refusal::Internal)?;
        let stored = loaded
            .transfers
            .get(&id)
            .cloned()
            .flatten()
            .ok_or(Refusal::Internal)?;
        let built = build_transfer(self.book.keys(), self.book.journal(), draft, false).transfer;
        let same = stored.entries.len() == built.entries.len()
            && stored.entries.iter().zip(&built.entries).all(|(a, b)| {
                a.account_id == b.account_id
                    && a.direction == b.direction
                    && a.amount == b.amount
                    && a.ledger == b.ledger
            });
        // The book disagreeing with itself is a fault, never a refusal about the caller.
        if !same {
            return Err(Refusal::Internal);
        }
        Ok(())
    }
}

/// The check of a reversal's posting (amendment 0006 items 1 and 4, refined 2026-09-29;
/// `REQ-RC-5`, `REQ-PO-3`): it mirrors `original` **exactly and whole** — every entry of the
/// original, and nothing else, on the other side, for the same value, on the same account (the
/// holding position of `r` read as its payer, as [`fold`] folds it). Anything else is
/// `ERR-EvidenceInsufficient`: the reversal is refused, whoever approves it.
pub(crate) fn mirrors(
    original: &PostingDraft,
    reversal: &PostingDraft,
    r: &Reservation,
) -> Result<(), Refusal> {
    let key = |account: KernelAccountSpec, side: cipher_clerk::Direction, value: u64| {
        let account = if account == r.holding {
            r.payer.spec
        } else {
            account
        };
        (
            alloc::format!("{account:?}"),
            side == cipher_clerk::Direction::Debit,
            value,
        )
    };
    let flip = |side: cipher_clerk::Direction| match side {
        cipher_clerk::Direction::Debit => cipher_clerk::Direction::Credit,
        cipher_clerk::Direction::Credit => cipher_clerk::Direction::Debit,
    };
    let mut want: Vec<_> = original
        .entries()
        .iter()
        .map(|e| key(e.account, flip(e.side), e.value.get()))
        .collect();
    let mut got: Vec<_> = reversal
        .entries()
        .iter()
        .map(|e| key(e.account, e.side, e.value.get()))
        .collect();
    want.sort();
    got.sort();
    if want == got && reversal.meta().kind == PostingKind::Reversal {
        Ok(())
    } else {
        Err(Refusal::EvidenceInsufficient)
    }
}

/// The capture's mirror with the holding position replaced by the payer it was reserved from:
/// the mirror of the capture and of the reserve, with the two holding legs, which cancel, left
/// out. See the module documentation.
fn fold(
    capture: &PostingDraft,
    r: &Reservation,
    meta: PostingMeta,
) -> Result<PostingDraft, Refusal> {
    let payer: KernelAccountSpec = r.payer.spec;
    let entries: Vec<EntryDraft> = capture
        .entries()
        .iter()
        .map(|e| {
            let account = if e.account == r.holding {
                payer
            } else {
                e.account
            };
            match e.side {
                cipher_clerk::Direction::Debit => EntryDraft::credit(account, e.value),
                cipher_clerk::Direction::Credit => EntryDraft::debit(account, e.value),
            }
        })
        .collect();
    // It balances because the capture did: every value is kept, on the other side.
    PostingDraft::balanced(meta, entries).map_err(|_| Refusal::NotAuthorised)
}
