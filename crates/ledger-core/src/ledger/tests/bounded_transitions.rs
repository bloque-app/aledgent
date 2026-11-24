// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of the value bounds of a later transition of an accepted reservation (`REQ-AU-1`,
//! `REQ-AG-2`, `AC-G1.3`, from rules version [`BOUNDED_TRANSITIONS`] on): a grant that captures or
//! releases a reservation is held to its per-operation bound for what it moves, and a capture to
//! its per-window bounds, except the links the reservation already charged when it was opened.
//! The submission of an outbound leg is tested with the settlement legs (`tests/settlement.rs`).

use super::*;
use crate::authority::WindowKey;
use crate::eval::BOUNDED_TRANSITIONS;

/// A grant over `acct` from its owner `alice` to a new person, for `ops`, with the bounds given.
fn delegate(
    fx: &mut Fx,
    alice: Caller,
    acct: AccountId,
    ops: &[OperationKind],
    per_operation: Option<u64>,
    per_window: Option<u64>,
) -> (Caller, GrantId) {
    let mallory = fx.person();
    let grant = grant_to(fx, alice, acct, &mallory, ops, per_operation, per_window);
    (mallory, grant)
}

/// A grant over `acct` from its owner `alice` to `to`, for `ops`, with the bounds given.
fn grant_to(
    fx: &mut Fx,
    alice: Caller,
    acct: AccountId,
    to: &Caller,
    ops: &[OperationKind],
    per_operation: Option<u64>,
    per_window: Option<u64>,
) -> GrantId {
    let ctx = fx.ctx(alice);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: to.principal,
                subject: Subject::Account(acct),
                ops: NonEmptySet::new(ops.iter().copied().collect()).unwrap(),
                per_operation: per_operation.map(nz),
                per_window: per_window.map(|l| Window {
                    length: NonZeroU64::new(1_000).unwrap(),
                    limit: nz(l),
                }),
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap()
        .value
}

fn window(grant: GrantId) -> WindowKey {
    WindowKey {
        grant,
        window: 0,
        asset: usd(),
    }
}

/// `ledger-core-rest-1`: a delegate bounded to 1 per operation and 5 per window cannot capture a
/// reservation of 10 000 that the owner placed. It is `ERR-BoundExceeded` and nothing changes.
/// Under the rules version before, the capture was accepted, as that code decided it.
#[test]
fn req_ag_2_a_bounded_delegate_cannot_capture_the_owners_larger_reservation() {
    for (rules, held) in [
        (BOUNDED_TRANSITIONS, true),
        (BOUNDED_TRANSITIONS - 1, false),
    ] {
        let mut fx = Fx::new();
        fx.ledger.set_generation(rules, false);
        let (alice, acct) = funded(&mut fx, 10_000);
        let ops = [OperationKind::Reserve, OperationKind::Capture];
        let (mallory, grant) = delegate(&mut fx, alice, acct, &ops, Some(1), Some(5));
        let hers = fx.account(&mallory, Custody::Owned);
        let own = fx.ctx(mallory);
        assert_eq!(
            fx.ledger
                .reserve(&own, acct, usd(), Minor::new(2))
                .map(|a| a.value),
            Err(Refusal::BoundExceeded)
        );
        let hold = fx.ctx(alice);
        fx.ledger
            .reserve(&hold, acct, usd(), Minor::new(10_000))
            .unwrap();
        let ctx = Ctx {
            op: hold.op,
            ..fx.ctx(mallory)
        };
        let c = fx.ledger.capture(&ctx, hers, Minor::new(10_000));
        if held {
            assert_eq!(c.map(|a| a.value), Err(Refusal::BoundExceeded));
            assert_eq!(fx.ledger.balance_view(&hers, &usd()).unwrap().total, 0);
            let view = fx.ledger.balance_view(&acct, &usd()).unwrap();
            assert_eq!((view.available, view.reserved), (0, 10_000));
            assert_eq!(fx.ledger.state().used(&window(grant)), Minor::ZERO);
            assert!(fx.ledger.state().transitions(&hold.op).is_empty());
            // The owner still captures her own reservation.
            let ctx = Ctx {
                op: hold.op,
                ..fx.ctx(alice)
            };
            fx.ledger.capture(&ctx, hers, Minor::new(10_000)).unwrap();
        } else {
            assert!(c.is_ok());
        }
        assert_eq!(
            fx.ledger.balance_view(&hers, &usd()).unwrap().available,
            10_000
        );
        assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
    }
}

