// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of the operator surface (`F-013`). The acceptance and error cases run in the
//! conformance suite (`crates/conformance/src/cases/f013.rs`) on both stores; these cover the
//! frame's refusals one by one, the store's rules under it, and what the suite cannot see.

use super::*;
use crate::audit::{
    AuditAction, AuditClass, AuditOutcome, AuditRecord, AuditSink, AuditSubject, AuditTrail,
    SinkError,
};
use crate::operator::{OperatorGrant, OperatorOpKind, OpsRefusal, OpsWrite};
use crate::state::StoreRefusal;

/// An operator-surface grant of `kinds` to `to`, made by the operator's organisation identity.
pub(super) fn grant_kinds(fx: &mut Fx, to: IdentityId, kinds: &[OperatorOpKind]) -> GrantId {
    let ctx = fx.ctx(fx.operator());
    fx.ledger
        .grant_operator(
            &ctx,
            to,
            NonEmptySet::new(kinds.iter().copied().collect()).unwrap(),
            None,
            b"staffing: on-call operator",
        )
        .unwrap()
        .value
}

/// A member of the operator's staff: a person holding `kinds` on the operator surface.
pub(super) fn staff(fx: &mut Fx, kinds: &[OperatorOpKind]) -> Caller {
    let who = fx.person();
    grant_kinds(fx, who.principal, kinds);
    who
}

/// The last audit record the store emitted.
pub(super) fn last_audit(fx: &Fx) -> AuditRecord {
    fx.ledger
        .state()
        .audit()
        .outbox()
        .last()
        .cloned()
        .expect("an audit record")
}

/// An audit store that cannot be written.
pub(super) struct Down;

impl AuditSink for Down {
    fn append(&mut self, _: &AuditRecord) -> Result<(), SinkError> {
        Err(SinkError::Unavailable)
    }

    fn head(&self) -> Result<Option<(u64, [u8; 32])>, SinkError> {
        Err(SinkError::Unavailable)
    }
}

// ---------------------------------------------------------------- T-013-03: the frame

#[test]
fn req_os_1_a_catch_all_grant_cannot_be_constructed() {
    let mut fx = Fx::new();
    // Every kind, the removal's included, which is taken from its rules version (amendment 0043).
    fx.ledger
        .set_generation(crate::asset::ASSET_RETIREMENT, false);
    let who = fx.person();
    let ctx = fx.ctx(fx.operator());
    // Every kind at once is a catch-all: refused at the command...
    assert_eq!(
        fx.ledger
            .grant_operator(
                &ctx,
                who.principal,
                NonEmptySet::new(OperatorOpKind::ALL.into_iter().collect()).unwrap(),
                None,
                b"everything",
            )
            .map(|a| a.value),
        Err(Refusal::UnboundedGrant)
    );
    // ... and at the store, whoever writes.
    let g = OperatorGrant::new(
        Mint::new(fx.op(), 0),
        fx.founded.operator,
        who.principal,
        NonEmptySet::new(OperatorOpKind::ALL.into_iter().collect()).unwrap(),
        10,
        None,
    );
    assert_eq!(
        fx.ledger.state.commit(&[Write::Ops(OpsWrite::PutGrant(g))]),
        Err(StoreRefusal::Ops(OpsRefusal::CatchAll))
    );
    // An empty set of kinds cannot be expressed at all.
    assert!(NonEmptySet::<OperatorOpKind>::new(Default::default()).is_none());
}

