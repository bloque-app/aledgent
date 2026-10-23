// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The way out of a scheduled upgrade (`T-037-15`, `REQ-RT-20`, `REQ-RT-18`): `reschedule_upgrade`
//! and `cancel_upgrade`, decided by the library, applied by the writer.

use super::operator::last_audit;
use super::*;
use crate::audit::{AuditAction, AuditOutcome};
use crate::code::WayOut;
use crate::state::Command;

/// `REQ-RT-20`, `REQ-RT-18`, `REQ-RT-16`: a way out is an operator intervention under the mode in
/// force (the genesis mode, the operator, until `T-037-05`): accepted, it is recorded and audited,
/// and the writer reads what it took once; a repeat of the same request answers its original
/// outcome and takes nothing again.
#[test]
fn t037_15_an_authorised_way_out_is_taken_once_and_audited() {
    let mut fx = Fx::new();
    fx.ledger.set_input(7);
    let op = fx.operator();
    let ctx = fx.ctx(op);
    fx.ledger
        .reschedule_upgrade(&ctx, 8, b"T-037-15: the switch failed")
        .unwrap();
    assert_eq!(
        fx.ledger.take_way_out(),
        Some(WayOut::Reschedule { from: 8 })
    );
    assert_eq!(fx.ledger.take_way_out(), None);
    let r = last_audit(&fx);
    assert_eq!(r.action, AuditAction::Command(Command::RescheduleUpgrade));
    assert_eq!(r.outcome, AuditOutcome::Accepted);
    // The same request again, as its own input: its original outcome, and nothing taken.
    fx.ledger.set_input(8);
    fx.ledger
        .reschedule_upgrade(&ctx, 8, b"T-037-15: the switch failed")
        .unwrap();
    assert_eq!(fx.ledger.take_way_out(), None);
    fx.ledger.set_input(9);
    let ctx = fx.ctx(op);
    fx.ledger
        .cancel_upgrade(&ctx, b"T-037-15: abandoned")
        .unwrap();
    assert_eq!(fx.ledger.take_way_out(), Some(WayOut::Cancel));
    assert_eq!(
        last_audit(&fx).action,
        AuditAction::Command(Command::CancelUpgrade)
    );
    // What the writer does with it: a cancel clears the upgrade, a move moves it, and with
    // nothing scheduled nothing changes.
    assert_eq!(WayOut::Cancel.apply(Some(([1; 32], 5))), None);
    assert_eq!(
        WayOut::Reschedule { from: 9 }.apply(Some(([1; 32], 5))),
        Some(([1; 32], 9))
    );
    assert_eq!(WayOut::Reschedule { from: 9 }.apply(None), None);
}

/// `AC-K7.17`, `REQ-RT-18`: a way out not authorised under the mode in force is refused, recorded
/// on the trail, and takes nothing: by another principal than the genesis mode's (the operator), or
/// by anyone while the writer's hot row and its upgrade-authority record disagree on the mode
/// (`T-037-05`; a way out under a changed mode is `core-binding`'s `tests/upgrade_authority.rs`). A move to the way out's own input or before is
/// `ERR-InvalidTransition`; a blank justification is `ERR-JustificationRequired`.
#[test]
fn t037_15_an_unauthorised_or_invalid_way_out_takes_nothing() {
    let mut fx = Fx::new();
    let p = fx.person();
    fx.ledger.set_input(7);
    let ctx = fx.ctx(p);
    assert_eq!(
        fx.ledger.cancel_upgrade(&ctx, b"not mine").unwrap_err(),
        Refusal::NotAuthorised
    );
    let r = last_audit(&fx);
    assert_eq!(r.action, AuditAction::Command(Command::CancelUpgrade));
    assert_eq!(r.outcome, AuditOutcome::Refused(Refusal::NotAuthorised));
    assert_eq!(fx.ledger.take_way_out(), None);
    let op = fx.operator();
    for (from, why, refusal) in [
        (7, &b"to its own input"[..], Refusal::InvalidTransition),
        (3, &b"to an earlier input"[..], Refusal::InvalidTransition),
        (9, &b"  "[..], Refusal::JustificationRequired),
    ] {
        fx.ledger.set_input(7);
        let ctx = fx.ctx(op);
        assert_eq!(
            fx.ledger.reschedule_upgrade(&ctx, from, why).unwrap_err(),
            refusal
        );
        assert_eq!(fx.ledger.take_way_out(), None);
    }
    // A writer whose hot row names a mode its record does not hold stops there (`T-037-05`): the
    // way out is decided under no mode at all.
    fx.ledger.set_code([0; 32], [0xA0; 32]);
    fx.ledger.set_input(7);
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger.cancel_upgrade(&ctx, b"a mode").unwrap_err(),
        Refusal::Internal
    );
    assert_eq!(fx.ledger.take_way_out(), None);
}