/// A partial capture is held to the per-operation bound for what it clears, not for what was
/// reserved: the rest returns to the payer.
#[test]
fn req_ag_2_a_partial_capture_is_bounded_by_what_it_clears() {
    let mut fx = Fx::new();
    fx.ledger.set_generation(BOUNDED_TRANSITIONS, false);
    let (alice, acct) = funded(&mut fx, 1_000);
    let (mallory, _) = delegate(
        &mut fx,
        alice,
        acct,
        &[OperationKind::Capture],
        Some(10),
        None,
    );
    let hers = fx.account(&mallory, Custody::Owned);
    let hold = fx.ctx(alice);
    fx.ledger
        .reserve(&hold, acct, usd(), Minor::new(100))
        .unwrap();
    let over = Ctx {
        op: hold.op,
        ..fx.ctx(mallory)
    };
    assert_eq!(
        fx.ledger
            .capture(&over, hers, Minor::new(11))
            .map(|a| a.value),
        Err(Refusal::BoundExceeded)
    );
    let within = Ctx {
        op: hold.op,
        ..fx.ctx(mallory)
    };
    fx.ledger.capture(&within, hers, Minor::new(10)).unwrap();
    assert_eq!(fx.ledger.balance_view(&hers, &usd()).unwrap().available, 10);
    let view = fx.ledger.balance_view(&acct, &usd()).unwrap();
    assert_eq!((view.available, view.reserved), (990, 0));
}

/// A capture of a reservation another authority opened is charged to the capturing grant's window,
/// and refused past it; a capture of the grant's own reservation is not charged again.
#[test]
fn req_ag_2_a_capture_counts_against_the_window_once() {
    let mut fx = Fx::new();
    fx.ledger.set_generation(BOUNDED_TRANSITIONS, false);
    let (alice, acct) = funded(&mut fx, 1_000);
    let ops = [OperationKind::Reserve, OperationKind::Capture];
    let (mallory, grant) = delegate(&mut fx, alice, acct, &ops, None, Some(50));
    let hers = fx.account(&mallory, Custody::Owned);
    // Her own reservation is charged once, when it is opened.
    let own = fx.ctx(mallory);
    fx.ledger
        .reserve(&own, acct, usd(), Minor::new(20))
        .unwrap();
    let ctx = Ctx {
        op: own.op,
        ..fx.ctx(mallory)
    };
    fx.ledger.capture(&ctx, hers, Minor::new(20)).unwrap();
    assert_eq!(fx.ledger.state().used(&window(grant)), Minor::new(20));
    // The owner's reservation is charged when the delegate captures it.
    let first = fx.ctx(alice);
    fx.ledger
        .reserve(&first, acct, usd(), Minor::new(30))
        .unwrap();
    let ctx = Ctx {
        op: first.op,
        ..fx.ctx(mallory)
    };
    fx.ledger.capture(&ctx, hers, Minor::new(30)).unwrap();
    assert_eq!(fx.ledger.state().used(&window(grant)), Minor::new(50));
    // Past the window, it is refused and nothing changes.
    let second = fx.ctx(alice);
    fx.ledger
        .reserve(&second, acct, usd(), Minor::new(1))
        .unwrap();
    let ctx = Ctx {
        op: second.op,
        ..fx.ctx(mallory)
    };
    assert_eq!(
        fx.ledger
            .capture(&ctx, hers, Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::BoundExceeded)
    );
    assert_eq!(fx.ledger.state().used(&window(grant)), Minor::new(50));
    assert_eq!(fx.ledger.balance_view(&hers, &usd()).unwrap().available, 50);
}

/// A delegate bounded to 1 per operation cannot release a reservation of 10 000 the owner placed;
/// it still releases its own. A release charges no window.
#[test]
fn req_ag_2_a_bounded_delegate_cannot_release_the_owners_larger_reservation() {
    let mut fx = Fx::new();
    fx.ledger.set_generation(BOUNDED_TRANSITIONS, false);
    let (alice, acct) = funded(&mut fx, 10_001);
    let ops = [OperationKind::Reserve, OperationKind::Release];
    let (mallory, grant) = delegate(&mut fx, alice, acct, &ops, Some(1), Some(5));
    let hold = fx.ctx(alice);
    fx.ledger
        .reserve(&hold, acct, usd(), Minor::new(10_000))
        .unwrap();
    let ctx = Ctx {
        op: hold.op,
        ..fx.ctx(mallory)
    };
    assert_eq!(
        fx.ledger.release(&ctx).map(|a| a.value),
        Err(Refusal::BoundExceeded)
    );
    assert_eq!(
        fx.ledger.balance_view(&acct, &usd()).unwrap().reserved,
        10_000
    );
    assert!(fx.ledger.state().transitions(&hold.op).is_empty());
    // Her own reservation of 1 she releases, charged once, when it was opened.
    let own = fx.ctx(mallory);
    fx.ledger.reserve(&own, acct, usd(), Minor::new(1)).unwrap();
    let ctx = Ctx {
        op: own.op,
        ..fx.ctx(mallory)
    };
    fx.ledger.release(&ctx).unwrap();
    assert_eq!(fx.ledger.state().used(&window(grant)), Minor::new(1));
    assert_eq!(
        fx.ledger.balance_view(&acct, &usd()).unwrap().reserved,
        10_000
    );
}

