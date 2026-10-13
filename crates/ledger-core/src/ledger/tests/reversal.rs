// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of the reversal pair (`F-013` T-013-05), over the real kernel. The acceptance
//! cases run in the conformance suite on both stores; these add what needs the book's insides:
//! the kernel's own refusal of a second reversal, and the stored original. A reversal is an
//! operation of its own (amendment 0006 item 1): the reversed operation stays `Settled`.

use super::operator::staff;
use super::resolution::{at, available, funded_in, in_review};
use super::*;
use crate::ledger::lifecycle::{EvidenceFacts, FireInput};
use crate::lifecycle::{KindClass, State as OpState, Trigger};
use crate::operator::{IncidentKind, OperatorOpKind};
use ledger_kernel::book::{EventClass, Step, Unit};
use ledger_kernel::posting::{PostingKind, PostingMeta};
use ledger_kernel::store::{KeySet, LeafStore};

struct Captured {
    alice: AccountId,
    merchant: AccountId,
    merchant_owner: Caller,
    op: OperationId,
    capture_seq: u64,
}

/// Alice (100, owned) reserves 50 and captures 30 into a merchant account in delegated custody.
fn captured(fx: &mut Fx) -> Captured {
    let (alice_who, alice) = funded_in(fx, 100, Custody::Owned);
    let merchant_owner = fx.person();
    let merchant = fx.account(&merchant_owner, Custody::Delegated);
    let r = fx.ctx(alice_who);
    fx.ledger.reserve(&r, alice, usd(), Minor::new(50)).unwrap();
    let capture_seq = fx
        .ledger
        .capture(&at(r, 12), merchant, Minor::new(30))
        .unwrap()
        .value;
    Captured {
        alice,
        merchant,
        merchant_owner,
        op: r.op,
        capture_seq,
    }
}

