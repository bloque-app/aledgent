// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of the lifecycle engine over the real kernel (`F-007`, T-007-05 to T-007-08,
//! T-007-10). The service-layer acceptance, error and edge cases run in the conformance suite
//! (`crates/conformance/src/cases/f007.rs`) on both stores; these cover what needs the engine's
//! insides: fault injection, the posting sequence, notifications, and the tick's bounds.

use super::*;
use crate::call::Call;
use crate::ledger::lifecycle::{EvidenceFacts, FaultAt, FireInput, Fired, Resumed, ReviewReason};
use crate::lifecycle::keys::{
    ClientKey, Evidence, NotificationId, Surface, derive_client_key, operation_reference,
};
use crate::lifecycle::store::Operation;
use crate::lifecycle::{KindClass, Shown, State as OpState, StateClass, Trigger};
use crate::reservation::Status;
use ledger_kernel::book::{EventClass, Step, Unit};
use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};

fn at(ctx: Ctx, now: Time) -> Ctx {
    Ctx { now, ..ctx }
}

fn on(caller: Caller, op: OperationId, now: Time) -> Ctx {
    Ctx { caller, op, now }
}

fn op_of(fx: &Fx, op: &OperationId) -> Operation {
    fx.ledger
        .operation(op)
        .cloned()
        .expect("the operation exists")
}

/// Everything a refused command must leave as it was: the book, the operation store, the
/// reservation index, the attribution records and the outbox.
fn fingerprint(fx: &Fx) -> (Vec<Operation>, Vec<Status>, usize, [u8; 32], u64) {
    (
        fx.ledger.state().operations().all().cloned().collect(),
        fx.ledger.state().reservations().iter().copied().collect(),
        fx.ledger.state().operations().outbox().count(),
        fx.ledger.book().root().unwrap(),
        fx.ledger.book().store().next_posting_seq().unwrap(),
    )
}

fn outbound(fx: &mut Fx, owner: Caller, payer: AccountId, amount: u64, deadline: Time) -> Ctx {
    let ctx = fx.ctx(owner);
    fx.ledger
        .receive(&ctx, OperationKind::Redeem, KindClass::Outbound)
        .unwrap();
    let fired = fx
        .ledger
        .fire(
            &ctx,
            Trigger::Decide,
            FireInput {
                deadline: Some(deadline),
                ..FireInput::default()
            },
        )
        .unwrap();
    assert_eq!(
        fired,
        Fired::Moved {
            to: OpState::Accepted,
            posting: None
        }
    );
    let fired = fx
        .ledger
        .fire(
            &ctx,
            Trigger::PostReservation,
            FireInput {
                reserve: Some((payer, usd(), nz(amount))),
                ..FireInput::default()
            },
        )
        .unwrap();
    assert!(matches!(
        fired,
        Fired::Moved {
            to: OpState::Reserved,
            posting: Some(_)
        }
    ));
    ctx
}

// ---------------------------------------------------------------- T-007-05: the two keys

/// AC-A1.3: a repeated client key returns the same account, not a second one; the same key with
/// another body is `ERR-ClientKeyReused`.
#[test]
fn ac_a1_3_a_repeated_client_key_returns_the_same_account() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let key = ClientKey::new(*b"open-my-account");
    let deployment = fx.ledger.config().deployment;
    let op = operation_reference(
        &deployment,
        &fx.ledger.state().key_domain(&alice.principal),
        Surface::Command,
        &key,
    );
    let req = OpenAccount {
        owner: alice.principal,
        kind: AccountKind::Customer,
        custody: Custody::Owned,
        treasury_limits: BTreeMap::new(),
        acknowledged: fx.ack(&alice, Custody::Owned),
    };
    let first = fx
        .ledger
        .open_account(&on(alice, op, 10), req.clone())
        .unwrap();
    let count = fx.ledger.state().all_accounts().count();
    // Later, the same key again: the same account, and nothing new.
    let again = fx
        .ledger
        .open_account(&on(alice, op, 99), req.clone())
        .unwrap();
    assert_eq!(again.value, first.value);
    assert_eq!(again, first);
    assert_eq!(fx.ledger.state().all_accounts().count(), count);
    let changed = OpenAccount {
        custody: Custody::Delegated,
        ..req
    };
    assert_eq!(
        fx.ledger
            .open_account(&on(alice, op, 99), changed)
            .map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
}

