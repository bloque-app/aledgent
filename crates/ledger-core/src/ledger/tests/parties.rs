// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Integrating parties and the client key's domain (`REQ-ID-7`, `REQ-OP-10`, amendment 0011 R4,
//! `OQ-65`, T-009-16).

use super::operator::{last_audit, staff};
use super::*;
use crate::audit::{AuditAction, AuditClass, AuditOutcome, AuditSubject};
use crate::lifecycle::keys::{ClientKey, Surface, operation_reference};
use crate::operator::{OperatorOpKind, OpsRefusal, OpsWrite};
use crate::party::{KeyDomain, PartyId, PartyTie, RegisterIntegratingParty};
use crate::state::{Command, StoreRefusal};

/// A new organisation identity, opened by the operator.
fn organisation(fx: &mut Fx) -> IdentityId {
    let ctx = fx.ctx(fx.operator());
    fx.ledger
        .open_identity(
            &ctx,
            IdentityKind::Organisation,
            None,
            Verifier::new(vec![3]),
        )
        .unwrap()
        .value
        .identity
}

fn register(
    fx: &mut Fx,
    by: Caller,
    party: PartyId,
    principals: &[IdentityId],
    why: &[u8],
) -> Result<PartyId, Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger
        .register_integrating_party(
            &ctx,
            RegisterIntegratingParty {
                party,
                principals: principals.iter().copied().collect(),
            },
            why,
        )
        .map(|a| a.value)
}

fn tie(
    fx: &mut Fx,
    by: Caller,
    party: PartyId,
    principal: IdentityId,
    why: &[u8],
) -> Result<(), Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger
        .tie_principal(&ctx, party, principal, why)
        .map(|a| a.value)
}

/// A registered party, with `principals` tied to it.
fn party_with(fx: &mut Fx, principals: &[IdentityId]) -> PartyId {
    let party = PartyId::of(organisation(fx));
    let op = fx.operator();
    register(fx, op, party, principals, b"onboarding").unwrap();
    party
}

#[test]
fn req_id_7_the_operator_registers_a_party_and_ties_its_principals() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let bob = fx.person();
    let carol = fx.person();
    let party = PartyId::of(organisation(&mut fx));
    let op = fx.operator();
    assert_eq!(
        register(&mut fx, op, party, &[alice.principal], b"onboarding"),
        Ok(party)
    );
    // An accepted registration is audited, with its justification, as an operator intervention.
    let a = last_audit(&fx);
    assert_eq!(
        a.action,
        AuditAction::Command(Command::RegisterIntegratingParty)
    );
    assert_eq!(a.class, AuditClass::OperatorIntervention);
    assert_eq!(a.outcome, AuditOutcome::Accepted);
    assert!(a.justification.is_some());
    assert!(
        a.subject
            .contains(&AuditSubject::Identity(party.identity()))
    );
    assert!(a.subject.contains(&AuditSubject::Identity(alice.principal)));
    assert_eq!(fx.ledger.party_of(&alice.principal), Some(party));
    assert_eq!(fx.ledger.party_of(&bob.principal), None);

    // A principal is tied later, too.
    assert_eq!(
        tie(&mut fx, op, party, bob.principal, b"a second service"),
        Ok(())
    );
    assert_eq!(
        last_audit(&fx).action,
        AuditAction::Command(Command::TiePrincipal)
    );
    assert_eq!(fx.ledger.party_of(&bob.principal), Some(party));

    // The operator's list names the party and both principals; carol is in none.
    let list = fx.ledger.integrating_parties(&op).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].party.party, party);
    let tied: BTreeSet<IdentityId> = list[0].principals.iter().map(|t| t.principal).collect();
    assert_eq!(tied, BTreeSet::from([alice.principal, bob.principal]));
    assert_eq!(fx.ledger.party_of(&carol.principal), None);
    assert_eq!(
        fx.ledger.state().key_domain(&carol.principal),
        KeyDomain::Principal(carol.principal),
        "a principal tied to none is its own client-key domain"
    );
    assert_eq!(
        fx.ledger.state().key_domain(&alice.principal),
        KeyDomain::Party(party)
    );
}

#[test]
fn req_id_7_a_principal_is_tied_to_at_most_one_party() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let first = party_with(&mut fx, &[alice.principal]);
    let second = party_with(&mut fx, &[]);
    let op = fx.operator();
    // Not to a second party, at registration or later, and not to the same one twice.
    assert_eq!(
        tie(&mut fx, op, second, alice.principal, b"move her"),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        tie(&mut fx, op, first, alice.principal, b"again"),
        Err(Refusal::NotAuthorised)
    );
    let third = PartyId::of(organisation(&mut fx));
    assert_eq!(
        register(&mut fx, op, third, &[alice.principal], b"with her"),
        Err(Refusal::NotAuthorised)
    );
    assert!(
        fx.ledger.state().ops().party(&third).is_none(),
        "a refused registration registers nothing"
    );
    assert_eq!(fx.ledger.party_of(&alice.principal), Some(first));
    // The refusal is on the trail.
    let a = last_audit(&fx);
    assert_eq!(a.outcome, AuditOutcome::Refused(Refusal::NotAuthorised));
    // And the store refuses a second tie whoever writes it.
    let second_tie = PartyTie {
        principal: alice.principal,
        party: second,
        tied_by: fx.op(),
        tied_at: 11,
    };
    assert_eq!(
        fx.ledger
            .state
            .commit(&[Write::Ops(OpsWrite::Tie(second_tie))]),
        Err(StoreRefusal::Ops(OpsRefusal::Tied))
    );
}

