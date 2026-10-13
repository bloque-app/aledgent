// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The identities principal and who opens accounts (`REQ-ID-6`, `US-A2`, amendment 0025): the
//! operator grants the identities principal's role to another principal, whose holder opens
//! identities and accounts for any identity; a delegated grant over an identity opens a Customer
//! account in delegated custody for it, and nothing else.

use super::operator::grant_kinds;
use super::*;
use crate::operator::OperatorOpKind;

fn open_person(fx: &mut Fx, by: Caller) -> Result<Accepted<IdentityOpened>, Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger
        .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![9]))
}

fn open_for(
    fx: &mut Fx,
    by: Caller,
    owner: IdentityId,
    kind: AccountKind,
    custody: Custody,
) -> Result<Accepted<AccountId>, Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger.open_account(
        &ctx,
        OpenAccount {
            owner,
            kind,
            custody,
            treasury_limits: BTreeMap::new(),
            acknowledged: None,
        },
    )
}

/// A grant over `identity` to `to`, naming `ops`, bounded per operation.
fn over_identity(
    fx: &mut Fx,
    by: Caller,
    to: IdentityId,
    identity: IdentityId,
    ops: &[OperationKind],
) -> Result<GrantId, Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: to,
                subject: Subject::Identity(identity),
                ops: NonEmptySet::new(ops.iter().copied().collect()).unwrap(),
                per_operation: Some(nz(1)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .map(|a| a.value)
}

/// An organisation holding the identities principal's role, and the grant that gives it.
fn onboarding(fx: &mut Fx) -> (Caller, GrantId) {
    let ctx = fx.ctx(fx.operator());
    let o = fx
        .ledger
        .open_identity(
            &ctx,
            IdentityKind::Organisation,
            None,
            Verifier::new(vec![4]),
        )
        .unwrap()
        .value;
    let who = Caller::new(o.identity, o.credential);
    let g = grant_kinds(fx, who.principal, &[OperatorOpKind::OpenIdentity]);
    (who, g)
}