/// REQ-OP-11: no time value enters any key. A request without a client key derives one from the
/// request alone; retried at another time, it derives the same key, the same reference, and the
/// original outcome.
#[test]
fn req_op_11_no_time_value_enters_any_key() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let p = crate::principal::resolve(fx.ledger.state(), &alice).unwrap();
    let digest = super::super::requests::reserve(&p, &acct, &usd(), Minor::new(30), None);
    let key = derive_client_key(Surface::Command, &digest);
    let deployment = fx.ledger.config().deployment;
    let op = operation_reference(
        &deployment,
        &fx.ledger.state().key_domain(&alice.principal),
        Surface::Command,
        &key,
    );
    let first = fx
        .ledger
        .reserve(&on(alice, op, 10), acct, usd(), Minor::new(30))
        .unwrap();
    let before = fingerprint(&fx);
    let later = fx
        .ledger
        .reserve(&on(alice, op, 700), acct, usd(), Minor::new(30))
        .unwrap();
    assert_eq!(later, first);
    assert_eq!(fingerprint(&fx), before);
    // The reply of the original outcome is the one given then, labelled non-terminal.
    assert_eq!(
        first.status.map(|s| s.class),
        Some(StateClass::OpenNonRefunding)
    );
    // Recomputed later, the key is the same: the derivation takes no time.
    assert_eq!(derive_client_key(Surface::Command, &digest), key);
    // Another amount under the same reference is another request.
    assert_eq!(
        fx.ledger
            .reserve(&on(alice, op, 700), acct, usd(), Minor::new(31))
            .map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
}

/// AC-C2.1, second half: a repeated capture is a no-op that returns the original outcome. A
/// repeated release too. A different capture of an ended reservation is terminal.
#[test]
fn ac_c2_1_a_repeated_capture_returns_the_original_outcome() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let (_, dest) = funded(&mut fx, 1);
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve(&reserve, acct, usd(), Minor::new(40))
        .unwrap();
    let capture = on(alice, reserve.op, 20);
    let first = fx.ledger.capture(&capture, dest, Minor::new(25)).unwrap();
    assert_eq!(first.status.map(|s| s.state), Some(OpState::Settled));
    let before = fingerprint(&fx);
    let again = fx
        .ledger
        .capture(&at(capture, 30), dest, Minor::new(25))
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(fingerprint(&fx), before);
    assert_eq!(
        fx.ledger
            .capture(&capture, dest, Minor::new(24))
            .map(|a| a.value),
        Err(Refusal::OperationTerminal)
    );
    assert_eq!(
        fx.ledger.release(&capture).map(|a| a.value),
        Err(Refusal::OperationTerminal)
    );
    // Release: the same.
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve(&reserve, acct, usd(), Minor::new(10))
        .unwrap();
    let release = on(alice, reserve.op, 40);
    let first = fx.ledger.release(&release).unwrap();
    let before = fingerprint(&fx);
    assert_eq!(fx.ledger.release(&release).unwrap(), first);
    assert_eq!(fingerprint(&fx), before);
}

// ---------------------------------------------------------------- T-007-06: one commit