/// `T-037-05`, owner ruling w6: with an upgrade scheduled from input 10, a move goes strictly
/// later than 10 (and than its own input): earlier or equal is `ERR-InvalidTransition`, recorded
/// on the trail, taking nothing; strictly later is taken. A cancel is taken whatever is scheduled.
/// With nothing scheduled the move is checked against its own input alone, as before. An
/// unauthorised move stays `ERR-NotAuthorised`, however early.
#[test]
fn t037_05_a_move_goes_strictly_later_than_the_scheduled_input() {
    let mut fx = Fx::new();
    let op = fx.operator();
    let p = fx.person();
    let scheduled = Some(([0xB1; 32], 10));
    // Earlier than the scheduled input, equal to it: refused, recorded, nothing taken. Input 7
    // is below both, so the way out's own input does not decide it.
    for from in [8, 9, 10] {
        fx.ledger.set_scheduled_upgrade(scheduled);
        fx.ledger.set_input(7);
        let ctx = fx.ctx(op);
        assert_eq!(
            fx.ledger
                .reschedule_upgrade(&ctx, from, b"pull it forward")
                .unwrap_err(),
            Refusal::InvalidTransition,
            "to {from}"
        );
        assert_eq!(
            last_audit(&fx).outcome,
            AuditOutcome::Refused(Refusal::InvalidTransition)
        );
        assert_eq!(fx.ledger.take_way_out(), None);
    }
    // An unauthorised move keeps its refusal: authority is decided first.
    fx.ledger.set_scheduled_upgrade(scheduled);
    fx.ledger.set_input(7);
    let ctx = fx.ctx(p);
    assert_eq!(
        fx.ledger
            .reschedule_upgrade(&ctx, 8, b"not mine")
            .unwrap_err(),
        Refusal::NotAuthorised
    );
    // Strictly later: taken.
    fx.ledger.set_scheduled_upgrade(scheduled);
    fx.ledger.set_input(7);
    let ctx = fx.ctx(op);
    fx.ledger
        .reschedule_upgrade(&ctx, 11, b"push it back")
        .unwrap();
    assert_eq!(
        fx.ledger.take_way_out(),
        Some(WayOut::Reschedule { from: 11 })
    );
    assert_eq!(last_audit(&fx).outcome, AuditOutcome::Accepted);
    // A cancel is not a move: taken.
    fx.ledger.set_scheduled_upgrade(scheduled);
    fx.ledger.set_input(8);
    let ctx = fx.ctx(op);
    fx.ledger.cancel_upgrade(&ctx, b"abandon it").unwrap();
    assert_eq!(fx.ledger.take_way_out(), Some(WayOut::Cancel));
    // The scheduled upgrade already applies (the way out's own input is at or after it): the move
    // is still checked against its own input, which is the later of the two.
    fx.ledger.set_scheduled_upgrade(scheduled);
    fx.ledger.set_input(12);
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger
            .reschedule_upgrade(&ctx, 12, b"to its own input")
            .unwrap_err(),
        Refusal::InvalidTransition
    );
    fx.ledger.set_input(12);
    let ctx = fx.ctx(op);
    fx.ledger
        .reschedule_upgrade(&ctx, 13, b"after its own input")
        .unwrap();
    assert_eq!(
        fx.ledger.take_way_out(),
        Some(WayOut::Reschedule { from: 13 })
    );
    // Nothing scheduled: only its own input decides, and the move is recorded, changing nothing.
    fx.ledger.set_scheduled_upgrade(None);
    fx.ledger.set_input(7);
    let ctx = fx.ctx(op);
    fx.ledger
        .reschedule_upgrade(&ctx, 8, b"nothing to move")
        .unwrap();
    assert_eq!(
        fx.ledger.take_way_out(),
        Some(WayOut::Reschedule { from: 8 })
    );
}