#[test]
fn req_id_6_the_identities_principal_role_opens_identities_and_accounts_for_any_identity() {
    let mut fx = Fx::new();
    let (onboarding, role) = onboarding(&mut fx);

    let opened = open_person(&mut fx, onboarding).expect("the role opens an identity");
    let used = &opened.record.authorities[0];
    assert_eq!(
        (used.kind, used.grant, used.principal),
        (AuthorityKind::Operator, Some(role), onboarding.principal),
        "attributed to the role's grant, not to the operator"
    );
    let user = opened.value.identity;

    // Any kind but Issuance, for any identity, as the operator's policy authority opens them.
    let a = open_for(
        &mut fx,
        onboarding,
        user,
        AccountKind::Customer,
        Custody::Delegated,
    )
    .expect("the role opens an account for the identity");
    assert_eq!(a.record.authorities[0].grant, Some(role));
    assert_eq!(fx.ledger.state().account(&a.value).unwrap().owner(), user);
    assert_eq!(
        open_for(
            &mut fx,
            onboarding,
            user,
            AccountKind::Issuance,
            Custody::Delegated
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );

    // Without the role, a principal opens neither.
    let stranger = fx.person();
    assert_eq!(
        open_person(&mut fx, stranger).err(),
        Some(Refusal::NotAuthorised)
    );
    assert_eq!(
        open_for(
            &mut fx,
            stranger,
            user,
            AccountKind::Customer,
            Custody::Delegated
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );

    // Revoked, it stops at once.
    let ctx = fx.ctx(fx.operator());
    fx.ledger
        .revoke_operator(&ctx, role, b"rotation")
        .expect("the operator revokes the role");
    assert_eq!(
        open_person(&mut fx, onboarding).err(),
        Some(Refusal::AuthorityRevoked)
    );
}

#[test]
fn req_id_6_only_the_operator_grants_the_role() {
    let mut fx = Fx::new();
    let (onboarding, _) = onboarding(&mut fx);
    let other = fx.person();
    let ctx = fx.ctx(onboarding);
    assert_eq!(
        fx.ledger
            .grant_operator(
                &ctx,
                other.principal,
                NonEmptySet::one(OperatorOpKind::OpenIdentity),
                None,
                b"passing it on",
            )
            .err(),
        Some(Refusal::NotAuthorised)
    );
}

#[test]
fn req_id_6_a_delegated_grant_over_an_identity_opens_its_customer_accounts_in_delegated_custody() {
    let mut fx = Fx::new();
    let (onboarding, _) = onboarding(&mut fx);
    let user = open_person(&mut fx, onboarding).unwrap().value.identity;
    let other = open_person(&mut fx, onboarding).unwrap().value.identity;
    let custodian = fx.person();

    // Before any grant, the custodian opens nothing for the user.
    assert_eq!(
        open_for(
            &mut fx,
            custodian,
            user,
            AccountKind::Customer,
            Custody::Delegated
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );

    // The identities principal grants it over the user.
    let g = over_identity(
        &mut fx,
        onboarding,
        custodian.principal,
        user,
        &[OperationKind::OpenAccount],
    )
    .expect("the identities principal grants over the identity");
    let a = open_for(
        &mut fx,
        custodian,
        user,
        AccountKind::Customer,
        Custody::Delegated,
    )
    .expect("the grant opens a Customer account in delegated custody");
    let used = &a.record.authorities[0];
    assert_eq!((used.kind, used.grant), (AuthorityKind::Delegated, Some(g)));
    assert_eq!(fx.ledger.state().account(&a.value).unwrap().owner(), user);

    // Nothing more: another identity, another kind, owned custody.
    for (owner, kind, custody) in [
        (other, AccountKind::Customer, Custody::Delegated),
        (user, AccountKind::Treasury, Custody::Delegated),
        (user, AccountKind::Customer, Custody::Owned),
    ] {
        assert_eq!(
            open_for(&mut fx, custodian, owner, kind, custody).err(),
            Some(Refusal::NotAuthorised),
            "{kind:?} {custody:?}"
        );
    }
    // It opens identities no more than before.
    assert_eq!(
        open_person(&mut fx, custodian).err(),
        Some(Refusal::NotAuthorised)
    );

    // The identities principal revokes it, and it stops at once.
    let ctx = fx.ctx(onboarding);
    fx.ledger.revoke_authority(&ctx, g).unwrap();
    assert_eq!(
        open_for(
            &mut fx,
            custodian,
            user,
            AccountKind::Customer,
            Custody::Delegated
        )
        .err(),
        Some(Refusal::AuthorityRevoked)
    );
}

#[test]
fn req_id_6_the_identity_itself_delegates_opening_its_accounts() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let helper = fx.person();
    let g = over_identity(
        &mut fx,
        alice,
        helper.principal,
        alice.principal,
        &[OperationKind::OpenAccount],
    )
    .expect("the identity grants over itself");
    let a = open_for(
        &mut fx,
        helper,
        alice.principal,
        AccountKind::Customer,
        Custody::Delegated,
    )
    .unwrap();
    assert_eq!(a.record.authorities[0].grant, Some(g));
    // The identity revokes it.
    let ctx = fx.ctx(alice);
    fx.ledger.revoke_authority(&ctx, g).unwrap();
}

#[test]
fn req_au_2_a_grant_over_an_identity_names_open_account_and_nothing_else_names_it() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let bob = fx.person();
    let agent = fx.agent(alice.principal);
    let op = fx.operator();
    // Over an identity, `OpenAccount` always, and never `Issue`.
    assert_eq!(
        over_identity(
            &mut fx,
            alice,
            bob.principal,
            alice.principal,
            &[OperationKind::Transfer],
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );
    assert_eq!(
        over_identity(
            &mut fx,
            alice,
            bob.principal,
            alice.principal,
            &[OperationKind::OpenAccount, OperationKind::Issue],
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );
    // `OpenAccount` over an account: refused.
    let acct = fx.account(&alice, Custody::Delegated);
    let ctx = fx.ctx(alice);
    let r = fx.ledger.grant_authority(
        &ctx,
        GrantRequest {
            kind: GrantKind::Delegated,
            principal: bob.principal,
            subject: Subject::Account(acct),
            ops: NonEmptySet::one(OperationKind::OpenAccount),
            per_operation: Some(nz(1)),
            per_window: None,
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        },
    );
    assert_eq!(r.err(), Some(Refusal::NotAuthorised));
    // Unbounded: refused, as every grant (`REQ-AU-2`).
    let ctx = fx.ctx(alice);
    let r = fx.ledger.grant_authority(
        &ctx,
        GrantRequest {
            kind: GrantKind::Delegated,
            principal: bob.principal,
            subject: Subject::Identity(alice.principal),
            ops: NonEmptySet::one(OperationKind::OpenAccount),
            per_operation: None,
            per_window: None,
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        },
    );
    assert_eq!(r.err(), Some(Refusal::UnboundedGrant));
    // Of another kind than delegated: refused.
    let ctx = fx.ctx(op);
    let r = fx.ledger.grant_authority(
        &ctx,
        GrantRequest {
            kind: GrantKind::Operator,
            principal: bob.principal,
            subject: Subject::Identity(alice.principal),
            ops: NonEmptySet::one(OperationKind::OpenAccount),
            per_operation: Some(nz(1)),
            per_window: None,
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        },
    );
    assert_eq!(r.err(), Some(Refusal::NotAuthorised));
    // By a stranger to the identity: refused.
    assert_eq!(
        over_identity(
            &mut fx,
            bob,
            bob.principal,
            alice.principal,
            &[OperationKind::OpenAccount],
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );
    // Over an agent, which owns no account: refused.
    assert_eq!(
        over_identity(
            &mut fx,
            op,
            bob.principal,
            agent.principal,
            &[OperationKind::OpenAccount],
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );
    // The operator, the identities principal by policy, grants it.
    assert!(
        over_identity(
            &mut fx,
            op,
            bob.principal,
            alice.principal,
            &[OperationKind::OpenAccount],
        )
        .is_ok()
    );
}

// ------------------------------------------------- clause R2: register_credential for a managed identity

fn register_for(
    fx: &mut Fx,
    by: Caller,
    marker: CredentialMarker,
) -> Result<Accepted<CredentialId>, Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger.register_credential(
        &ctx,
        crate::ledger::custody::RegisterCredential {
            marker,
            scope: None,
            verifier: Verifier::new(vec![0x42]),
        },
    )
}

/// Amendment 0025 clause R2 (ruling `pk-register`): the identities principal adds a
/// credential to the set of an identity it manages, acting for it, as the operator-held key would
/// (`REQ-AU-20`); attributed to the role's grant; never for an identity with an owned account, the
/// operator, or a holder-held recovery credential; never without the role.
#[test]
fn req_au_20_the_identities_principal_registers_a_credential_for_a_managed_identity() {
    let mut fx = Fx::new();
    let (onboarding, role) = onboarding(&mut fx);
    let user = open_person(&mut fx, onboarding).unwrap().value.identity;
    open_for(
        &mut fx,
        onboarding,
        user,
        AccountKind::Customer,
        Custody::Delegated,
    )
    .unwrap();

    let r = register_for(
        &mut fx,
        onboarding.on_behalf_of(user),
        CredentialMarker::Ordinary,
    )
    .expect("the role registers into the user's set");
    assert_eq!(
        fx.ledger.state().credential(&r.value).unwrap().identity(),
        user,
        "the credential joins the user's set, not onboarding's"
    );
    let used = &r.record.authorities[0];
    assert_eq!(
        (used.kind, used.grant, used.principal, used.on_behalf_of),
        (
            AuthorityKind::Operator,
            Some(role),
            onboarding.principal,
            Some(user)
        )
    );

    // Not a holder-held recovery credential: that is the holder's own act.
    assert_eq!(
        register_for(
            &mut fx,
            onboarding.on_behalf_of(user),
            CredentialMarker::RecoveryHeld
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );
    // Not without the role.
    let stranger = fx.person();
    assert_eq!(
        register_for(
            &mut fx,
            stranger.on_behalf_of(user),
            CredentialMarker::Ordinary
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );
    // Not for the operator.
    let operator = fx.founded.operator;
    assert_eq!(
        register_for(
            &mut fx,
            onboarding.on_behalf_of(operator),
            CredentialMarker::Ordinary
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );
    // Not for an identity holding an account in owned custody (`REQ-AU-6`).
    let holder = fx.person();
    fx.account(&holder, Custody::Owned);
    assert_eq!(
        register_for(
            &mut fx,
            onboarding.on_behalf_of(holder.principal),
            CredentialMarker::Ordinary
        )
        .err(),
        Some(Refusal::NotAuthorised)
    );
}

// ------------------------------------------------- clause R3: broad grants never reach system accounts

/// Ruling `au4-q2`, amendment 0025 R3: an operator grant over every delegated account does not
/// cover an account the operator's identity owns (a system account), even in delegated custody;
/// an account-scoped grant on it does. A customer's delegated account is still covered.
#[test]
fn au4_q2_a_broad_grant_never_reaches_a_system_account_and_an_account_scoped_one_does() {
    let mut fx = Fx::new();
    let op = fx.operator();
    let system = fx.account(&op, Custody::Delegated);
    let alice = fx.person();
    let customer = fx.account(&alice, Custody::Delegated);
    let custodian = fx.person();
    let issuer = fx.person();
    let ctx = fx.ctx(op);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Operator,
                principal: custodian.principal,
                subject: Subject::DelegatedAccounts,
                ops: NonEmptySet::one(OperationKind::Transfer),
                per_operation: Some(nz(1_000)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    fx.delegate(
        &op,
        issuer.principal,
        system,
        &[OperationKind::Transfer],
        1_000,
    );

    let ctx = fx.ctx(custodian);
    assert!(
        fx.ledger.authorise(&ctx, &fx.transfer(customer, 1)).is_ok(),
        "the broad grant covers a customer's delegated account"
    );
    assert_eq!(
        fx.ledger.authorise(&ctx, &fx.transfer(system, 1)).err(),
        Some(Refusal::UnknownAccount),
        "and never the operator's system account"
    );
    let ctx = fx.ctx(issuer);
    assert!(
        fx.ledger.authorise(&ctx, &fx.transfer(system, 1)).is_ok(),
        "the account-scoped grant reaches it"
    );
}

// ------------------------------------------------- R1: one holder of the identities principal's role

#[test]
fn a0025_q1_granting_the_role_moves_it_and_the_old_holder_loses_it() {
    let mut fx = Fx::new();
    let (first, old_grant) = onboarding(&mut fx);
    let second = fx.person();
    let new_grant = grant_kinds(&mut fx, second.principal, &[OperatorOpKind::OpenIdentity]);
    assert!(
        fx.ledger
            .state()
            .ops()
            .grant(&old_grant)
            .unwrap()
            .revoked()
            .is_some(),
        "the old holder's grant is revoked in the same commit"
    );
    assert_eq!(
        open_person(&mut fx, first).err(),
        Some(Refusal::AuthorityRevoked)
    );
    let r = open_person(&mut fx, second).expect("the new holder opens identities");
    assert_eq!(r.record.authorities[0].grant, Some(new_grant));
    // The operator keeps it.
    let op = fx.operator();
    assert!(open_person(&mut fx, op).is_ok());
    // The role is a grant of its own: never with other kinds.
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger
            .grant_operator(
                &ctx,
                first.principal,
                NonEmptySet::new(
                    [OperatorOpKind::OpenIdentity, OperatorOpKind::ReadSignals].into()
                )
                .unwrap(),
                None,
                b"bundled",
            )
            .err(),
        Some(Refusal::NotAuthorised)
    );
}

// ------------------------------------------------- R4: the opener grants the issuer, never wider

/// A grant over an identity naming `ops`, with a per-operation bound of `per_op`.
fn identity_grant(
    fx: &mut Fx,
    by: Caller,
    to: IdentityId,
    identity: IdentityId,
    ops: &[OperationKind],
    per_op: u64,
) -> GrantId {
    let ctx = fx.ctx(by);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: to,
                subject: Subject::Identity(identity),
                ops: NonEmptySet::new(ops.iter().copied().collect()).unwrap(),
                per_operation: Some(nz(per_op)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap()
        .value
}

fn account_grant(
    fx: &mut Fx,
    by: Caller,
    to: IdentityId,
    account: AccountId,
    ops: &[OperationKind],
    per_op: u64,
    parent: Option<GrantId>,
) -> Result<GrantId, Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: to,
                subject: Subject::Account(account),
                ops: NonEmptySet::new(ops.iter().copied().collect()).unwrap(),
                per_operation: Some(nz(per_op)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent,
                expires: None,
            },
        )
        .map(|a| a.value)
}

/// Rulings `a0025-q4`: whoever opens an account grants the issuer its per-account grant. The custodian does
/// it under its grant over the identity, never wider than that grant, and the grant over the
/// identity reaches nothing by itself; onboarding does it as the identities principal.
#[test]
fn a0025_q4_the_opener_grants_the_issuer_its_per_account_grant_and_nothing_broader() {
    use OperationKind::{OpenAccount, Redeem, Reserve, Transfer};
    let mut fx = Fx::new();
    let (onboarding, _) = onboarding(&mut fx);
    let user = open_person(&mut fx, onboarding).unwrap().value.identity;
    let other = open_person(&mut fx, onboarding).unwrap().value.identity;
    let custodian = fx.person();
    let issuer = fx.person();
    let ig = identity_grant(
        &mut fx,
        onboarding,
        custodian.principal,
        user,
        &[OpenAccount, Transfer, Reserve],
        500,
    );
    let virtual_ = open_for(
        &mut fx,
        custodian,
        user,
        AccountKind::Customer,
        Custody::Delegated,
    )
    .unwrap()
    .value;
    let elsewhere = open_for(
        &mut fx,
        onboarding,
        other,
        AccountKind::Customer,
        Custody::Delegated,
    )
    .unwrap()
    .value;

    // Within the grant over the identity: allowed.
    let sg = account_grant(
        &mut fx,
        custodian,
        issuer.principal,
        virtual_,
        &[Transfer],
        100,
        Some(ig),
    )
    .expect("the custodian grants the issuer over the account it opened");
    let ctx = fx.ctx(issuer);
    assert!(
        fx.ledger
            .authorise(&ctx, &fx.transfer(virtual_, 100))
            .is_ok()
    );
    assert_eq!(
        fx.ledger.authorise(&ctx, &fx.transfer(virtual_, 101)).err(),
        Some(Refusal::BoundExceeded),
        "its own bound"
    );
    // Wider kinds than the grant over the identity: the meet leaves them out.
    let wide = account_grant(
        &mut fx,
        custodian,
        issuer.principal,
        virtual_,
        &[Redeem],
        1,
        Some(ig),
    );
    if let Ok(g) = wide {
        let ctx = fx.ctx(issuer);
        let r = Request {
            kind: Redeem,
            ..fx.transfer(virtual_, 1)
        };
        assert!(fx.ledger.authorise(&ctx, &r).is_err(), "{g:?} is empty");
    }
    // A larger bound is cut to the parent's.
    account_grant(
        &mut fx,
        custodian,
        issuer.principal,
        virtual_,
        &[Reserve],
        10_000,
        Some(ig),
    )
    .unwrap();
    let ctx = fx.ctx(issuer);
    let reserve = |n| Request {
        kind: Reserve,
        ..fx.transfer(virtual_, n)
    };
    assert_eq!(
        fx.ledger.authorise(&ctx, &reserve(501)).err(),
        Some(Refusal::BoundExceeded),
        "never above the grant over the identity"
    );
    // Over another identity's account, over the identity itself, or with no parent: refused.
    assert!(
        account_grant(
            &mut fx,
            custodian,
            issuer.principal,
            elsewhere,
            &[Transfer],
            1,
            Some(ig)
        )
        .is_err()
    );
    assert!(
        account_grant(
            &mut fx,
            custodian,
            issuer.principal,
            virtual_,
            &[Transfer],
            1,
            None
        )
        .is_err()
    );
    let ctx = fx.ctx(custodian);
    assert_eq!(
        fx.ledger
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Delegated,
                    principal: issuer.principal,
                    subject: Subject::Identity(user),
                    ops: NonEmptySet::one(OpenAccount),
                    per_operation: Some(nz(1)),
                    per_window: None,
                    assets: None,
                    counterparties: None,
                    parent: Some(ig),
                    expires: None,
                },
            )
            .err(),
        Some(Refusal::NotAuthorised),
        "a grant over an identity is never passed on"
    );
    // The grant over the identity reaches no account by itself.
    let ctx = fx.ctx(custodian);
    assert_eq!(
        fx.ledger.authorise(&ctx, &fx.transfer(virtual_, 1)).err(),
        Some(Refusal::UnknownAccount)
    );

    // Onboarding, as the identities principal, grants the issuer directly over an account it opened.
    let default = open_for(
        &mut fx,
        onboarding,
        user,
        AccountKind::Customer,
        Custody::Delegated,
    )
    .unwrap()
    .value;
    account_grant(
        &mut fx,
        onboarding,
        issuer.principal,
        default,
        &[Transfer],
        100,
        None,
    )
    .expect("onboarding grants the issuer over the default account");
    // Not over the operator's own account.
    let op = fx.operator();
    let system = fx.account(&op, Custody::Delegated);
    assert!(
        account_grant(
            &mut fx,
            onboarding,
            issuer.principal,
            system,
            &[Transfer],
            1,
            None
        )
        .is_err()
    );

    // Revoking the custodian's grant over the identity stops the issuer's grant made under it (REQ-AU-3).
    let ctx = fx.ctx(onboarding);
    fx.ledger.revoke_authority(&ctx, ig).unwrap();
    let ctx = fx.ctx(issuer);
    assert_eq!(
        fx.ledger.authorise(&ctx, &fx.transfer(virtual_, 1)).err(),
        Some(Refusal::AuthorityRevoked)
    );
    let _ = sg;
}

// ------------------------------------------------- amendment 0026: grants(identity)

/// Amendment 0026: a holder reads its own grants over an identity, with their ids, and names one
/// as the `parent` of the account grant it makes; the identity itself (`a0026-q1` (a)), the
/// operator and the identities principal read every grant over the identity, including ones the
/// identity did not grant; anyone else is refused.
#[test]
fn a0026_a_holder_reads_its_grant_over_an_identity_and_names_it_as_parent() {
    use OperationKind::{OpenAccount, Transfer};
    let mut fx = Fx::new();
    let now = 10;
    let (onboarding, _) = onboarding(&mut fx);
    let opened = open_person(&mut fx, onboarding).unwrap().value;
    let user = opened.identity;
    let custodian = fx.person();
    let other = fx.person();
    let issuer = fx.person();
    let ig = identity_grant(
        &mut fx,
        onboarding,
        custodian.principal,
        user,
        &[OpenAccount, Transfer],
        500,
    );
    let og = identity_grant(
        &mut fx,
        onboarding,
        other.principal,
        user,
        &[OpenAccount],
        1,
    );

    // The holder sees its own grant, and only its own.
    let mine = fx
        .ledger
        .grants_over_identity(&custodian, &user, now)
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id(), ig);
    assert_eq!(mine[0].principal(), custodian.principal);
    assert_eq!(mine[0].subject(), Subject::Identity(user));
    assert!(mine[0].ops().contains(&Transfer));
    assert_eq!(mine[0].parent(), None);

    // The id it read is the parent of the account grant it makes under it (amendment 0025 R4).
    let virtual_ = open_for(
        &mut fx,
        custodian,
        user,
        AccountKind::Customer,
        Custody::Delegated,
    )
    .unwrap()
    .value;
    account_grant(
        &mut fx,
        custodian,
        issuer.principal,
        virtual_,
        &[Transfer],
        100,
        Some(mine[0].id()),
    )
    .expect("the custodian grants the issuer with the parent it read");
    let ctx = fx.ctx(issuer);
    assert!(
        fx.ledger
            .authorise(&ctx, &fx.transfer(virtual_, 100))
            .is_ok()
    );

    // The identity itself, the operator and the identities principal see every grant over the
    // identity: both were granted by onboarding, not by the identity.
    let op = fx.operator();
    let me = Caller {
        principal: user,
        credential: opened.credential,
        on_behalf_of: None,
    };
    for who in [me, op, onboarding] {
        let all: Vec<GrantId> = fx
            .ledger
            .grants_over_identity(&who, &user, now)
            .unwrap()
            .iter()
            .map(|g| g.id())
            .collect();
        let mut want = vec![ig, og];
        want.sort();
        assert_eq!(all, want);
    }

    let seen = fx.ledger.grants_over_identity(&me, &user, now).unwrap();
    assert!(
        seen.iter().all(|g| g.grantor() != user),
        "none granted by the identity"
    );

    // Anyone else holding none is refused: the issuer (its grant is over an account) and a stranger.
    let stranger = fx.person();
    for who in [issuer, stranger] {
        assert_eq!(
            fx.ledger.grants_over_identity(&who, &user, now).err(),
            Some(Refusal::NotAuthorised)
        );
    }

    // A revoked grant is still its holder's to read, with when it was revoked.
    let ctx = fx.ctx(onboarding);
    fx.ledger.revoke_authority(&ctx, ig).unwrap();
    let mine = fx
        .ledger
        .grants_over_identity(&custodian, &user, now)
        .unwrap();
    assert_eq!(mine[0].id(), ig);
    assert!(mine[0].revoked().is_some());
}