/// A fault injected between any two writes, or before the kernel step, leaves nothing committed:
/// not the book, not the service store.
#[test]
fn a_fault_between_any_two_writes_leaves_nothing_committed() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let (_, dest) = funded(&mut fx, 1);
    // A reserve: every write of its batch, then before the kernel.
    let mut faults: Vec<FaultAt> = (0..64).map(FaultAt::Write).collect();
    faults.push(FaultAt::BeforeKernel);
    let mut injected = 0;
    for fault in faults.iter().copied() {
        let before = fingerprint(&fx);
        let ctx = fx.ctx(alice);
        fx.ledger.inject(fault);
        match fx.ledger.reserve(&ctx, acct, usd(), Minor::new(10)) {
            Err(_) => {
                injected += 1;
                assert_eq!(fingerprint(&fx), before, "{fault:?}");
                assert!(fx.ledger.operation(&ctx.op).is_none());
            }
            // Past the end of the batch the fault never fires: the reserve commits whole. Take
            // it back so the next fault starts from the same book.
            Ok(_) => {
                fx.ledger.release(&on(alice, ctx.op, 10)).unwrap();
            }
        }
        fx.ledger.fault = None;
    }
    // Nine writes (no window charge; the index change, the record, the opening, two
    // notifications, two transitions, the client-key entry, and the posting's plaintext of
    // `F-011`): a fault before each, one after the last, and one before the kernel.
    assert_eq!(injected, 11, "{injected}");
    // A capture, the same way.
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve(&reserve, acct, usd(), Minor::new(10))
        .unwrap();
    for fault in faults {
        let before = fingerprint(&fx);
        fx.ledger.inject(fault);
        let r = fx
            .ledger
            .capture(&on(alice, reserve.op, 10), dest, Minor::new(5));
        if r.is_ok() {
            break;
        }
        assert_eq!(fingerprint(&fx), before, "{fault:?}");
        assert_eq!(op_of(&fx, &reserve.op).state(), OpState::Reserved);
        fx.ledger.fault = None;
    }
    assert_eq!(op_of(&fx, &reserve.op).state(), OpState::Settled);
}

/// The kernel refuses a replayed posting (REQ-KER-6): the posting identity is
/// `(operation, kind, ordinal)`, and a second copy is a duplicate for ever.
#[test]
fn req_ker_6_the_kernel_refuses_a_replayed_posting() {
    let mut fx = Fx::new();
    let (_, a) = funded(&mut fx, 100);
    let (_, b) = funded(&mut fx, 1);
    let op = fx.op();
    let asset = *fx.ledger.state().registry().get(&usd()).unwrap();
    let spec = |id: &AccountId| fx.ledger.state().account(id).unwrap().sub_account(&asset);
    let draft = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Transfer, op.to_ref(), 0),
        vec![
            EntryDraft::debit(spec(&a), nz(3).get()),
            EntryDraft::credit(spec(&b), nz(3).get()),
        ],
    )
    .unwrap();
    let unit = Unit {
        steps: vec![Step {
            creations: Vec::new(),
            chain: vec![draft],
        }],
    };
    let first = fx.ledger.book.apply(&unit, 10).unwrap();
    assert!(first.steps[0].applied);
    let again = fx.ledger.book.apply(&unit, 11).unwrap();
    assert!(!again.steps[0].applied);
    assert_eq!(again.steps[0].events, vec![EventClass::Duplicate]);
}

/// Postings take the global, gapless sequence (REQ-PO-2); each transition's audit record names
/// its posting; each notification's identity is `H(op, key)` with no time in it (REQ-EV-3).
#[test]
fn postings_notifications_and_audit_records_commit_with_the_transition() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let (_, dest) = funded(&mut fx, 1);
    let next = fx.ledger.book().store().next_posting_seq().unwrap();
    let reserve = fx.ctx(alice);
    let r = fx
        .ledger
        .reserve(&reserve, acct, usd(), Minor::new(40))
        .unwrap();
    assert_eq!(r.value, next);
    assert_eq!(r.record.postings, vec![next]);
    let c = fx
        .ledger
        .capture(&on(alice, reserve.op, 11), dest, Minor::new(40))
        .unwrap();
    assert_eq!(c.value, next + 1);
    let o = op_of(&fx, &reserve.op);
    let pairs: Vec<(OpState, OpState, Option<PostingKind>, Vec<u64>)> = o
        .transitions()
        .iter()
        .map(|t| (t.from, t.to, t.posting, t.postings.clone()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            (OpState::Received, OpState::Accepted, None, vec![]),
            (
                OpState::Accepted,
                OpState::Reserved,
                Some(PostingKind::Reserve),
                vec![next]
            ),
            (
                OpState::Reserved,
                OpState::Settled,
                Some(PostingKind::Capture),
                vec![next + 1]
            ),
        ]
    );
    for t in o.transitions() {
        let n = fx
            .ledger
            .state()
            .operations()
            .notification(&NotificationId::derive(&reserve.op, &t.key))
            .expect("one notification per transition");
        assert_eq!(n.shown.state, t.to);
        assert_eq!(n.outcome, t.to.is_terminal().then_some(t.to));
        assert!(!t.authorities.is_empty(), "attributed");
    }
    assert!(o.lifecycle().closed_at().is_some());
}