/// `T-037-05`, owner ruling w9: a move goes at most `MAX_LEAD` inputs after its own input, the
/// bound a change of mode is held to (`check_from`). Exactly `MAX_LEAD` ahead is taken; one past it
/// is `ERR-InvalidTransition`, recorded, taking nothing, with or without an upgrade scheduled. A
/// cancel has no target and is unaffected. An unauthorised move stays `ERR-NotAuthorised`.
#[test]
fn t037_05_a_move_goes_at_most_the_lead_ahead() {
    use crate::code::authority::MAX_LEAD;
    let mut fx = Fx::new();
    let op = fx.operator();
    let p = fx.person();
    let own = 7;
    for scheduled in [None, Some(([0xB1; 32], 10))] {
        for (from, taken) in [
            (own + MAX_LEAD, true),
            (own + MAX_LEAD + 1, false),
            (u64::MAX, false),
        ] {
            fx.ledger.set_scheduled_upgrade(scheduled);
            fx.ledger.set_input(own);
            let ctx = fx.ctx(op);
            let r = fx.ledger.reschedule_upgrade(&ctx, from, b"far ahead");
            if taken {
                r.unwrap();
                assert_eq!(
                    fx.ledger.take_way_out(),
                    Some(WayOut::Reschedule { from }),
                    "{scheduled:?} {from}"
                );
                assert_eq!(last_audit(&fx).outcome, AuditOutcome::Accepted);
            } else {
                assert_eq!(r.unwrap_err(), Refusal::InvalidTransition, "{from}");
                assert_eq!(
                    last_audit(&fx).outcome,
                    AuditOutcome::Refused(Refusal::InvalidTransition)
                );
                assert_eq!(fx.ledger.take_way_out(), None);
            }
        }
    }
    // The lead counts from the way out's own input, not from the scheduled one.
    fx.ledger.set_scheduled_upgrade(Some(([0xB1; 32], 10)));
    fx.ledger.set_input(own + 1);
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger
            .reschedule_upgrade(&ctx, own + 1 + MAX_LEAD + 1, b"one past")
            .unwrap_err(),
        Refusal::InvalidTransition
    );
    // A cancel is unaffected; an unauthorised move is refused for authority first.
    fx.ledger.set_input(own + 2);
    let ctx = fx.ctx(op);
    fx.ledger.cancel_upgrade(&ctx, b"abandon").unwrap();
    assert_eq!(fx.ledger.take_way_out(), Some(WayOut::Cancel));
    fx.ledger.set_input(own + 3);
    let ctx = fx.ctx(p);
    assert_eq!(
        fx.ledger
            .reschedule_upgrade(&ctx, u64::MAX, b"not mine")
            .unwrap_err(),
        Refusal::NotAuthorised
    );
}

/// Owner ruling on #44: a way out is its input's first call. One decided after another call of the
/// same input is refused `ERR-MalformedCommand`, recorded, and takes nothing; the next input starts
/// again ([`Ledger::set_input`]).
#[test]
fn t037_15_a_way_out_after_another_call_of_its_input_is_malformed() {
    let mut fx = Fx::new();
    fx.ledger.set_input(7);
    let _ = fx.person();
    let op = fx.operator();
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger.cancel_upgrade(&ctx, b"second call").unwrap_err(),
        Refusal::MalformedCommand
    );
    assert_eq!(
        last_audit(&fx).outcome,
        AuditOutcome::Refused(Refusal::MalformedCommand)
    );
    assert_eq!(fx.ledger.take_way_out(), None);
    fx.ledger.set_input(8);
    let ctx = fx.ctx(op);
    fx.ledger.cancel_upgrade(&ctx, b"first call").unwrap();
    assert_eq!(fx.ledger.take_way_out(), Some(WayOut::Cancel));
}