#[test]
fn req_au_5_only_the_policy_authority_grants_operator_kinds() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let bob = fx.person();
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .grant_operator(
                &ctx,
                bob.principal,
                NonEmptySet::one(OperatorOpKind::FreezeAccount),
                None,
                b"please",
            )
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // The refusal is on the trail, as an operator's attempt.
    let r = last_audit(&fx);
    assert_eq!(r.outcome, AuditOutcome::Refused(Refusal::NotAuthorised));
    assert_eq!(r.principal(), Some(alice.principal));
    assert_eq!(r.action, AuditAction::Command(Command::GrantOperator));
    // A holder of a kind cannot pass it on: granting is the policy authority's.
    let ops = staff(&mut fx, &[OperatorOpKind::FreezeAccount]);
    let ctx = fx.ctx(ops);
    assert_eq!(
        fx.ledger
            .grant_operator(
                &ctx,
                bob.principal,
                NonEmptySet::one(OperatorOpKind::FreezeAccount),
                None,
                b"handover",
            )
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn err_justification_required_and_the_frame_refuses_in_order() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    // 1. No grant for the kind.
    let clerk = staff(&mut fx, &[OperatorOpKind::UnfreezeAccount]);
    let ctx = fx.ctx(clerk);
    assert_eq!(
        fx.ledger.freeze_account(&ctx, acct, b"").map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // 2. A grant for the kind, but no justification.
    let freezer = staff(&mut fx, &[OperatorOpKind::FreezeAccount]);
    for blank in [&b""[..], b"   ", b"\n\t"] {
        let ctx = fx.ctx(freezer);
        assert_eq!(
            fx.ledger.freeze_account(&ctx, acct, blank).map(|a| a.value),
            Err(Refusal::JustificationRequired)
        );
    }
    // Nothing moved; each refusal is on the trail.
    assert_eq!(
        fx.ledger.state().account(&acct).unwrap().status(),
        AccountStatus::Active
    );
    let r = last_audit(&fx);
    assert_eq!(
        r.outcome,
        AuditOutcome::Refused(Refusal::JustificationRequired)
    );
    assert_eq!(r.class, AuditClass::OperatorIntervention);
    assert!(r.subject.contains(&AuditSubject::Account(acct)));
}

#[test]
fn req_au_3_a_revoked_or_expired_operator_grant_stops_at_once() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let freezer = fx.person();
    let grant = grant_kinds(&mut fx, freezer.principal, &[OperatorOpKind::FreezeAccount]);
    let ctx = fx.ctx(fx.operator());
    fx.ledger
        .revoke_operator(&ctx, grant, b"left the team")
        .unwrap();
    let ctx = fx.ctx(freezer);
    assert_eq!(
        fx.ledger
            .freeze_account(&ctx, acct, b"fraud report")
            .map(|a| a.value),
        Err(Refusal::AuthorityRevoked)
    );
    // Revoking it again is refused.
    let ctx = fx.ctx(fx.operator());
    assert_eq!(
        fx.ledger
            .revoke_operator(&ctx, grant, b"again")
            .map(|a| a.value),
        Err(Refusal::AuthorityRevoked)
    );
    // An expired grant is no authority.
    let temp = fx.person();
    let ctx = fx.ctx(fx.operator());
    fx.ledger
        .grant_operator(
            &ctx,
            temp.principal,
            NonEmptySet::one(OperatorOpKind::FreezeAccount),
            Some(11),
            b"one shift",
        )
        .unwrap();
    let ctx = Ctx {
        now: 11,
        ..fx.ctx(temp)
    };
    assert_eq!(
        fx.ledger
            .freeze_account(&ctx, acct, b"late")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn req_au_5_operator_authority_needs_delegated_custody_and_an_unscoped_credential() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let owned = fx.account(&alice, Custody::Owned);
    let freezer = staff(&mut fx, &[OperatorOpKind::FreezeAccount]);
    let ctx = fx.ctx(freezer);
    assert_eq!(
        fx.ledger
            .freeze_account(&ctx, owned, b"fraud report")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // The issuance counterparty is no account the operator acts on.
    let ctx = fx.ctx(freezer);
    assert_eq!(
        fx.ledger
            .freeze_account(&ctx, fx.founded.issuance, b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // A credential scoped to some accounts is not an operator's.
    let delegated = fx.account(&freezer, Custody::Delegated);
    let scoped = Credential::new(
        Mint::new(fx.op(), 0),
        freezer.principal,
        CredentialMarker::Ordinary,
        Some(NonEmptySet::one(delegated)),
        Verifier::new(vec![9]),
    );
    let scoped_id = scoped.id();
    fx.ledger
        .state
        .commit(&[Write::PutCredential(scoped)])
        .unwrap();
    let ctx = fx.ctx(Caller::new(freezer.principal, scoped_id));
    assert_eq!(
        fx.ledger
            .freeze_account(&ctx, delegated, b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn req_op_11_an_operator_command_is_idempotent_on_its_client_key() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let freezer = staff(&mut fx, &[OperatorOpKind::FreezeAccount]);
    let ctx = fx.ctx(freezer);
    let first = fx
        .ledger
        .freeze_account(&ctx, acct, b"fraud report")
        .unwrap();
    let emitted = fx.ledger.state().audit().emitted();
    // The same request again: the original outcome, nothing written.
    let again = fx
        .ledger
        .freeze_account(&ctx, acct, b"fraud report")
        .unwrap();
    assert_eq!(again.record, first.record);
    assert_eq!(fx.ledger.state().audit().emitted(), emitted);
    // Another request under the same key.
    assert_eq!(
        fx.ledger
            .freeze_account(&ctx, acct, b"another reason")
            .map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
}

#[test]
fn req_os_4_an_operator_action_is_visible_to_the_accounts_authority_holders() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let agent = fx.agent(alice.principal);
    fx.delegate(
        &alice,
        agent.principal,
        acct,
        &[OperationKind::Transfer],
        10,
    );
    let freezer = staff(&mut fx, &[OperatorOpKind::FreezeAccount]);
    let ctx = fx.ctx(freezer);
    fx.ledger
        .freeze_account(&ctx, acct, b"fraud report")
        .unwrap();
    let r = last_audit(&fx);
    for holder in [alice, agent] {
        let actions = fx.ledger.account_actions(&holder, &acct, 20).unwrap();
        let freeze = actions
            .iter()
            .find(|r| r.command == Command::FreezeAccount)
            .expect("the freeze is in the account's log");
        assert_eq!(freeze.authorities[0].kind, AuthorityKind::Operator);
        assert_eq!(freeze.authorities[0].principal, freezer.principal);
    }
    // Nobody else sees it.
    let bob = fx.person();
    assert_eq!(
        fx.ledger.account_actions(&bob, &acct, 20).map(|a| a.len()),
        Err(Refusal::UnknownAccount)
    );
    // The audit record carries the justification; the account's log does not.
    assert_eq!(r.action, AuditAction::Command(Command::FreezeAccount));
    assert_eq!(
        r.justification.as_ref().unwrap().as_bytes(),
        b"fraud report"
    );
    assert_eq!(r.authority_used(), Some(AuthorityKind::Operator));
}

// ---------------------------------------------------------------- privileged reads (REQ-PV-4)

#[test]
fn req_pv_4_a_privileged_read_is_recorded_before_it_returns() {
    let mut fx = Fx::new();
    let (_, acct) = funded(&mut fx, 70);
    let reader = staff(&mut fx, &[OperatorOpKind::PrivilegedRead]);
    let mut trail = AuditTrail::new();
    // Without the kind: refused, and the attempt recorded.
    let nobody = fx.person();
    let ctx = fx.ctx(nobody);
    assert_eq!(
        fx.ledger
            .privileged_balances(&ctx, acct, usd(), b"support ticket 3", &mut trail)
            .map(|v| v.available),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(last_audit(&fx).class, AuditClass::PrivilegedRead);
    // With it: the record reaches the audit store before the data comes back.
    let ctx = fx.ctx(reader);
    let view = fx
        .ledger
        .privileged_balances(&ctx, acct, usd(), b"support ticket 3", &mut trail)
        .unwrap();
    assert_eq!(view.available, 70);
    let r = trail.iter().last().unwrap();
    assert_eq!(r.class, AuditClass::PrivilegedRead);
    assert_eq!(r.principal(), Some(reader.principal));
    assert_eq!(
        r.justification.as_ref().unwrap().as_bytes(),
        b"support ticket 3"
    );
    assert!(r.subject.contains(&AuditSubject::Account(acct)));
    assert_eq!(fx.ledger.state().audit().outbox().count(), 0);
    // An audit store that cannot be written: no data, and no record of a read that did not
    // happen. It is a fault of aledgent's, not of the reader: `ERR-Internal`, counted as its
    // incident (`REQ-ER-2`).
    let emitted = fx.ledger.state().audit().emitted();
    let faults = fx.ledger.internal_faults().0;
    let ctx = fx.ctx(reader);
    assert_eq!(
        fx.ledger
            .privileged_balances(&ctx, acct, usd(), b"support ticket 4", &mut Down)
            .map(|v| v.available),
        Err(Refusal::Internal)
    );
    assert_eq!(fx.ledger.state().audit().emitted(), emitted);
    assert_eq!(fx.ledger.internal_faults().0, faults + 1);
}

// ---------------------------------------------------------------- resume on CTR-OPS

#[test]
fn req_rc_1_resume_is_an_audited_operator_command() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve_until(&reserve, acct, usd(), Minor::new(5), Some(100))
        .unwrap();
    fx.ledger.tick(50).unwrap();
    // The operator's organisation identity holds no Resume kind until it is granted.
    let ctx = Ctx {
        now: 60,
        ..fx.ctx(fx.operator())
    };
    assert_eq!(
        fx.ledger.resume(&ctx, reserve.op, b"customer asked"),
        Err(Refusal::NotAuthorised)
    );
    let resumer = staff(&mut fx, &[OperatorOpKind::Resume]);
    let ctx = Ctx {
        now: 60,
        ..fx.ctx(resumer)
    };
    // The reservation is in owned custody: no operator authority releases it.
    assert_eq!(
        fx.ledger.resume(&ctx, reserve.op, b"customer asked"),
        Err(Refusal::UnknownAccount)
    );
    assert_eq!(
        last_audit(&fx).outcome,
        AuditOutcome::Refused(Refusal::UnknownAccount)
    );
}

// ---------------------------------------------------------------- T-013-06: freeze

#[test]
fn inv_9_a_frozen_account_refuses_debits_and_still_receives_credits() {
    let mut fx = Fx::new();
    let (bob, bob_acct) = funded(&mut fx, 50);
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let issuer = fx.operator();
    let ctx = fx.ctx(issuer);
    fx.ledger.issue(&ctx, usd(), acct, Minor::new(40)).unwrap();
    let ops = staff(
        &mut fx,
        &[
            OperatorOpKind::FreezeAccount,
            OperatorOpKind::UnfreezeAccount,
        ],
    );
    let ctx = fx.ctx(ops);
    fx.ledger
        .freeze_account(&ctx, acct, b"court order 7")
        .unwrap();
    assert_eq!(
        fx.ledger.state().account(&acct).unwrap().status(),
        AccountStatus::Frozen
    );
    // Debits and reservations against it are refused, by its owner or anyone.
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .reserve(&ctx, acct, usd(), Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::AccountFrozen)
    );
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger.close_account(&ctx, acct).map(|a| a.value),
        Err(Refusal::AccountFrozen)
    );
    // Credits land: an issue, and a capture from another account.
    let ctx = fx.ctx(issuer);
    fx.ledger.issue(&ctx, usd(), acct, Minor::new(5)).unwrap();
    let r = fx.ctx(bob);
    fx.ledger
        .reserve(&r, bob_acct, usd(), Minor::new(10))
        .unwrap();
    fx.ledger
        .capture(&Ctx { now: 12, ..r }, acct, Minor::new(10))
        .unwrap();
    assert_eq!(fx.ledger.balance_view(&acct, &usd()).unwrap().available, 55);
    // No posting was made by the freeze: the value stays available.
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
    // Unfrozen, it transacts again.
    let ctx = fx.ctx(ops);
    fx.ledger
        .unfreeze_account(&ctx, acct, b"order lifted")
        .unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.reserve(&ctx, acct, usd(), Minor::new(1)).unwrap();
}

#[test]
fn err_account_frozen_is_not_for_a_closed_account() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let ops = staff(
        &mut fx,
        &[
            OperatorOpKind::FreezeAccount,
            OperatorOpKind::UnfreezeAccount,
        ],
    );
    // Freezing twice, or unfreezing an active account, changes nothing and is recorded.
    for freeze in [true, true, false, false] {
        let ctx = fx.ctx(ops);
        let r = if freeze {
            fx.ledger.freeze_account(&ctx, acct, b"x")
        } else {
            fx.ledger.unfreeze_account(&ctx, acct, b"x")
        };
        assert!(r.is_ok());
    }
    assert_eq!(
        fx.ledger.state().account(&acct).unwrap().status(),
        AccountStatus::Active
    );
    let ctx = fx.ctx(alice);
    fx.ledger.close_account(&ctx, acct).unwrap();
    let ctx = fx.ctx(ops);
    assert_eq!(
        fx.ledger
            .freeze_account(&ctx, acct, b"too late")
            .map(|a| a.value),
        Err(Refusal::AccountClosed)
    );
}

// ---------------------------------------------------------------- T-013-07: registry, issuance

fn review(n: u8) -> crate::ledger::operator::ReviewRef {
    crate::ledger::operator::ReviewRef([n; 32])
}

fn register(id: &str, precision: u8, n: u8) -> crate::ledger::operator::RegisterAsset {
    crate::ledger::operator::RegisterAsset {
        id: AssetId::parse(id).unwrap(),
        precision,
        kind: AssetKind::Stable,
        review: review(n),
    }
}

#[test]
fn inv_24_register_asset_is_a_reviewed_change_with_one_precision() {
    let mut fx = Fx::new();
    let registrar = staff(&mut fx, &[OperatorOpKind::RegisterAsset]);
    // No review, no change.
    let ctx = fx.ctx(registrar);
    assert_eq!(
        fx.ledger
            .register_asset(&ctx, register("USDX", 6, 0), b"listing")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let ctx = fx.ctx(registrar);
    fx.ledger
        .register_asset(&ctx, register("USDX", 6, 1), b"listing, CR-101")
        .unwrap();
    let usdx = AssetId::parse("USDX").unwrap();
    assert_eq!(
        fx.ledger.state().registry().get(&usdx).unwrap().precision(),
        6
    );
    let r = last_audit(&fx);
    assert!(r.subject.contains(&AuditSubject::Review([1; 32])));
    assert!(r.subject.contains(&AuditSubject::Asset(usdx)));
    assert_eq!(r.class, AuditClass::OperatorIntervention);
    // A second precision for the asset, or the same one again, is refused; the first stays.
    for p in [6, 2] {
        let ctx = fx.ctx(registrar);
        assert_eq!(
            fx.ledger
                .register_asset(&ctx, register("USDX", p, 2), b"relist")
                .map(|a| a.value),
            Err(Refusal::NotAuthorised)
        );
    }
    assert_eq!(
        fx.ledger.state().registry().get(&usdx).unwrap().precision(),
        6
    );
    let ctx = fx.ctx(registrar);
    assert_eq!(
        fx.ledger
            .register_asset(
                &ctx,
                register("HUGE", crate::asset::MAX_PRECISION + 1, 3),
                b"x"
            )
            .map(|a| a.value),
        Err(Refusal::PrecisionExceeded)
    );
    // Registering is its own kind: another kind does not cover it.
    let freezer = staff(&mut fx, &[OperatorOpKind::FreezeAccount]);
    let ctx = fx.ctx(freezer);
    assert_eq!(
        fx.ledger
            .register_asset(&ctx, register("EURX", 6, 4), b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn req_as_2_deprecating_an_asset_is_a_reviewed_change() {
    let mut fx = Fx::new();
    let (_, acct) = funded(&mut fx, 10);
    let registrar = staff(&mut fx, &[OperatorOpKind::DeprecateAsset]);
    let ctx = fx.ctx(registrar);
    assert_eq!(
        fx.ledger
            .deprecate_asset(&ctx, AssetId::parse("NOPE").unwrap(), review(1), b"x")
            .map(|a| a.value),
        Err(Refusal::UnknownAsset)
    );
    let ctx = fx.ctx(registrar);
    fx.ledger
        .deprecate_asset(&ctx, usd(), review(1), b"sunset, CR-7")
        .unwrap();
    let ctx = fx.ctx(fx.operator());
    assert_eq!(
        fx.ledger
            .issue(&ctx, usd(), acct, Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::AssetDeprecated)
    );
    // Its precision stays in the registry.
    assert_eq!(
        fx.ledger
            .state()
            .registry()
            .get(&usd())
            .unwrap()
            .precision(),
        2
    );
}

// ---------------------------------------------------------------- amendment 0043: remove an asset

use crate::asset::{ASSET_RETIREMENT, AssetStatus, MAX_SWEEP_PAGE, SWEEP_END, SWEEP_START};
use crate::ledger::operator::{SweepNext, SweepPage};
use crate::state::{AssetInUse, RetirementRefusal};

fn tstx() -> AssetId {
    AssetId::parse("TSTX").unwrap()
}

/// A fixture whose inputs are decided at the rules version that takes the removal.
fn removal_fx() -> Fx {
    let mut fx = Fx::new();
    fx.ledger.set_generation(ASSET_RETIREMENT, false);
    fx
}

/// An issuance grant over `asset`, bounded per window and per asset.
fn issuance_over(asset: AssetId, to: IdentityId) -> GrantRequest {
    GrantRequest {
        subject: Subject::Asset(asset),
        assets: Some(NonEmptySet::one(asset)),
        ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into()).unwrap(),
        ..issuance_request(to, Some(1_000))
    }
}

/// A member of staff who registers and removes assets, and one who grants issuance authority.
fn registry_staff(fx: &mut Fx) -> (Caller, Caller) {
    let registrar = staff(
        fx,
        &[OperatorOpKind::RegisterAsset, OperatorOpKind::RemoveAsset],
    );
    let admin = staff(fx, &[OperatorOpKind::IssuanceAuthority]);
    (registrar, admin)
}

fn register_tstx(fx: &mut Fx, registrar: Caller, precision: u8, n: u8) -> crate::asset::Asset {
    let ctx = fx.ctx(registrar);
    fx.ledger
        .register_asset(&ctx, register("TSTX", precision, n), b"listing")
        .unwrap();
    *fx.ledger.state().registry().get(&tstx()).unwrap()
}

fn retire_tstx(fx: &mut Fx, who: Caller) -> Result<Option<SweepNext>, Refusal> {
    let ctx = fx.ctx(who);
    fx.ledger
        .retire_asset(
            &ctx,
            tstx(),
            review(8),
            b"registered at 8 decimals, never used",
        )
        .map(|a| a.value)
}

fn sweep(fx: &mut Fx, who: Caller, page: SweepPage) -> Result<Option<SweepNext>, Refusal> {
    let ctx = fx.ctx(who);
    fx.ledger
        .sweep_asset_uses(&ctx, tstx(), page, b"sweep")
        .map(|a| a.value)
}

/// Sweep to the end, each page at the level the host suggests: the number of pages.
fn sweep_all(fx: &mut Fx, who: Caller, mut next: Option<SweepNext>) -> Result<usize, Refusal> {
    let mut pages = 0;
    while let Some(n) = next {
        let page = SweepPage {
            start: n.start,
            level: n.level.expect("a whole store suggests a level"),
        };
        next = sweep(fx, who, page)?;
        pages += 1;
    }
    Ok(pages)
}

fn remove_tstx(fx: &mut Fx, who: Caller) -> Result<(), Refusal> {
    let ctx = fx.ctx(who);
    fx.ledger
        .remove_asset(&ctx, tstx(), review(9), b"wrong precision, never used")
        .map(|a| a.value)
}

fn still_registered(fx: &Fx, asset: &crate::asset::Asset) {
    assert_eq!(fx.ledger.state().registry().get(&asset.id()), Ok(asset));
}

fn status(fx: &Fx) -> AssetStatus {
    fx.ledger.state().registry().get(&tstx()).unwrap().status()
}

/// `REQ-AS-10`, `INV-24`: retire, sweep every account and grant, remove; the same id registers
/// again at the right precision with a fresh tag. Each step's refusals change nothing.
#[test]
fn req_as_10_an_unused_asset_is_retired_swept_removed_and_registers_again() {
    let mut fx = removal_fx();
    let (registrar, _) = registry_staff(&mut fx);
    let wrong = register_tstx(&mut fx, registrar, 8, 1);
    let digest = fx.ledger.state().registry().digest();
    // No review, nothing to retire, no justification, no authority: each refused.
    let ctx = fx.ctx(registrar);
    assert_eq!(
        fx.ledger
            .retire_asset(&ctx, tstx(), review(0), b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let ctx = fx.ctx(registrar);
    assert_eq!(
        fx.ledger
            .retire_asset(&ctx, AssetId::parse("NOPE").unwrap(), review(9), b"x")
            .map(|a| a.value),
        Err(Refusal::UnknownAsset)
    );
    let ctx = fx.ctx(registrar);
    assert_eq!(
        fx.ledger
            .retire_asset(&ctx, tstx(), review(9), b"")
            .map(|a| a.value),
        Err(Refusal::JustificationRequired)
    );
    let registerer = staff(&mut fx, &[OperatorOpKind::RegisterAsset]);
    assert_eq!(
        retire_tstx(&mut fx, registerer),
        Err(Refusal::NotAuthorised)
    );
    // Not retired: neither swept nor removed.
    assert_eq!(remove_tstx(&mut fx, registrar), Err(Refusal::NotAuthorised));
    let page = SweepPage {
        start: SWEEP_START,
        level: 8,
    };
    assert_eq!(
        sweep(&mut fx, registrar, page),
        Err(Refusal::InvalidTransition)
    );
    still_registered(&fx, &wrong);
    assert_eq!(fx.ledger.state().registry().digest(), digest);

    // Retired: audited, naming the asset and the review; the sweep starts at the accounts.
    let next = retire_tstx(&mut fx, registrar).unwrap();
    assert_eq!(next.map(|n| n.start), Some(SWEEP_START));
    let r = last_audit(&fx);
    assert_eq!(r.class, AuditClass::OperatorIntervention);
    assert!(r.subject.contains(&AuditSubject::Asset(tstx())));
    assert!(r.subject.contains(&AuditSubject::Review([8; 32])));
    assert_eq!(status(&fx), AssetStatus::Retiring);
    assert_eq!(
        retire_tstx(&mut fx, registrar),
        Err(Refusal::InvalidTransition)
    );
    // Not removed before the sweep is complete.
    assert_eq!(remove_tstx(&mut fx, registrar), Err(Refusal::NotAuthorised));
    assert_eq!(
        fx.ledger.state().asset_removable(&tstx()),
        Err(StoreRefusal::Retirement(RetirementRefusal::SweepIncomplete))
    );
    // A page that is not at the cursor, or not a subtree.
    let ahead = SweepPage {
        start: [
            crate::leaves::kind::GRANT,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ],
        level: 8,
    };
    assert_eq!(
        sweep(&mut fx, registrar, ahead),
        Err(Refusal::InvalidTransition)
    );
    let misaligned = SweepPage {
        start: SWEEP_START,
        level: 7,
    };
    assert_eq!(
        sweep(&mut fx, registrar, misaligned),
        Err(Refusal::MalformedCommand)
    );
    let pages = sweep_all(&mut fx, registrar, next).unwrap();
    assert!(pages >= 2, "at least the accounts and the grants");
    assert_eq!(
        fx.ledger.state().retirement(&tstx()).map(|r| r.cursor),
        Some(SWEEP_END)
    );
    // Complete: no more pages.
    assert_eq!(
        sweep(&mut fx, registrar, page),
        Err(Refusal::InvalidTransition)
    );

    // Removed: recorded as an operator intervention, naming the asset and the review.
    remove_tstx(&mut fx, registrar).unwrap();
    let r = last_audit(&fx);
    assert!(r.subject.contains(&AuditSubject::Review([9; 32])));
    assert!(fx.ledger.state().registry().get(&tstx()).is_err());
    assert!(fx.ledger.state().retirement(&tstx()).is_none());
    assert_ne!(fx.ledger.state().registry().digest(), digest);
    assert_eq!(remove_tstx(&mut fx, registrar), Err(Refusal::UnknownAsset));

    // Registered again at the right precision: a fresh tag, never the retired one.
    let ctx = fx.ctx(registrar);
    fx.ledger
        .register_asset(&ctx, register("TSTX", 6, 2), b"relisting")
        .unwrap();
    let right = *fx.ledger.state().registry().get(&tstx()).unwrap();
    assert_eq!(right.precision(), 6);
    assert_eq!(right.status(), AssetStatus::Active);
    assert!(right.tag().0 > wrong.tag().0);
    assert!(fx.ledger.state().registry().by_tag(wrong.tag()).is_none());
}

/// `REQ-AS-10` step 1: from the retirement on, nothing new may name the asset, and a revocation
/// is still taken. Cancelling returns it to the status it had.
#[test]
fn req_as_10_a_retiring_asset_takes_no_new_use_until_the_retirement_is_cancelled() {
    let mut fx = removal_fx();
    let (registrar, admin) = registry_staff(&mut fx);
    register_tstx(&mut fx, registrar, 8, 1);
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Owned);
    let bob = fx.person();
    let bounded = |asset| GrantRequest {
        kind: GrantKind::Delegated,
        principal: bob.principal,
        subject: Subject::Account(acct),
        ops: NonEmptySet::one(OperationKind::Transfer),
        per_operation: Some(nz(1_000)),
        per_window: None,
        assets: Some(NonEmptySet::one(asset)),
        counterparties: None,
        parent: None,
        expires: None,
    };
    let ctx = fx.ctx(alice);
    let before = fx
        .ledger
        .grant_authority(&ctx, bounded(tstx()))
        .unwrap()
        .value;
    retire_tstx(&mut fx, registrar).unwrap();
    // A grant bounding it, a grant over it, a Treasury limit in it, a deprecation: each refused.
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .grant_authority(&ctx, bounded(tstx()))
            .map(|a| a.value),
        Err(Refusal::AssetDeprecated)
    );
    let treasurer = fx.person();
    let ctx = fx.ctx(admin);
    assert_eq!(
        fx.ledger
            .grant_issuance(&ctx, issuance_over(tstx(), treasurer.principal), b"desk")
            .map(|a| a.value),
        Err(Refusal::AssetDeprecated)
    );
    let org = fx.person();
    let treasury = |asset| OpenAccount {
        owner: org.principal,
        kind: AccountKind::Treasury,
        custody: Custody::Delegated,
        treasury_limits: BTreeMap::from([(asset, Minor::new(1_000))]),
        acknowledged: None,
    };
    let ctx = fx.ctx(fx.operator());
    assert_eq!(
        fx.ledger
            .open_account(&ctx, treasury(tstx()))
            .map(|a| a.value),
        Err(Refusal::AssetDeprecated)
    );
    let deprecator = staff(&mut fx, &[OperatorOpKind::DeprecateAsset]);
    let ctx = fx.ctx(deprecator);
    assert_eq!(
        fx.ledger
            .deprecate_asset(&ctx, tstx(), review(3), b"x")
            .map(|a| a.value),
        Err(Refusal::AssetDeprecated)
    );
    assert_eq!(
        fx.ledger.state().registry().active(&tstx()).map(|a| a.id()),
        Err(Refusal::AssetDeprecated)
    );
    // A revocation is no new use.
    let ctx = fx.ctx(alice);
    fx.ledger.revoke_authority(&ctx, before).unwrap();
    // Cancelled: active again, and it takes new uses.
    let ctx = fx.ctx(registrar);
    fx.ledger
        .cancel_asset_retirement(&ctx, tstx(), b"keep it")
        .unwrap();
    assert_eq!(status(&fx), AssetStatus::Active);
    assert!(fx.ledger.state().retirement(&tstx()).is_none());
    let ctx = fx.ctx(registrar);
    assert_eq!(
        fx.ledger
            .cancel_asset_retirement(&ctx, tstx(), b"again")
            .map(|a| a.value),
        Err(Refusal::InvalidTransition)
    );
    let ctx = fx.ctx(fx.operator());
    fx.ledger.open_account(&ctx, treasury(tstx())).unwrap();
}

/// `REQ-AS-10` step 2: a grant that bounds the asset refuses its page, names the grant in the
/// audit record, and leaves the cursor; once revoked, the same page goes through.
#[test]
fn req_as_10_a_page_with_a_grant_bound_is_refused_until_the_grant_is_revoked() {
    let mut fx = removal_fx();
    let (registrar, _) = registry_staff(&mut fx);
    let asset = register_tstx(&mut fx, registrar, 8, 1);
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Owned);
    let bob = fx.person();
    let ctx = fx.ctx(alice);
    let grant = fx
        .ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: bob.principal,
                subject: Subject::Account(acct),
                ops: NonEmptySet::one(OperationKind::Transfer),
                per_operation: Some(nz(1_000)),
                per_window: None,
                assets: Some(NonEmptySet::one(tstx())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap()
        .value;
    let mut next = retire_tstx(&mut fx, registrar).unwrap();
    // The accounts are clean; then the grants' page finds it.
    let found = loop {
        let n = next.expect("the sweep reaches the grant");
        let page = SweepPage {
            start: n.start,
            level: n.level.unwrap(),
        };
        match sweep(&mut fx, registrar, page) {
            Ok(after) => next = after,
            Err(e) => break (e, page),
        }
    };
    assert_eq!(found.0, Refusal::NotAuthorised);
    assert!(
        last_audit(&fx)
            .subject
            .contains(&AuditSubject::Grant(grant))
    );
    let sub = crate::leaves::Subtree::new(found.1.start, found.1.level).unwrap();
    assert_eq!(
        fx.ledger.state().sweep_page(&tstx(), &sub),
        Err(StoreRefusal::AssetInUse(AssetInUse::GrantBound(grant)))
    );
    assert_eq!(
        fx.ledger.state().retirement(&tstx()).map(|r| r.cursor),
        Some(found.1.start)
    );
    assert_eq!(remove_tstx(&mut fx, registrar), Err(Refusal::NotAuthorised));
    still_registered(
        &fx,
        &crate::asset::Asset::clone(fx.ledger.state().registry().get(&asset.id()).unwrap()),
    );
    // Revoked while the asset is retiring; the same page again, and on to the end.
    let ctx = fx.ctx(alice);
    fx.ledger.revoke_authority(&ctx, grant).unwrap();
    let next = sweep(&mut fx, registrar, found.1).unwrap();
    sweep_all(&mut fx, registrar, next).unwrap();
    remove_tstx(&mut fx, registrar).unwrap();
}

/// `REQ-AS-10` step 2: a Treasury limit in the asset refuses its page for good; the operator
/// cancels, and the asset stays, active again.
#[test]
fn req_as_10_a_treasury_limit_refuses_the_sweep_and_the_retirement_is_cancelled() {
    let mut fx = removal_fx();
    let (registrar, _) = registry_staff(&mut fx);
    register_tstx(&mut fx, registrar, 8, 1);
    let org = fx.person();
    let ctx = fx.ctx(fx.operator());
    let treasury = fx
        .ledger
        .open_account(
            &ctx,
            OpenAccount {
                owner: org.principal,
                kind: AccountKind::Treasury,
                custody: Custody::Delegated,
                treasury_limits: BTreeMap::from([(tstx(), Minor::new(1_000))]),
                acknowledged: None,
            },
        )
        .unwrap()
        .value;
    let next = retire_tstx(&mut fx, registrar).unwrap();
    assert_eq!(
        sweep_all(&mut fx, registrar, next),
        Err(Refusal::NotAuthorised)
    );
    assert!(
        last_audit(&fx)
            .subject
            .contains(&AuditSubject::Account(treasury))
    );
    assert_eq!(remove_tstx(&mut fx, registrar), Err(Refusal::NotAuthorised));
    let ctx = fx.ctx(registrar);
    fx.ledger
        .cancel_asset_retirement(&ctx, tstx(), b"a Treasury limit names it")
        .unwrap();
    assert_eq!(status(&fx), AssetStatus::Active);
}

/// `REQ-AS-10` step 2: a page holds at most `MAX_SWEEP_PAGE` leaves; the host suggests one that
/// fits, and the sweep takes several.
#[test]
fn req_as_10_a_page_over_the_maximum_is_refused_and_the_sweep_takes_several() {
    let mut fx = removal_fx();
    let (registrar, _) = registry_staff(&mut fx);
    register_tstx(&mut fx, registrar, 8, 1);
    let owner = fx.person();
    for _ in 0..MAX_SWEEP_PAGE + 10 {
        fx.account(&owner, Custody::Owned);
    }
    let next = retire_tstx(&mut fx, registrar).unwrap();
    let whole = SweepPage {
        start: SWEEP_START,
        level: 8,
    };
    assert_eq!(
        sweep(&mut fx, registrar, whole),
        Err(Refusal::BatchTooLarge)
    );
    assert!(next.and_then(|n| n.level).is_some_and(|l| l > 8));
    let pages = sweep_all(&mut fx, registrar, next).unwrap();
    assert!(pages >= 3, "{pages} pages");
    remove_tstx(&mut fx, registrar).unwrap();
}

#[test]
fn req_as_10_an_asset_that_was_ever_issued_or_has_a_live_issuance_grant_is_never_retired() {
    let mut fx = removal_fx();
    let (registrar, admin) = registry_staff(&mut fx);
    let asset = register_tstx(&mut fx, registrar, 8, 1);
    let holder = fx.person();
    let acct = fx.account(&holder, Custody::Owned);
    let treasurer = fx.person();
    let ctx = fx.ctx(admin);
    let grant = fx
        .ledger
        .grant_issuance(
            &ctx,
            issuance_over(tstx(), treasurer.principal),
            b"desk, CR-12",
        )
        .unwrap()
        .value;
    // A live issuance grant over it: revoke first.
    assert_eq!(retire_tstx(&mut fx, registrar), Err(Refusal::NotAuthorised));
    assert!(
        last_audit(&fx)
            .subject
            .contains(&AuditSubject::Grant(grant))
    );
    // Issued once: the asset is used, whatever the balance is afterwards.
    let ctx = fx.ctx(treasurer);
    fx.ledger.issue(&ctx, tstx(), acct, Minor::new(5)).unwrap();
    let ctx = fx.ctx(admin);
    fx.ledger
        .revoke_issuance(&ctx, grant, b"desk closed")
        .unwrap();
    let digest = fx.ledger.state().registry().digest();
    assert_eq!(retire_tstx(&mut fx, registrar), Err(Refusal::NotAuthorised));
    assert_eq!(
        fx.ledger.state().asset_retirable(&tstx()),
        Err(StoreRefusal::AssetInUse(AssetInUse::Issuance))
    );
    still_registered(&fx, &asset);
    assert_eq!(fx.ledger.state().registry().digest(), digest);
    assert_eq!(last_audit(&fx).class, AuditClass::OperatorIntervention);
}

#[test]
fn req_as_10_a_bound_asset_is_never_retired() {
    let mut fx = removal_fx();
    let registrar = staff(&mut fx, &[OperatorOpKind::RemoveAsset]);
    // The genesis assets are bound to a location on the settlement layer; one is deprecated.
    for asset in [usd(), old()] {
        let before = *fx.ledger.state().registry().get(&asset).unwrap();
        let ctx = fx.ctx(registrar);
        assert_eq!(
            fx.ledger
                .retire_asset(&ctx, asset, review(9), b"x")
                .map(|a| a.value),
            Err(Refusal::NotAuthorised)
        );
        still_registered(&fx, &before);
    }
}

/// `REQ-RT-16`, `REQ-RT-17`, `REQ-RT-20`: before the rules version that takes the removal, every
/// step and a grant of its operator kind are `ERR-MalformedCommand`, write nothing and are not
/// recorded as calls, so no input an older core Agent cannot decode is made.
#[test]
fn req_rt_20_the_removal_is_malformed_before_its_rules_version_and_records_no_call() {
    let mut fx = Fx::new();
    fx.ledger.set_generation(ASSET_RETIREMENT - 1, false);
    let registrar = staff(&mut fx, &[OperatorOpKind::RegisterAsset]);
    let asset = register_tstx(&mut fx, registrar, 8, 1);
    fx.ledger.record_calls(true);
    let _ = fx.ledger.take_calls();
    let audit = fx.ledger.state().audit().outbox().count();
    let ctx = fx.ctx(fx.operator());
    assert_eq!(
        fx.ledger
            .grant_operator(
                &ctx,
                registrar.principal,
                NonEmptySet::one(OperatorOpKind::RemoveAsset),
                None,
                b"x"
            )
            .map(|a| a.value),
        Err(Refusal::MalformedCommand)
    );
    assert_eq!(
        retire_tstx(&mut fx, registrar),
        Err(Refusal::MalformedCommand)
    );
    let page = SweepPage {
        start: SWEEP_START,
        level: 8,
    };
    assert_eq!(
        sweep(&mut fx, registrar, page),
        Err(Refusal::MalformedCommand)
    );
    let ctx = fx.ctx(registrar);
    assert_eq!(
        fx.ledger
            .cancel_asset_retirement(&ctx, tstx(), b"x")
            .map(|a| a.value),
        Err(Refusal::MalformedCommand)
    );
    assert_eq!(
        remove_tstx(&mut fx, registrar),
        Err(Refusal::MalformedCommand)
    );
    assert!(fx.ledger.take_calls().is_empty(), "no call recorded");
    assert_eq!(fx.ledger.state().audit().outbox().count(), audit);
    still_registered(&fx, &asset);
}

/// `REQ-AS-10`: a book that entered the rules version by an upgrade, with accounts and grants
/// written under earlier ones, sweeps them like any other: nothing is indexed for the removal.
#[test]
fn req_as_10_a_book_that_entered_the_version_by_an_upgrade_sweeps_its_old_records() {
    let mut fx = Fx::new();
    fx.ledger.set_generation(ASSET_RETIREMENT - 1, false);
    let registerer = staff(&mut fx, &[OperatorOpKind::RegisterAsset]);
    register_tstx(&mut fx, registerer, 8, 1);
    let org = fx.person();
    let ctx = fx.ctx(fx.operator());
    let treasury = fx
        .ledger
        .open_account(
            &ctx,
            OpenAccount {
                owner: org.principal,
                kind: AccountKind::Treasury,
                custody: Custody::Delegated,
                treasury_limits: BTreeMap::from([(tstx(), Minor::new(9))]),
                acknowledged: None,
            },
        )
        .unwrap()
        .value;
    // The upgrade: the inputs that follow are decided under the removal's rules version.
    fx.ledger.set_generation(ASSET_RETIREMENT, false);
    let registrar = staff(&mut fx, &[OperatorOpKind::RemoveAsset]);
    let next = retire_tstx(&mut fx, registrar).unwrap();
    assert_eq!(
        sweep_all(&mut fx, registrar, next),
        Err(Refusal::NotAuthorised)
    );
    assert!(
        last_audit(&fx)
            .subject
            .contains(&AuditSubject::Account(treasury))
    );
    assert_eq!(
        fx.ledger.state().registry().get(&tstx()).unwrap().status(),
        AssetStatus::Retiring
    );
}

fn issuance_request(to: IdentityId, per_window: Option<u64>) -> GrantRequest {
    GrantRequest {
        kind: GrantKind::Issuance,
        principal: to,
        subject: Subject::Asset(usd()),
        ops: NonEmptySet::one(OperationKind::Issue),
        per_operation: None,
        per_window: per_window.map(|n| Window {
            length: NonZeroU64::new(1_000).unwrap(),
            limit: nz(n),
        }),
        assets: Some(NonEmptySet::one(usd())),
        counterparties: None,
        parent: None,
        expires: None,
    }
}

#[test]
fn req_au_10_issuance_grants_on_the_operator_surface_are_bounded_and_revocable() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Owned);
    let treasurer = fx.person();
    let admin = staff(&mut fx, &[OperatorOpKind::IssuanceAuthority]);
    // Without a per-window bound: unbounded.
    let ctx = fx.ctx(admin);
    assert_eq!(
        fx.ledger
            .grant_issuance(&ctx, issuance_request(treasurer.principal, None), b"x")
            .map(|a| a.value),
        Err(Refusal::UnboundedGrant)
    );
    let ctx = fx.ctx(admin);
    let grant = fx
        .ledger
        .grant_issuance(
            &ctx,
            issuance_request(treasurer.principal, Some(100)),
            b"treasury desk, CR-12",
        )
        .unwrap();
    assert_eq!(grant.record.authorities[0].kind, AuthorityKind::Operator);
    let grant = grant.value;
    // Within its window it issues; past it, the bound holds.
    let ctx = fx.ctx(treasurer);
    fx.ledger.issue(&ctx, usd(), acct, Minor::new(60)).unwrap();
    let ctx = fx.ctx(treasurer);
    assert_eq!(
        fx.ledger
            .issue(&ctx, usd(), acct, Minor::new(41))
            .map(|a| a.value),
        Err(Refusal::BoundExceeded)
    );
    // Revoked on the operator surface: it stops at once.
    let ctx = fx.ctx(admin);
    fx.ledger
        .revoke_issuance(&ctx, grant, b"desk closed")
        .unwrap();
    let ctx = fx.ctx(treasurer);
    assert_eq!(
        fx.ledger
            .issue(&ctx, usd(), acct, Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::AuthorityRevoked)
    );
    let ctx = fx.ctx(admin);
    assert_eq!(
        fx.ledger
            .revoke_issuance(&ctx, grant, b"again")
            .map(|a| a.value),
        Err(Refusal::AuthorityRevoked)
    );
    // Only issuance grants are revoked here.
    let other = fx.delegate(
        &alice,
        treasurer.principal,
        acct,
        &[OperationKind::Reserve],
        5,
    );
    let ctx = fx.ctx(admin);
    assert_eq!(
        fx.ledger
            .revoke_issuance(&ctx, other, b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

// ---------------------------------------------------------------- T-013-08: adapter support

#[test]
fn err_unsupported_leg_for_an_undeclared_leg_and_asset() {
    let mut fx = Fx::new();
    // Nothing declared: nothing is supported.
    assert_eq!(
        fx.ledger.require_supported(OperationKind::Redeem, &usd()),
        Err(Refusal::UnsupportedLeg)
    );
    let adapter = staff(&mut fx, &[OperatorOpKind::DeclareAdapterSupport]);
    let ctx = fx.ctx(adapter);
    assert_eq!(
        fx.ledger
            .declare_adapter_support(
                &ctx,
                [(OperationKind::Redeem, AssetId::parse("NOPE").unwrap())].into(),
                b"start-up",
            )
            .map(|a| a.value),
        Err(Refusal::UnknownAsset)
    );
    let ctx = fx.ctx(adapter);
    fx.ledger
        .declare_adapter_support(&ctx, [(OperationKind::Redeem, usd())].into(), b"start-up")
        .unwrap();
    assert_eq!(
        fx.ledger.require_supported(OperationKind::Redeem, &usd()),
        Ok(())
    );
    assert_eq!(
        fx.ledger.require_supported(OperationKind::Issue, &usd()),
        Err(Refusal::UnsupportedLeg)
    );
    assert_eq!(
        fx.ledger.require_supported(OperationKind::Redeem, &old()),
        Err(Refusal::UnsupportedLeg)
    );
    // A new declaration replaces the last.
    let ctx = fx.ctx(adapter);
    fx.ledger
        .declare_adapter_support(&ctx, [(OperationKind::Issue, usd())].into(), b"restart")
        .unwrap();
    assert_eq!(
        fx.ledger.require_supported(OperationKind::Redeem, &usd()),
        Err(Refusal::UnsupportedLeg)
    );
    assert_eq!(
        fx.ledger.require_supported(OperationKind::Issue, &usd()),
        Ok(())
    );
    assert_eq!(
        last_audit(&fx).action,
        AuditAction::Command(Command::DeclareAdapterSupport)
    );
    // Only kinds with an external leg are the adapter's to support.
    let ctx = fx.ctx(adapter);
    assert_eq!(
        fx.ledger
            .declare_adapter_support(&ctx, [(OperationKind::Transfer, usd())].into(), b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // It is the declaration `withdraw` reads: an undeclared leg is refused at submission.
    let (alice, acct) = funded(&mut fx, 10);
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .withdraw(
                &ctx,
                &crate::ledger::settlement::Withdrawal {
                    payer: acct,
                    asset: usd(),
                    amount: Minor::new(5),
                    deadline: Some(1_000),
                },
            )
            .map(|a| a.value),
        Err(Refusal::UnsupportedLeg)
    );
}