// ---------------------------------------------------------------- T-007-07: deadlines

/// AC-C4.1: a reservation past its deadline appears in the review queue, with an owner and a
/// service level: it moved to `Needs-Review` (amendment 0006 item 5). AC-C4.2: the age of the
/// oldest open operation is observable.
#[test]
fn ac_c4_1_and_c4_2_the_review_queue_and_the_oldest_age() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    assert_eq!(fx.ledger.oldest_open_age(10), None);
    let reserve = at(fx.ctx(alice), 20);
    fx.ledger
        .reserve_until(&reserve, acct, usd(), Minor::new(10), Some(50))
        .unwrap();
    assert_eq!(fx.ledger.oldest_open_age(70), Some(50));
    assert!(fx.ledger.review_queue(49).is_empty());
    let t = fx.ledger.tick(60).unwrap();
    assert!(t.queued.is_empty());
    assert_eq!(t.moved, vec![(reserve.op, OpState::NeedsReview)]);
    let q = fx.ledger.review_queue(60);
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].op, reserve.op);
    assert_eq!(q[0].owner, fx.founded.operator);
    assert_eq!(
        q[0].resolve_by,
        50 + fx.ledger.config().review_service_level
    );
    assert_eq!(q[0].reason, ReviewReason::NeedsReview);
    assert_eq!(q[0].shown.class, StateClass::AwaitingHuman);
    // Silence never releases (REQ-OP-6, REQ-OP-9): the value is still reserved.
    assert_eq!(op_of(&fx, &reserve.op).state(), OpState::NeedsReview);
    // Past the review service level it stays, and escalates.
    let later = 50 + fx.ledger.config().review_service_level + 1;
    let t = fx.ledger.tick(later).unwrap();
    assert_eq!(t.queued, vec![reserve.op]);
    assert_eq!(
        fx.ledger.review_queue(later)[0].reason,
        ReviewReason::NeedsReview
    );
    assert_eq!(
        fx.ledger
            .balances(&alice, &acct, &usd(), 60)
            .unwrap()
            .reserved,
        10
    );
}

/// A tick with an older `now` changes nothing; repeated ticks give one transition key; one tick
/// fires at most `max_fires_per_tick` deadlines.
#[test]
fn ticks_are_monotonic_idempotent_and_bounded() {
    let mut fx = Fx::new();
    fx.ledger.config.max_fires_per_tick = 2;
    let (alice, acct) = funded(&mut fx, 100);
    let mut ops = Vec::new();
    for _ in 0..3 {
        ops.push(outbound(&mut fx, alice, acct, 10, 50).op);
    }
    for op in &ops {
        let r = fx
            .ledger
            .fire(
                &on(alice, *op, 10),
                Trigger::HandOffAcknowledged,
                FireInput::default(),
            )
            .unwrap();
        assert!(matches!(
            r,
            Fired::Moved {
                to: OpState::Submitted,
                ..
            }
        ));
    }
    let t = fx.ledger.tick(100).unwrap();
    assert_eq!(t.moved.len(), 2);
    assert!(t.more);
    let before = fingerprint(&fx);
    let older = fx.ledger.tick(90).unwrap();
    assert!(older.ignored);
    assert_eq!(fingerprint(&fx), before);
    let t = fx.ledger.tick(100).unwrap();
    assert_eq!(t.moved.len(), 1);
    assert!(!t.more);
    // Everything is in Needs-Review now, by one transition each, keyed on the deadline.
    for op in &ops {
        let o = op_of(&fx, op);
        assert_eq!(o.state(), OpState::NeedsReview);
        let last = o.transitions().last().unwrap();
        assert_eq!(last.evidence, Evidence::deadline(50));
        // The review's service level runs from the deadline that passed, not the tick's time.
        assert_eq!(
            o.lifecycle().deadline(),
            Some(50 + fx.ledger.config().review_service_level)
        );
    }
    let before = fingerprint(&fx);
    let t = fx.ledger.tick(200).unwrap();
    assert!(t.moved.is_empty());
    assert_eq!(fingerprint(&fx).0.len(), before.0.len());
    // In the queue, awaiting a human, value still reserved.
    let q = fx.ledger.review_queue(200);
    assert_eq!(q.len(), 3);
    assert!(q.iter().all(|e| e.shown.class == StateClass::AwaitingHuman));
}