/// A delegate's narrower grant does not shadow the one that covers what it moves. It holds G1
/// (reserve, capture and release, 1 000 per operation) and G2 (capture only, 10 per operation),
/// and reserves 500 under G1. The capture is accepted at both rules versions: before
/// [`BOUNDED_TRANSITIONS`] under G2, the narrowest, with no amount checked, as that code decided
/// it; from it under G1, the first candidate whose bounds admit the 500.
#[test]
fn req_ag_2_a_narrower_grant_does_not_shadow_the_one_that_covers_the_capture() {
    for (rules, decides) in [(BOUNDED_TRANSITIONS, 0), (BOUNDED_TRANSITIONS - 1, 1)] {
        let mut fx = Fx::new();
        fx.ledger.set_generation(rules, false);
        let (alice, acct) = funded(&mut fx, 1_000);
        let ops = [
            OperationKind::Reserve,
            OperationKind::Capture,
            OperationKind::Release,
        ];
        let (bob, g1) = delegate(&mut fx, alice, acct, &ops, Some(1_000), None);
        let g2 = grant_to(
            &mut fx,
            alice,
            acct,
            &bob,
            &[OperationKind::Capture],
            Some(10),
            None,
        );
        let his = fx.account(&bob, Custody::Owned);
        let own = fx.ctx(bob);
        fx.ledger
            .reserve(&own, acct, usd(), Minor::new(500))
            .unwrap();
        let ctx = Ctx {
            op: own.op,
            ..fx.ctx(bob)
        };
        fx.ledger.capture(&ctx, his, Minor::new(500)).unwrap();
        assert_eq!(fx.ledger.balance_view(&his, &usd()).unwrap().available, 500);
        let record = fx.ledger.state().transitions(&own.op).last().unwrap();
        assert_eq!(record.authorities[0].grant, Some([g1, g2][decides]));
    }
}

/// A release returns the value to its payer and is neither checked against nor charged to a
/// window: a delegate that did not open the reservation releases 100 within its per-operation
/// bound although its window is 50, and the window stays untouched.
#[test]
fn req_ag_2_a_release_charges_no_window() {
    let mut fx = Fx::new();
    fx.ledger.set_generation(BOUNDED_TRANSITIONS, false);
    let (alice, acct) = funded(&mut fx, 1_000);
    let (mallory, grant) = delegate(
        &mut fx,
        alice,
        acct,
        &[OperationKind::Release],
        Some(1_000),
        Some(50),
    );
    let hold = fx.ctx(alice);
    fx.ledger
        .reserve(&hold, acct, usd(), Minor::new(100))
        .unwrap();
    let ctx = Ctx {
        op: hold.op,
        ..fx.ctx(mallory)
    };
    fx.ledger.release(&ctx).unwrap();
    assert_eq!(fx.ledger.state().used(&window(grant)), Minor::ZERO);
    let view = fx.ledger.balance_view(&acct, &usd()).unwrap();
    assert_eq!((view.available, view.reserved), (1_000, 0));
}

