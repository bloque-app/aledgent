// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Aliases after genesis and the grants on an account (`REQ-RC-7`, amendment 0024; ruling
//! `system-accounts-operator`): the recipe of `ops/runbooks/system-accounts.md`.

use super::operator::last_audit;
use super::*;
use crate::audit::{AuditAction, AuditOutcome, AuditSubject};
use crate::opening::OpeningSource;
use crate::state::Command;

const ROLE: OpeningSource = OpeningSource(*b"system-role\0\0\0\0\0");

fn register(
    fx: &mut Fx,
    by: Caller,
    name: &[u8],
    account: AccountId,
    why: &[u8],
) -> Result<(), Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger
        .register_alias(&ctx, ROLE, name, account, why)
        .map(|a| a.value)
}

/// `REQ-RC-7`: the operator opens a system account under its own identity, names it by its role,
/// grants a service principal account-scoped authority on it; the service resolves the role, and
/// the operator reads the grants on the account.
#[test]
fn req_rc_7_a_system_account_by_its_role_and_its_grants() {
    let mut fx = Fx::new();
    let op = fx.operator();
    let fee = fx.account(&op, Custody::Delegated);
    register(&mut fx, op, b"issuer/fee", fee, b"new fee account, issue 1").unwrap();
    let r = last_audit(&fx);
    assert_eq!(r.action, AuditAction::Command(Command::RegisterAlias));
    assert_eq!(r.outcome, AuditOutcome::Accepted);
    assert!(r.subject.contains(&AuditSubject::Account(fee)));
    let now = 10;
    assert_eq!(fx.ledger.alias(&op, &ROLE, b"issuer/fee", now), Ok(fee));

    let issuer = fx.person();
    // Before its grant the service reaches nothing: the role is as unknown as a missing one.
    assert_eq!(
        fx.ledger.alias(&issuer, &ROLE, b"issuer/fee", now),
        Err(Refusal::UnknownAccount)
    );
    let grant = fx.delegate(
        &op,
        issuer.principal,
        fee,
        &[OperationKind::Transfer],
        1_000,
    );
    assert_eq!(fx.ledger.alias(&issuer, &ROLE, b"issuer/fee", now), Ok(fee));
    assert_eq!(
        fx.ledger.alias(&issuer, &ROLE, b"issuer/none", now),
        Err(Refusal::UnknownAccount)
    );
    let grants = fx.ledger.grants_on(&op, &fee, now).unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].id(), grant);
    assert_eq!(grants[0].principal(), issuer.principal);
    // The grants are the operator's and the owner's to read, not a grantee's.
    assert_eq!(
        fx.ledger.grants_on(&issuer, &fee, now).map(|g| g.len()),
        Err(Refusal::NotAuthorised)
    );
    let stranger = fx.person();
    assert!(fx.ledger.grants_on(&stranger, &fee, now).is_err());
}

/// `REQ-RC-7`: an alias names one account for good; only the operator registers one, with a
/// justification, for an account the book holds and that is not closed.
#[test]
fn req_rc_7_an_alias_is_the_operators_and_names_one_account_for_good() {
    let mut fx = Fx::new();
    let op = fx.operator();
    let a = fx.account(&op, Custody::Delegated);
    let b = fx.account(&op, Custody::Delegated);
    register(&mut fx, op, b"operations/main", a, b"x").unwrap();
    assert_eq!(
        register(&mut fx, op, b"operations/main", b, b"re-point"),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        register(&mut fx, op, b"operations/main", a, b"again"),
        Err(Refusal::NotAuthorised)
    );
    let alice = fx.person();
    let own = fx.account(&alice, Custody::Delegated);
    assert_eq!(
        register(&mut fx, alice, b"alice/main", own, b"self-service"),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        register(&mut fx, op, b"x/y", b, b""),
        Err(Refusal::JustificationRequired)
    );
    assert_eq!(
        register(&mut fx, op, b"", b, b"empty"),
        Err(Refusal::MalformedCommand)
    );
    let unknown = AccountId::mint(&OperationId::from_ref(OperationRef([0xEE; 16])), 3);
    assert_eq!(
        register(&mut fx, op, b"x/z", unknown, b"unknown"),
        Err(Refusal::UnknownAccount)
    );
    let issuance = fx.founded.issuance;
    assert_eq!(
        register(&mut fx, op, b"x/i", issuance, b"the counterparty"),
        Err(Refusal::UnknownAccount)
    );
    let r = last_audit(&fx);
    assert_eq!(r.outcome, AuditOutcome::Refused(Refusal::UnknownAccount));
}