/// Finding F18 of `F-032`, `REQ-RT-1`, `DEC-6`: a tick chooses its due deadlines on the whole
/// store and records them in its call, at most `max_fires_per_tick`; the replay (`tick_due`, the
/// core Agent's) takes only entries of the deadline index, due at `now`, in strictly ascending
/// order, within the bound, and anything else changes nothing (`ERR-Internal`).
#[test]
fn req_rt_1_a_tick_records_its_due_deadlines_and_replays_only_those() {
    let mut fx = Fx::new();
    fx.ledger.config.max_fires_per_tick = 2;
    let (alice, acct) = funded(&mut fx, 100);
    let mut ops = Vec::new();
    for d in [50, 60, 70] {
        let c = at(fx.ctx(alice), 20);
        fx.ledger
            .reserve_until(&c, acct, usd(), Minor::new(10), Some(d))
            .unwrap();
        ops.push(c.op);
    }
    let before = fingerprint(&fx);
    for (now, bad) in [
        (100, vec![(60, ops[1]), (50, ops[0])]),
        (100, vec![(50, ops[0]), (50, ops[0])]),
        (100, vec![(51, ops[0])]),
        (65, vec![(70, ops[2])]),
        (100, vec![(50, ops[0]), (60, ops[1]), (70, ops[2])]),
    ] {
        assert_eq!(
            fx.ledger.tick_due(now, bad.clone()).unwrap_err(),
            Refusal::Internal,
            "{bad:?} at {now}"
        );
        assert_eq!(fingerprint(&fx), before);
    }
    // The tick records what it chose: the two earliest, of three due.
    fx.ledger.record_calls(true);
    let t = fx.ledger.tick(100).unwrap();
    assert!(t.more);
    let calls = fx.ledger.take_calls();
    fx.ledger.record_calls(false);
    assert!(matches!(
        &calls[..],
        [r] if matches!(&r.call, Call::Tick { now: 100, due } if *due == vec![(50, ops[0]), (60, ops[1])])
    ));
    // A replay of a choice that is due and in the index is a tick over exactly those.
    let t = fx.ledger.tick_due(100, vec![(70, ops[2])]).unwrap();
    assert_eq!(t.moved, vec![(ops[2], OpState::NeedsReview)]);
}

/// ERR-DeadlineRequired: an operation of a kind with no default, decided without a deadline.
#[test]
fn err_deadline_required_without_a_deadline_or_a_default() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let ctx = fx.ctx(alice);
    fx.ledger
        .receive(&ctx, OperationKind::Transfer, KindClass::NoReservation)
        .unwrap();
    assert_eq!(
        fx.ledger.fire(&ctx, Trigger::Decide, FireInput::default()),
        Err(Refusal::DeadlineRequired)
    );
    assert_eq!(op_of(&fx, &ctx.op).state(), OpState::Received);
    // With a deadline it is decided.
    let r = fx.ledger.fire(
        &ctx,
        Trigger::Decide,
        FireInput {
            deadline: Some(99),
            ..FireInput::default()
        },
    );
    assert!(matches!(
        r,
        Ok(Fired::Moved {
            to: OpState::Accepted,
            ..
        })
    ));
    // A received command past its decision bound is an incident, and moves to Needs-Review
    // (amendment 0006 item 5: the bound is `Received`'s deadline).
    let received = fx.ctx(alice);
    fx.ledger
        .receive(&received, OperationKind::Transfer, KindClass::NoReservation)
        .unwrap();
    let t = fx
        .ledger
        .tick(10 + fx.ledger.config().decision_bound)
        .unwrap();
    assert_eq!(t.incidents, vec![received.op]);
    assert_eq!(t.moved, vec![(received.op, OpState::NeedsReview)]);
    assert_eq!(op_of(&fx, &received.op).state(), OpState::NeedsReview);
}