#[test]
fn ac_k2_1_a_proposal_raises_an_incident_and_needs_a_second_principal() {
    let mut fx = Fx::new();
    let c = captured(&mut fx);
    let both = staff(
        &mut fx,
        &[
            OperatorOpKind::ProposeReversal,
            OperatorOpKind::ApproveReversal,
        ],
    );
    let ctx = fx.ctx(both);
    let proposal = fx
        .ledger
        .propose_reversal(&ctx, c.op, b"chargeback 88")
        .unwrap()
        .value;
    assert!(
        fx.ledger
            .state()
            .ops()
            .incidents()
            .any(|i| { i.kind == IncidentKind::ReversalProposed && i.op == Some(c.op) })
    );
    let pending = fx.ledger.state().ops().reversal(&proposal).unwrap().clone();
    assert_eq!(pending.reverses, vec![c.capture_seq]);
    assert!(pending.approval.is_none());
    // The proposer cannot approve.
    let ctx = fx.ctx(both);
    assert_eq!(
        fx.ledger
            .approve_reversal(&ctx, proposal, b"approving my own")
            .map(|a| a.value),
        Err(Refusal::ApprovalRequired)
    );
    // The reversal is an operation of its own, in `Accepted` with a deadline to be approved by.
    let rv = fx.ledger.operation(&proposal).unwrap().clone();
    assert_eq!(rv.state(), OpState::Accepted);
    assert_eq!(rv.kind(), OperationKind::Reversal);
    assert_eq!(rv.class(), KindClass::Reversal);
    let due = rv.lifecycle().deadline().unwrap();
    // No timer, tick or observation approves it.
    fx.ledger.tick(due - 1).unwrap();
    assert_eq!(
        fx.ledger.operation(&c.op).unwrap().state(),
        OpState::Settled
    );
    assert_eq!(
        fx.ledger.operation(&proposal).unwrap().state(),
        OpState::Accepted
    );
    let input = FireInput {
        facts: EvidenceFacts {
            justified: true,
            ..EvidenceFacts::default()
        },
        ..FireInput::default()
    };
    let unknown = at(fx.ctx(both), 20);
    assert_eq!(
        fx.ledger
            .fire(&unknown, Trigger::ApproveReversal, input)
            .map(|_| ()),
        Err(Refusal::UnknownOperation)
    );
    let mut on_op = fx.ctx(both);
    on_op.op = proposal;
    let input = FireInput {
        facts: EvidenceFacts {
            justified: true,
            ..EvidenceFacts::default()
        },
        ..FireInput::default()
    };
    assert_eq!(
        fx.ledger
            .fire(&on_op, Trigger::ApproveReversal, input)
            .map(|_| ()),
        Err(Refusal::ApprovalRequired)
    );
    assert_eq!(
        fx.ledger.operation(&c.op).unwrap().state(),
        OpState::Settled
    );
    // The reversed operation takes no trigger at all: it is terminal (`INV-21`).
    let mut on_settled = fx.ctx(both);
    on_settled.op = c.op;
    assert_eq!(
        fx.ledger
            .fire(&on_settled, Trigger::ApproveReversal, FireInput::default())
            .map(|_| ()),
        Err(Refusal::OperationTerminal)
    );
    // Without the approval kind, a second principal cannot approve either.
    let other = staff(&mut fx, &[OperatorOpKind::ProposeReversal]);
    let ctx = fx.ctx(other);
    assert_eq!(
        fx.ledger
            .approve_reversal(&ctx, proposal, b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn ac_k2_2_a_reversal_is_a_new_posting_and_a_second_one_fails_at_the_kernel() {
    let mut fx = Fx::new();
    let c = captured(&mut fx);
    assert_eq!(available(&fx, &c.alice), 70);
    assert_eq!(available(&fx, &c.merchant), 30);
    let proposer = staff(&mut fx, &[OperatorOpKind::ProposeReversal]);
    let approver = staff(&mut fx, &[OperatorOpKind::ApproveReversal]);
    // The capture debits Alice's holding, which is in her partition (`REQ-RT-9`).
    let capture_id = PostingMeta::new(PostingKind::Capture, c.op.to_ref(), 0)
        .transfer_id_in(ledger_kernel::partition::Partition::of(c.alice.as_bytes()));
    let stored = |fx: &Fx| {
        let mut keys = KeySet::default();
        keys.transfers.insert(capture_id);
        fx.ledger.book.store().load(&keys).unwrap().transfers[&capture_id].clone()
    };
    let before = stored(&fx);
    // What the approval will post, taken now, while the operation is settled.
    let o = fx.ledger.operation(&c.op).unwrap().clone();
    let reversal = fx.ledger.reversal_of(&o).unwrap().draft;
    let ctx = fx.ctx(proposer);
    let proposal = fx
        .ledger
        .propose_reversal(&ctx, c.op, b"chargeback 88")
        .unwrap()
        .value;
    let ctx = fx.ctx(approver);
    let done = fx
        .ledger
        .approve_reversal(&ctx, proposal, b"chargeback 88 confirmed")
        .unwrap();
    // The reversed operation stays `Settled`; the reversal operation settled.
    assert_eq!(
        fx.ledger.operation(&c.op).unwrap().state(),
        OpState::Settled
    );
    assert_eq!(
        fx.ledger.operation(&proposal).unwrap().state(),
        OpState::Settled
    );
    // The payer has its value back, the merchant has none, and the book still balances.
    assert_eq!(available(&fx, &c.alice), 100);
    assert_eq!(available(&fx, &c.merchant), 0);
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
    assert_eq!(fx.ledger.check_holdings(), Ok(()));
    // The original posting is unchanged; the reversal is a new posting that references it.
    assert_eq!(stored(&fx), before);
    let t = fx
        .ledger
        .operation(&proposal)
        .unwrap()
        .transitions()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (t.from, t.to, t.trigger),
        (
            OpState::Accepted,
            OpState::Settled,
            Trigger::ApproveReversal
        )
    );
    assert_eq!(t.posting, Some(PostingKind::Reversal));
    assert_eq!(t.postings, vec![done.value]);
    assert!(done.value > c.capture_seq);
    assert_eq!(t.authorities.len(), 2);
    let approved = fx.ledger.state().ops().reversal(&proposal).unwrap().clone();
    assert_eq!(approved.reverses, vec![c.capture_seq]);
    assert_eq!(approved.approval.unwrap().0.principal, approver.principal);
    // The merchant's authority holders see the operator's action.
    let actions = fx
        .ledger
        .account_actions(&c.merchant_owner, &c.merchant, 50)
        .unwrap();
    assert!(
        actions
            .iter()
            .any(|r| r.command == Command::ApproveReversal)
    );
    // A second reversal of the operation (`REQ-PO-3`): refused at the service ...
    let ctx = fx.ctx(proposer);
    assert_eq!(
        fx.ledger
            .propose_reversal(&ctx, c.op, b"again")
            .map(|a| a.value),
        Err(Refusal::InvalidTransition)
    );
    // ... and at the kernel: the same reversal posting again is refused for ever.
    let applied = fx
        .ledger
        .book
        .apply(
            &Unit {
                steps: vec![Step {
                    creations: Vec::new(),
                    chain: vec![reversal],
                }],
            },
            60,
        )
        .unwrap();
    assert_eq!(applied.steps[0].events, vec![EventClass::Duplicate]);
    assert_eq!(available(&fx, &c.alice), 100);
}

/// Amendment 0006 items 1 and 4: only a `Settled` operation is reversed. An operation in
/// `Needs-Review` is resolved to `Settled` or `Released`, never reversed from there; an inbound
/// arrival there has nothing posted either.
#[test]
fn req_po_3_only_a_settled_operation_is_reversed() {
    let mut fx = Fx::new();
    let (alice, acct) = funded_in(&mut fx, 100, Custody::Delegated);
    let op = in_review(&mut fx, alice, acct, 30);
    let proposer = staff(&mut fx, &[OperatorOpKind::ProposeReversal]);
    let ctx = at(fx.ctx(proposer), 300);
    assert_eq!(
        fx.ledger
            .propose_reversal(&ctx, op, b"return the reservation")
            .map(|a| a.value),
        Err(Refusal::InvalidTransition)
    );
    assert_eq!(
        fx.ledger.operation(&op).unwrap().state(),
        OpState::NeedsReview
    );
    assert_eq!(fx.ledger.balance_view(&acct, &usd()).unwrap().reserved, 30);
    let inbound = crate::lifecycle::keys::inbound_reference(b"coordinate 1");
    fx.ledger
        .arrive(inbound, OperationKind::Issue, Some(400), 300)
        .unwrap();
    fx.ledger.tick(500).unwrap();
    let ctx = at(fx.ctx(proposer), 500);
    assert_eq!(
        fx.ledger
            .propose_reversal(&ctx, inbound, b"x")
            .map(|a| a.value),
        Err(Refusal::InvalidTransition)
    );
}

#[test]
fn req_au_6_no_reversal_debits_an_owned_account_or_a_spent_one() {
    let mut fx = Fx::new();
    // The destination is in owned custody: the operator cannot take its value.
    let (alice_who, alice) = funded_in(&mut fx, 100, Custody::Owned);
    let shop_owner = fx.person();
    let shop = fx.account(&shop_owner, Custody::Owned);
    let r = fx.ctx(alice_who);
    fx.ledger.reserve(&r, alice, usd(), Minor::new(20)).unwrap();
    fx.ledger.capture(&at(r, 12), shop, Minor::new(20)).unwrap();
    let proposer = staff(&mut fx, &[OperatorOpKind::ProposeReversal]);
    let approver = staff(&mut fx, &[OperatorOpKind::ApproveReversal]);
    let ctx = fx.ctx(proposer);
    assert_eq!(
        fx.ledger
            .propose_reversal(&ctx, r.op, b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // The destination spent the value before the approval: the kernel refuses the reversal,
    // nothing moves, and the proposal stays pending.
    let c = captured(&mut fx);
    let ctx = fx.ctx(proposer);
    let proposal = fx
        .ledger
        .propose_reversal(&ctx, c.op, b"chargeback")
        .unwrap()
        .value;
    let spend = fx.ctx(c.merchant_owner);
    fx.ledger
        .reserve(&spend, c.merchant, usd(), Minor::new(25))
        .unwrap();
    let ctx = fx.ctx(approver);
    assert_eq!(
        fx.ledger
            .approve_reversal(&ctx, proposal, b"approved")
            .map(|a| a.value),
        Err(Refusal::InsufficientAvailable)
    );
    assert_eq!(
        fx.ledger.operation(&c.op).unwrap().state(),
        OpState::Settled
    );
    assert!(
        fx.ledger
            .state()
            .ops()
            .reversal(&proposal)
            .unwrap()
            .approval
            .is_none()
    );
    // Once the value is back, the same proposal can be approved.
    fx.ledger.release(&at(spend, 13)).unwrap();
    let ctx = fx.ctx(approver);
    fx.ledger
        .approve_reversal(&ctx, proposal, b"approved")
        .unwrap();
    assert_eq!(
        fx.ledger.operation(&c.op).unwrap().state(),
        OpState::Settled
    );
    assert_eq!(
        fx.ledger.operation(&proposal).unwrap().state(),
        OpState::Settled
    );
}

/// `REQ-RC-5`, `REQ-PO-3`, `AC-K2.3` (amendment 0006 items 1 and 4, refined 2026-09-29): the check a
/// reversal must pass, which no approver overrides. Its posting mirrors the reversed one exactly
/// and whole; a partial or altered mirror is refused.
#[test]
fn ac_k2_3_a_reversal_that_does_not_mirror_the_original_whole_is_refused() {
    use crate::ledger::operator::reversal::mirrors;
    use crate::reservation::Status;
    use ledger_kernel::posting::{EntryDraft, PostingDraft};
    let mut fx = Fx::new();
    let c = captured(&mut fx);
    let o = fx.ledger.operation(&c.op).unwrap().clone();
    let good = fx.ledger.reversal_of(&o).unwrap().draft;
    let Some(Status::Closed(r, closing)) = fx.ledger.state().reservations().status(&c.op) else {
        panic!("the reservation was captured");
    };
    let (_, capture) = fx.ledger.rebuilt(&r, Some(closing)).unwrap();
    let capture = capture.unwrap();
    assert_eq!(mirrors(&capture, &good, &r), Ok(()));
    // A partial reversal: every value halved. It still balances; it is not whole.
    let half: Vec<EntryDraft> = good
        .entries()
        .iter()
        .map(|e| EntryDraft {
            value: core::num::NonZeroU64::new((e.value.get() / 2).max(1)).unwrap(),
            ..*e
        })
        .collect();
    let partial = PostingDraft::balanced(*good.meta(), half).unwrap();
    assert_eq!(
        mirrors(&capture, &partial, &r),
        Err(Refusal::EvidenceInsufficient)
    );
    // The original itself, not its mirror.
    assert_eq!(
        mirrors(&capture, &capture, &r),
        Err(Refusal::EvidenceInsufficient)
    );
}