/// The operator's `resume` is exempt from the value bounds (`REQ-OP-5`, `REQ-RC-1`, `REQ-AT-3`):
/// it releases with no amount, as before, under an operator grant whose `Release` bound is 1 per
/// operation, a hold of 10 000. It is an audited operator command that carries a justification,
/// and a release returns the value to its payer. This test pins the exemption: bounding it is a
/// decision, not a refactor.
#[test]
fn req_rc_1_resume_releases_a_larger_hold_than_the_operators_release_bound() {
    let mut fx = Fx::new();
    fx.ledger.set_generation(BOUNDED_TRANSITIONS, false);
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let issuer = fx.issuer();
    let ctx = fx.ctx(issuer);
    fx.ledger
        .issue(&ctx, usd(), acct, Minor::new(10_000))
        .unwrap();
    let hold = fx.ctx(alice);
    fx.ledger
        .reserve(&hold, acct, usd(), Minor::new(10_000))
        .unwrap();
    let ops = super::operator::staff(&mut fx, &[crate::operator::OperatorOpKind::Resume]);
    let ctx = fx.ctx(fx.operator());
    let grant = fx
        .ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Operator,
                principal: ops.principal,
                subject: Subject::DelegatedAccounts,
                ops: NonEmptySet::one(OperationKind::Release),
                per_operation: Some(nz(1)),
                per_window: Some(Window {
                    length: NonZeroU64::new(1_000).unwrap(),
                    limit: nz(1),
                }),
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap()
        .value;
    // A release under the grant is held to its bound.
    let release = Ctx {
        op: hold.op,
        ..fx.ctx(ops)
    };
    assert_eq!(
        fx.ledger.release(&release).map(|a| a.value),
        Err(Refusal::BoundExceeded)
    );
    // The operator's resume is not.
    let ctx = fx.ctx(ops);
    assert_eq!(
        fx.ledger
            .resume(&ctx, hold.op, b"ticket 7: release the hold"),
        Ok(crate::ledger::lifecycle::Resumed::Moved(
            crate::lifecycle::State::Released
        ))
    );
    let view = fx.ledger.balance_view(&acct, &usd()).unwrap();
    assert_eq!((view.available, view.reserved), (10_000, 0));
    assert_eq!(fx.ledger.state().used(&window(grant)), Minor::ZERO);
}

/// A fallback candidate is checked against all of its own bounds, not only the value bounds. A
/// delegate holds a narrow capture grant (10 per operation) and a wide one (1 000 per operation)
/// whose counterparty bound, or asset bound, leaves out the capture: the narrow one cannot cover
/// the owner's 500, the wide one does not admit the destination or the asset, so the capture is
/// refused, nothing moves and no window is charged. With the wide grant admitting both, it is
/// accepted under it.
#[test]
fn req_ag_2_a_fallback_grant_is_held_to_its_own_asset_and_counterparty_bounds() {
    for (wide_asset, wide_party, accepted) in [
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let mut fx = Fx::new();
        fx.ledger.set_generation(BOUNDED_TRANSITIONS, false);
        let (alice, acct) = funded(&mut fx, 1_000);
        let (bob, narrow) = delegate(
            &mut fx,
            alice,
            acct,
            &[OperationKind::Capture],
            Some(10),
            Some(10),
        );
        let his = fx.account(&bob, Custody::Owned);
        let elsewhere = fx.account(&alice, Custody::Owned);
        let ctx = fx.ctx(alice);
        let wide = fx
            .ledger
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Delegated,
                    principal: bob.principal,
                    subject: Subject::Account(acct),
                    ops: NonEmptySet::one(OperationKind::Capture),
                    per_operation: Some(nz(1_000)),
                    per_window: Some(Window {
                        length: NonZeroU64::new(1_000).unwrap(),
                        limit: nz(1_000),
                    }),
                    assets: Some(NonEmptySet::one(if wide_asset {
                        usd()
                    } else {
                        AssetId::parse("EUR").unwrap()
                    })),
                    counterparties: Some(NonEmptySet::one(if wide_party {
                        his
                    } else {
                        elsewhere
                    })),
                    parent: None,
                    expires: None,
                },
            )
            .unwrap()
            .value;
        let hold = fx.ctx(alice);
        fx.ledger
            .reserve(&hold, acct, usd(), Minor::new(500))
            .unwrap();
        let ctx = Ctx {
            op: hold.op,
            ..fx.ctx(bob)
        };
        let c = fx
            .ledger
            .capture(&ctx, his, Minor::new(500))
            .map(|a| a.value);
        assert_eq!(fx.ledger.state().used(&window(narrow)), Minor::ZERO);
        if accepted {
            assert!(c.is_ok());
            assert_eq!(fx.ledger.balance_view(&his, &usd()).unwrap().available, 500);
            assert_eq!(fx.ledger.state().used(&window(wide)), Minor::new(500));
            let record = fx.ledger.state().transitions(&hold.op).last().unwrap();
            assert_eq!(record.authorities[0].grant, Some(wide));
        } else {
            assert_eq!(c, Err(Refusal::BoundExceeded));
            assert_eq!(fx.ledger.balance_view(&his, &usd()).unwrap().total, 0);
            assert_eq!(fx.ledger.balance_view(&acct, &usd()).unwrap().reserved, 500);
            assert_eq!(fx.ledger.state().used(&window(wide)), Minor::ZERO);
            assert!(fx.ledger.state().transitions(&hold.op).is_empty());
        }
    }
}