// ---------------------------------------------------------------- T-007-08: resume and INV-23

/// INV-23: a kernel refusal injected at `Final → Settled` leaves the record open and queued.
#[test]
fn inv_23_a_kernel_refusal_at_final_leaves_the_record_open() {
    let mut fx = Fx::new();
    let (_, empty) = funded(&mut fx, 1);
    let (_, dest) = funded(&mut fx, 1);
    let op = crate::lifecycle::keys::inbound_reference(b"chain/block-7/event-2");
    fx.ledger
        .arrive(op, OperationKind::Issue, Some(500), 10)
        .unwrap();
    let evidence = Some(Evidence([7; 32]));
    let fin = fx
        .ledger
        .fire(
            &on(fx.operator(), op, 20),
            Trigger::FinalityAndCanonicalMatch,
            FireInput {
                evidence,
                facts: EvidenceFacts {
                    finality_passed: true,
                    canonical_match: true,
                    ..EvidenceFacts::default()
                },
                ..FireInput::default()
            },
        )
        .unwrap();
    assert!(matches!(
        fin,
        Fired::Moved {
            to: OpState::Final,
            ..
        }
    ));
    // An `Issue` posting the kernel refuses: it overdraws a customer that holds 1.
    let asset = *fx.ledger.state().registry().get(&usd()).unwrap();
    let spec = |id: &AccountId| fx.ledger.state().account(id).unwrap().sub_account(&asset);
    let draft = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Issue, op.to_ref(), 0),
        vec![
            EntryDraft::debit(spec(&empty), nz(50).get()),
            EntryDraft::credit(spec(&dest), nz(50).get()),
        ],
    )
    .unwrap();
    let before = fx.ledger.book().root().unwrap();
    let r = fx
        .ledger
        .fire(
            &on(fx.operator(), op, 30),
            Trigger::AppendSettling,
            FireInput {
                evidence: Some(Evidence([8; 32])),
                posting: Some(draft),
                ..FireInput::default()
            },
        )
        .unwrap();
    assert_eq!(r, Fired::KernelRefused(Refusal::InsufficientAvailable));
    let o = op_of(&fx, &op);
    assert_eq!(o.state(), OpState::Final);
    assert!(o.lifecycle().is_open());
    assert!(o.flagged());
    assert_eq!(fx.ledger.book().root().unwrap(), before);
    assert!(fx.ledger.review_queue(30).iter().any(|e| e.op == op));
}