#[test]
fn err_not_authorised_only_the_operator_registers_and_ties() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let org = organisation(&mut fx);
    let party = PartyId::of(org);
    // A principal holding every operator-surface kind but one, and then the identities
    // principal's role too (granted alone, amendment 0025), still cannot: no grant gives these to
    // another principal (`REQ-ID-7`).
    let kinds: Vec<OperatorOpKind> = OperatorOpKind::ALL
        .into_iter()
        .skip(1)
        .filter(|k| *k != OperatorOpKind::OpenIdentity)
        .collect();
    let ops = staff(&mut fx, &kinds);
    super::operator::grant_kinds(&mut fx, ops.principal, &[OperatorOpKind::OpenIdentity]);
    for who in [alice, ops] {
        assert_eq!(
            register(&mut fx, who, party, &[], b"self-service"),
            Err(Refusal::NotAuthorised)
        );
    }
    let op = fx.operator();
    register(&mut fx, op, party, &[], b"onboarding").unwrap();
    for who in [alice, ops] {
        assert_eq!(
            tie(&mut fx, who, party, alice.principal, b"self-service"),
            Err(Refusal::NotAuthorised)
        );
    }
    // A party registered already; a party that is a person, not an organisation; an unknown
    // party; an unknown principal.
    assert_eq!(
        register(&mut fx, op, party, &[], b"twice"),
        Err(Refusal::NotAuthorised)
    );
    let bob = fx.person();
    assert_eq!(
        register(&mut fx, op, PartyId::of(bob.principal), &[], b"a person"),
        Err(Refusal::NotAuthorised)
    );
    let stranger = PartyId::of(IdentityId::mint(
        &OperationId::from_ref(OperationRef([0xEE; 16])),
        0,
    ));
    assert_eq!(
        tie(&mut fx, op, stranger, bob.principal, b"unknown party"),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        tie(
            &mut fx,
            op,
            party,
            stranger.identity(),
            b"unknown principal"
        ),
        Err(Refusal::NotAuthorised)
    );
    // The list is the operator's.
    assert_eq!(
        fx.ledger.integrating_parties(&alice).map(|l| l.len()),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(fx.ledger.party_of(&alice.principal), None);
}

#[test]
fn err_justification_required_to_register_and_to_tie() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let party = PartyId::of(organisation(&mut fx));
    let op = fx.operator();
    assert_eq!(
        register(&mut fx, op, party, &[alice.principal], b"  "),
        Err(Refusal::JustificationRequired)
    );
    assert!(fx.ledger.state().ops().party(&party).is_none());
    register(&mut fx, op, party, &[], b"onboarding").unwrap();
    assert_eq!(
        tie(&mut fx, op, party, alice.principal, b""),
        Err(Refusal::JustificationRequired)
    );
    assert_eq!(fx.ledger.party_of(&alice.principal), None);
}

/// `REQ-OP-11`: a repeat of the same registration under the same reference is answered with
/// the first outcome; another request under it is `ERR-ClientKeyReused`.
#[test]
fn req_op_11_a_registration_is_idempotent_on_its_reference() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let party = PartyId::of(organisation(&mut fx));
    let ctx = fx.ctx(fx.operator());
    let req = RegisterIntegratingParty {
        party,
        principals: BTreeSet::from([alice.principal]),
    };
    let first = fx
        .ledger
        .register_integrating_party(&ctx, req.clone(), b"onboarding")
        .unwrap();
    let again = fx
        .ledger
        .register_integrating_party(&ctx, req.clone(), b"onboarding")
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(
        fx.ledger
            .register_integrating_party(&ctx, req, b"another reason")
            .map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
}

/// `REQ-OP-10`, `ERR-ClientKeyReused` (T-009-16): within one integrating party, the same client
/// key from another of its principals is refused, never answered with the first one's reply. The
/// same key from a principal of another party, or from one tied to none, is a request of its
/// own.
#[test]
fn err_client_key_reused_the_same_key_from_two_principals_of_one_party() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let bob = fx.person();
    let carol = fx.person();
    let dave = fx.person();
    party_with(&mut fx, &[alice.principal, bob.principal]);
    party_with(&mut fx, &[carol.principal]);
    let deployment = fx.ledger.config().deployment;
    let key = ClientKey::new(*b"open-account-1");
    let reference = |fx: &Fx, who: &Caller| {
        operation_reference(
            &deployment,
            &fx.ledger.state().key_domain(&who.principal),
            Surface::Command,
            &key,
        )
    };
    let open = |fx: &mut Fx, who: Caller| {
        let op = reference(fx, &who);
        let acknowledged = fx.ack(&who, Custody::Delegated);
        fx.ledger
            .open_account(
                &on(who, op),
                OpenAccount {
                    owner: who.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Delegated,
                    treasury_limits: BTreeMap::new(),
                    acknowledged,
                },
            )
            .map(|a| a.value)
    };
    assert_eq!(reference(&fx, &alice), reference(&fx, &bob));
    let first = open(&mut fx, alice).unwrap();
    let count = fx.ledger.state().all_accounts().count();
    assert_eq!(open(&mut fx, alice), Ok(first), "a repeat is answered");
    assert_eq!(
        open(&mut fx, bob),
        Err(Refusal::ClientKeyReused),
        "another principal of the party is refused, not answered with alice's account"
    );
    assert_eq!(fx.ledger.state().all_accounts().count(), count);
    // Another party's principal, and a principal tied to none: their own requests.
    let c = open(&mut fx, carol).unwrap();
    let d = open(&mut fx, dave).unwrap();
    assert!(c != first && d != first && c != d);
}

fn on(caller: Caller, op: OperationId) -> Ctx {
    Ctx {
        caller,
        op,
        now: 10,
    }
}
