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
    // The same request again: its original outcome, and nothing taken.
    fx.ledger
        .reschedule_upgrade(&ctx, 8, b"T-037-15: the switch failed")
        .unwrap();
    assert_eq!(fx.ledger.take_way_out(), None);
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
/// on the trail, and takes nothing: by another principal than the operator, or by anyone while the
/// writer names a mode this code does not enforce. A move to the way out's own input or before is
/// `ERR-InvalidTransition`; a blank justification is `ERR-JustificationRequired`.
#[test]
fn t037_15_an_unauthorised_or_invalid_way_out_takes_nothing() {
    let mut fx = Fx::new();
    fx.ledger.set_input(7);
    let p = fx.person();
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
        let ctx = fx.ctx(op);
        assert_eq!(
            fx.ledger.reschedule_upgrade(&ctx, from, why).unwrap_err(),
            refusal
        );
        assert_eq!(fx.ledger.take_way_out(), None);
    }
    // A writer that names a mode: this code enforces none, so nobody is authorised.
    fx.ledger.set_code([0; 32], [0xA0; 32]);
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger.cancel_upgrade(&ctx, b"a mode").unwrap_err(),
        Refusal::NotAuthorised
    );
    assert_eq!(fx.ledger.take_way_out(), None);
}