/// `resume` re-evaluates the forward rows of an operation with the facts aledgent holds, never
/// adds an edge, and is the operator's with a justification. (Past its deadline, `tick` moves
/// every non-terminal operation to `Needs-Review`, amendment 0006 item 5; `resume` is the
/// operator's forward path before that.)
#[test]
fn resume_moves_only_along_the_table_and_is_the_operators() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let out = outbound(&mut fx, alice, acct, 30, 50);
    // At Final, with finality established (F-010's evidence).
    for (trigger, facts) in [
        (Trigger::HandOffAcknowledged, EvidenceFacts::default()),
        (Trigger::ProvisionalObservation, EvidenceFacts::default()),
        (
            Trigger::FinalityAndCanonicalMatch,
            EvidenceFacts {
                finality_passed: true,
                canonical_match: true,
                ..EvidenceFacts::default()
            },
        ),
    ] {
        fx.ledger
            .fire(
                &on(alice, out.op, 20),
                trigger,
                FireInput {
                    facts,
                    ..FireInput::default()
                },
            )
            .unwrap();
    }
    assert_eq!(op_of(&fx, &out.op).state(), OpState::Final);
    // Not the holder; not without a justification. The operator's staff holds the `Resume` kind
    // on the operator surface (`F-013`).
    let ctx = on(alice, fx.op(), 70);
    assert_eq!(
        fx.ledger.resume(&ctx, out.op, b"stuck at final"),
        Err(Refusal::NotAuthorised)
    );
    let operator = fx.operator();
    super::operator::grant_kinds(
        &mut fx,
        operator.principal,
        &[crate::operator::OperatorOpKind::Resume],
    );
    let ctx = on(fx.operator(), fx.op(), 70);
    assert_eq!(
        fx.ledger.resume(&ctx, out.op, b""),
        Err(Refusal::JustificationRequired)
    );
    let ctx = on(fx.operator(), fx.op(), 70);
    // The operator resumes: the settling posting (Redeem) is appended, Final → Settled.
    assert_eq!(
        fx.ledger
            .resume(&ctx, out.op, b"ticket 42: settle at final"),
        Ok(Resumed::Moved(OpState::Settled))
    );
    let o = op_of(&fx, &out.op);
    assert_eq!(
        o.transitions().last().unwrap().posting,
        Some(PostingKind::Redeem)
    );
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
    // The same request again returns the original outcome; another one under the same key is a
    // reused key; a new request finds the operation terminal.
    assert_eq!(
        fx.ledger
            .resume(&ctx, out.op, b"ticket 42: settle at final"),
        Ok(Resumed::Moved(OpState::Settled))
    );
    assert_eq!(
        fx.ledger.resume(&ctx, out.op, b"again"),
        Err(Refusal::ClientKeyReused)
    );
    let ctx = on(fx.operator(), fx.op(), 70);
    assert_eq!(
        fx.ledger.resume(&ctx, out.op, b"again"),
        Err(Refusal::OperationTerminal)
    );
    // A reservation in owned custody: the operator has no authority to release it.
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve_until(&reserve, acct, usd(), Minor::new(5), Some(80))
        .unwrap();
    let ctx = on(fx.operator(), fx.op(), 75);
    assert_eq!(
        fx.ledger.resume(&ctx, reserve.op, b"customer asked"),
        Err(Refusal::UnknownAccount)
    );
    assert_eq!(op_of(&fx, &reserve.op).state(), OpState::Reserved);
}

// ---------------------------------------------------------------- T-007-10: labels and refusals

/// ERR-ReleaseUnsafe, `AC-C3.2` (amendment 0006 item 11): a release while the external leg may
/// still execute is refused, and the operation moves to `Needs-Review` with the refusal recorded
/// on the move. Nothing is released: no posting, and the value stays reserved.
#[test]
fn err_release_unsafe_moves_the_operation_to_review_and_releases_nothing() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    // Reserved, not yet acknowledged by the adapter: it may have been handed over already.
    let reserved = outbound(&mut fx, alice, acct, 30, 500);
    // Submitted: handed to the adapter.
    let submitted = outbound(&mut fx, alice, acct, 20, 500);
    fx.ledger
        .fire(
            &on(alice, submitted.op, 20),
            Trigger::HandOffAcknowledged,
            FireInput::default(),
        )
        .unwrap();
    let before = fx.ledger.book().root().unwrap();
    for (op, from) in [
        (reserved.op, OpState::Reserved),
        (submitted.op, OpState::Submitted),
    ] {
        assert_eq!(
            fx.ledger.release(&on(alice, op, 21)).map(|a| a.value),
            Err(Refusal::ReleaseUnsafe)
        );
        let o = op_of(&fx, &op);
        assert_eq!(o.state(), OpState::NeedsReview);
        let t = o.transitions().last().unwrap();
        assert_eq!(
            (t.from, t.to, t.trigger, t.posting),
            (
                from,
                OpState::NeedsReview,
                Trigger::UnsafeReleaseRefused,
                None
            )
        );
        assert!(
            fx.ledger
                .state()
                .operations()
                .outbox()
                .any(|n| n.op == op && n.refusal == Some(Refusal::ReleaseUnsafe)),
            "the refusal is recorded with the move"
        );
    }
    assert_eq!(fx.ledger.book().root().unwrap(), before, "no value moved");
    assert_eq!(fx.ledger.balance_view(&acct, &usd()).unwrap().reserved, 50);
    let q = fx.ledger.review_queue(21);
    assert_eq!(q.len(), 2);
    assert!(q.iter().all(|e| e.reason == ReviewReason::NeedsReview));
}