/// `REQ-RT-16`, `REQ-RT-18`, `AC-K7.1` (the library's half): an `upgrade` is an operator
/// intervention under the mode in force. Accepted, it is recorded and audited, and the writer reads
/// what it took once; a repeat of the same request answers its original outcome and takes nothing
/// again; a later upgrade before `from` is taken too (the writer replaces the pending one).
#[test]
fn t037_04_an_authorised_upgrade_is_taken_once_and_audited() {
    let mut fx = Fx::new();
    fx.ledger.set_input(7);
    let op = fx.operator();
    let ctx = fx.ctx(op);
    fx.ledger
        .upgrade(&ctx, [0xB1; 32], 8, b"T-037-04: the next code")
        .unwrap();
    assert_eq!(fx.ledger.take_upgrade(), Some(([0xB1; 32], 8)));
    assert_eq!(fx.ledger.take_upgrade(), None);
    let r = last_audit(&fx);
    assert_eq!(r.action, AuditAction::Command(Command::Upgrade));
    assert_eq!(r.outcome, AuditOutcome::Accepted);
    // The same request again, as its own input: its original outcome, nothing taken.
    fx.ledger.set_input(8);
    fx.ledger
        .upgrade(&ctx, [0xB1; 32], 8, b"T-037-04: the next code")
        .unwrap();
    assert_eq!(fx.ledger.take_upgrade(), None);
    // Another code version or another input is another request, and is taken.
    fx.ledger.set_input(9);
    let ctx = fx.ctx(op);
    fx.ledger
        .upgrade(&ctx, [0xB2; 32], 10, b"T-037-04: another")
        .unwrap();
    assert_eq!(fx.ledger.take_upgrade(), Some(([0xB2; 32], 10)));
}

/// `AC-K7.17`'s sibling for the upgrade, `REQ-RT-18`, `REQ-RT-16`: an upgrade not authorised under
/// the mode in force is refused, recorded, and takes nothing: another principal than the operator,
/// a writer naming a mode this code does not enforce. An `from` that is not later than its own
/// input is `ERR-InvalidTransition` (amendment 0023 (b) step 3), a blank justification
/// `ERR-JustificationRequired`.
#[test]
fn t037_04_an_unauthorised_or_invalid_upgrade_takes_nothing() {
    let mut fx = Fx::new();
    let p = fx.person();
    fx.ledger.set_input(7);
    let ctx = fx.ctx(p);
    assert_eq!(
        fx.ledger
            .upgrade(&ctx, [0xB1; 32], 9, b"not mine")
            .unwrap_err(),
        Refusal::NotAuthorised
    );
    let r = last_audit(&fx);
    assert_eq!(r.action, AuditAction::Command(Command::Upgrade));
    assert_eq!(r.outcome, AuditOutcome::Refused(Refusal::NotAuthorised));
    assert_eq!(fx.ledger.take_upgrade(), None);
    let op = fx.operator();
    for (from, why, refusal) in [
        (7, &b"to its own input"[..], Refusal::InvalidTransition),
        (3, &b"to an earlier input"[..], Refusal::InvalidTransition),
        (9, &b" \t"[..], Refusal::JustificationRequired),
    ] {
        fx.ledger.set_input(7);
        let ctx = fx.ctx(op);
        assert_eq!(
            fx.ledger.upgrade(&ctx, [0xB1; 32], from, why).unwrap_err(),
            refusal
        );
        assert_eq!(fx.ledger.take_upgrade(), None);
    }
    // A writer whose hot row names a mode its record does not hold stops there (`T-037-05`).
    fx.ledger.set_code([0; 32], [0xA0; 32]);
    fx.ledger.set_input(7);
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger
            .upgrade(&ctx, [0xB1; 32], 9, b"a mode")
            .unwrap_err(),
        Refusal::Internal
    );
    assert_eq!(fx.ledger.take_upgrade(), None);
}

/// `T-037-04`: the door asks the same authority function the input is decided with
/// ([`Ledger::may_change_code`]): the operator may, another principal may not, and a writer whose
/// hot row and record disagree on the mode lets nobody.
#[test]
fn t037_04_the_door_asks_the_function_the_input_is_decided_with() {
    use crate::ledger::operator::CodeInput;
    let mut fx = Fx::new();
    let p = fx.person();
    let op = fx.operator();
    let (by_op, by_person) = (fx.ctx(op).caller, fx.ctx(p).caller);
    for input in [CodeInput::Upgrade, CodeInput::WayOut] {
        assert!(fx.ledger.may_change_code(&by_op, input));
        assert!(!fx.ledger.may_change_code(&by_person, input));
    }
    fx.ledger.set_code([0; 32], [0xA0; 32]);
    for input in [CodeInput::Upgrade, CodeInput::WayOut] {
        assert!(!fx.ledger.may_change_code(&by_op, input));
    }
}