/// ERR-InvalidTransition, ERR-OperationTerminal, ERR-UnknownOperation from the engine.
#[test]
fn the_engine_names_its_refusals() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve(&reserve, acct, usd(), Minor::new(10))
        .unwrap();
    let ctx = on(alice, reserve.op, 20);
    assert_eq!(
        fx.ledger
            .fire(&ctx, Trigger::AppendSettling, FireInput::default()),
        Err(Refusal::InvalidTransition)
    );
    // A reservation-only operation has no hand-off.
    assert_eq!(
        fx.ledger
            .fire(&ctx, Trigger::HandOffAcknowledged, FireInput::default()),
        Err(Refusal::InvalidTransition)
    );
    fx.ledger.release(&ctx).unwrap();
    assert_eq!(
        fx.ledger.fire(&ctx, Trigger::Capture, FireInput::default()),
        Err(Refusal::OperationTerminal)
    );
    let unknown = on(alice, fx.op(), 20);
    assert_eq!(
        fx.ledger
            .fire(&unknown, Trigger::Decide, FireInput::default()),
        Err(Refusal::UnknownOperation)
    );
    assert_eq!(
        fx.ledger.release(&unknown).map(|a| a.value),
        Err(Refusal::UnknownOperation)
    );
}

/// REQ-OP-7: every reply to a lifecycle operation, and every notification, shows the state with
/// its class. No open state reads as terminal, and `Needs-Review` reads as awaiting a human.
#[test]
fn req_op_7_every_reply_and_notification_is_labelled() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let (_, dest) = funded(&mut fx, 1);
    let reserve = fx.ctx(alice);
    let r = fx
        .ledger
        .reserve(&reserve, acct, usd(), Minor::new(10))
        .unwrap();
    assert_eq!(
        r.status,
        Some(Shown {
            state: OpState::Reserved,
            class: StateClass::OpenNonRefunding
        })
    );
    let c = fx
        .ledger
        .capture(&on(alice, reserve.op, 11), dest, Minor::new(10))
        .unwrap();
    assert_eq!(c.status.map(|s| s.class), Some(StateClass::Terminal));
    let out = outbound(&mut fx, alice, acct, 5, 50);
    fx.ledger
        .fire(
            &on(alice, out.op, 20),
            Trigger::HandOffAcknowledged,
            FireInput::default(),
        )
        .unwrap();
    fx.ledger.tick(60).unwrap();
    assert_eq!(op_of(&fx, &out.op).shown().class, StateClass::AwaitingHuman);
    // Every notification in the outbox: its class is its state's, and only a terminal state
    // carries an outcome.
    let outbox: Vec<_> = fx.ledger.state().operations().outbox().cloned().collect();
    assert!(outbox.len() >= 7);
    for n in outbox {
        assert_eq!(n.shown.class, StateClass::of(n.shown.state));
        assert_eq!(n.outcome.is_some(), n.shown.class.is_terminal());
    }
    // A command with no lifecycle carries no state.
    let ctx = fx.ctx(alice);
    let acknowledged = fx.ack(&alice, Custody::Owned);
    let opened = fx
        .ledger
        .open_account(
            &ctx,
            OpenAccount {
                owner: alice.principal,
                kind: AccountKind::Customer,
                custody: Custody::Owned,
                treasury_limits: BTreeMap::new(),
                acknowledged,
            },
        )
        .unwrap();
    assert_eq!(opened.status, None);
}
