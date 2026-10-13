// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of `F-028` in the core (`DEC-19`, amendment 0010): a verifier's scheme is bound
//! wherever it counts, its shape is checked wherever one joins a set, the operator imports
//! credentials under `REQ-MG-7` and `REQ-MG-8`, and a reclaim proof is the one its credential's
//! scheme admits.

use super::*;
use crate::custody::reclaim_challenge;
use crate::ledger::custody::{
    CredentialProof, ImportCredentials, ImportedCredential, MAX_IMPORTED_CREDENTIALS, Proof,
    ReclaimCustody, RegisterCredential,
};
use crate::principal::{CredentialStatus, Scheme};
use crate::scheme::PasskeyKey;
use cipher_clerk::crypto::SecretKey;

fn passkey(seed: u8) -> Verifier {
    let mut public_key = [seed; 65];
    public_key[0] = 0x04;
    Verifier::of(
        Scheme::WebAuthnEs256,
        PasskeyKey {
            handle: [seed; 32],
            public_key,
            rp_id: "api.bloque.app".into(),
            backup_eligible: true,
            sign_count: 0,
        }
        .encode(),
    )
}

fn api_key(seed: u8) -> Verifier {
    Verifier::of(Scheme::ApiKey, vec![seed; 32])
}

fn register(fx: &mut Fx, who: Caller, verifier: Verifier) -> Result<CredentialId, Refusal> {
    let ctx = fx.ctx(who);
    fx.ledger
        .register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::Ordinary,
                scope: None,
                verifier,
            },
        )
        .map(|a| a.value)
}

fn importer(fx: &mut Fx) -> Caller {
    super::operator::staff(fx, &[crate::operator::OperatorOpKind::ImportCredentials])
}

fn import(
    fx: &mut Fx,
    by: Caller,
    identity: IdentityId,
    verifiers: Vec<Verifier>,
) -> Result<Vec<CredentialId>, Refusal> {
    let ctx = fx.ctx(by);
    fx.ledger
        .import_credentials(
            &ctx,
            ImportCredentials {
                identity,
                credentials: verifiers
                    .into_iter()
                    .map(|verifier| ImportedCredential {
                        verifier,
                        scope: None,
                    })
                    .collect(),
            },
            b"migration from the prior system, batch 1",
        )
        .map(|a| a.value)
}

#[test]
fn ac_n1_1_imported_credentials_join_the_set_marked_audited_and_shown() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let ops = importer(&mut fx);
    let ids = import(&mut fx, ops, alice.principal, vec![passkey(1), passkey(2)]).unwrap();
    assert_eq!(ids.len(), 2);
    for id in &ids {
        let c = fx.ledger.state().credential(id).unwrap();
        assert_eq!(c.identity(), alice.principal);
        assert_eq!(c.marker(), CredentialMarker::Imported);
        assert_eq!(c.status(), CredentialStatus::Active);
    }
    // Attributed to the operator's grant, on the account's action log, audited.
    let audit = super::operator::last_audit(&fx);
    assert_eq!(audit.class, crate::audit::AuditClass::OperatorIntervention);
    // An imported passkey acts as owner authority once it is used, like any member.
    let as_passkey = Caller::new(alice.principal, ids[0]);
    assert!(
        fx.ledger
            .recovery_mechanisms(&as_passkey, &acct, 10)
            .is_ok()
    );
}

#[test]
fn ac_n1_2_an_unaccepted_or_held_credential_is_refused_and_nothing_changes() {
    let mut fx = Fx::new();
    let alice = fx.person();
    fx.account(&alice, Custody::Delegated);
    let ops = importer(&mut fx);
    import(&mut fx, ops, alice.principal, vec![passkey(1)]).unwrap();
    let before = fx.ledger.state().credentials_of(&alice.principal).count();
    let mut malformed = passkey(3).as_bytes().to_vec();
    malformed[34] = 0x02;
    for set in [
        Vec::new(),
        vec![passkey(1)],
        vec![passkey(4), passkey(4)],
        vec![Verifier::of(Scheme::WebAuthnEs256, malformed)],
        vec![Verifier::of(Scheme::ApiKey, vec![1; 31])],
        (0..=MAX_IMPORTED_CREDENTIALS)
            .map(|n| Verifier::of(Scheme::ApiKey, vec![n as u8; 32]))
            .collect(),
    ] {
        assert_eq!(
            import(&mut fx, ops, alice.principal, set),
            Err(Refusal::CredentialInvalid)
        );
    }
    assert_eq!(
        fx.ledger.state().credentials_of(&alice.principal).count(),
        before
    );
}

#[test]
fn ac_n1_3_no_import_while_an_account_is_owned_and_none_without_the_grant() {
    let mut fx = Fx::new();
    let alice = fx.person();
    fx.account(&alice, Custody::Delegated);
    fx.account(&alice, Custody::Owned);
    let ops = importer(&mut fx);
    assert_eq!(
        import(&mut fx, ops, alice.principal, vec![passkey(5)]),
        Err(Refusal::NotAuthorised)
    );
    // Without the import grant, the operator's other powers do not import.
    let bob = fx.person();
    let not_ops =
        super::operator::staff(&mut fx, &[crate::operator::OperatorOpKind::FreezeAccount]);
    assert_eq!(
        import(&mut fx, not_ops, bob.principal, vec![passkey(6)]),
        Err(Refusal::NotAuthorised)
    );
    // A holder cannot import into anyone's set, its own included.
    assert_eq!(
        import(&mut fx, bob, bob.principal, vec![passkey(7)]),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn req_mg_7_a_repeat_returns_the_same_credentials() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let ops = importer(&mut fx);
    let ctx = fx.ctx(ops);
    let req = ImportCredentials {
        identity: alice.principal,
        credentials: vec![ImportedCredential {
            verifier: passkey(8),
            scope: None,
        }],
    };
    let first = fx
        .ledger
        .import_credentials(&ctx, req.clone(), b"batch 2")
        .unwrap()
        .value;
    let again = fx
        .ledger
        .import_credentials(&ctx, req, b"batch 2")
        .unwrap()
        .value;
    assert_eq!(first, again);
}

#[test]
fn dec_19_a_malformed_verifier_does_not_join_a_set() {
    let mut fx = Fx::new();
    let alice = fx.person();
    assert_eq!(
        register(
            &mut fx,
            alice,
            Verifier::of(Scheme::WebAuthnEs256, vec![1; 40])
        ),
        Err(Refusal::CredentialInvalid)
    );
    assert_eq!(
        register(&mut fx, alice, Verifier::of(Scheme::ApiKey, vec![1; 33])),
        Err(Refusal::CredentialInvalid)
    );
    assert!(register(&mut fx, alice, passkey(4)).is_ok());
    assert!(register(&mut fx, alice, api_key(5)).is_ok());
}

#[test]
fn dec_19_a_request_binds_its_verifiers_scheme() {
    // The same bytes under two schemes are two requests: under one client key, the second is not
    // a repeat of the first.
    let mut fx = Fx::new();
    let alice = fx.person();
    let ctx = fx.ctx(alice);
    let bytes = vec![9u8; 32];
    fx.ledger
        .register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::Ordinary,
                scope: None,
                verifier: Verifier::new(bytes.clone()),
            },
        )
        .unwrap();
    let again = fx.ledger.register_credential(
        &ctx,
        RegisterCredential {
            marker: CredentialMarker::Ordinary,
            scope: None,
            verifier: Verifier::of(Scheme::ApiKey, bytes),
        },
    );
    assert_eq!(again.map(|a| a.value), Err(Refusal::ClientKeyReused));
}

fn secret(n: u8) -> SecretKey {
    let mut b = [n; 32];
    b[31] = 0;
    SecretKey::from_bytes(b).expect("a canonical scalar")
}

fn reclaim(
    fx: &mut Fx,
    alice: Caller,
    account: AccountId,
    proof: CredentialProof,
) -> Result<(), Refusal> {
    let ctx = fx.ctx(alice);
    let acknowledged = Some(
        fx.ledger
            .recovery_mechanisms(&alice, &account, 10)
            .unwrap()
            .digest(),
    );
    fx.ledger
        .reclaim_custody(
            &ctx,
            ReclaimCustody {
                account,
                proof,
                acknowledged,
            },
        )
        .map(|_| ())
}

#[test]
fn ac_a7_3_a_passkey_proves_itself_by_the_boundary_mark_and_ac_a7_4_nothing_else_does() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let pk = register(&mut fx, alice, passkey(6)).unwrap();
    let ak = register(&mut fx, alice, api_key(7)).unwrap();
    let sk = secret(8);
    let key = register(&mut fx, alice, Verifier::new(sk.public_key().0.to_vec())).unwrap();
    let acct = fx.account(&alice, Custody::Delegated);
    let epoch = fx.ledger.state().custody().epoch(&acct);
    let sig =
        ledger_kernel::keys::RootKeys::new([0x51; 32]).sign(&sk, &reclaim_challenge(&acct, epoch));
    // The mark on a key or an API key, and a signature on a passkey or an API key: refused.
    for proof in [
        CredentialProof {
            credential: key,
            proof: Proof::PasskeyChecked,
        },
        CredentialProof {
            credential: ak,
            proof: Proof::PasskeyChecked,
        },
        CredentialProof {
            credential: pk,
            proof: Proof::Signature(sig),
        },
        CredentialProof {
            credential: ak,
            proof: Proof::Signature(sig),
        },
    ] {
        assert_eq!(
            reclaim(&mut fx, alice, acct, proof),
            Err(Refusal::CredentialInvalid)
        );
    }
    reclaim(
        &mut fx,
        alice,
        acct,
        CredentialProof {
            credential: pk,
            proof: Proof::PasskeyChecked,
        },
    )
    .unwrap();
    assert_eq!(
        fx.ledger.state().account(&acct).unwrap().custody(),
        Custody::Owned
    );
}

#[test]
fn ac_n1_4_an_imported_credential_proves_itself_before_owned_custody() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let ops = importer(&mut fx);
    let sk = secret(9);
    let ids = import(
        &mut fx,
        ops,
        alice.principal,
        vec![Verifier::new(sk.public_key().0.to_vec())],
    )
    .unwrap();
    let epoch = fx.ledger.state().custody().epoch(&acct);
    let challenge = reclaim_challenge(&acct, epoch);
    // A signature by another key does not prove it.
    let forged = ledger_kernel::keys::RootKeys::new([0x51; 32]).sign(&secret(10), &challenge);
    assert_eq!(
        reclaim(
            &mut fx,
            alice,
            acct,
            CredentialProof {
                credential: ids[0],
                proof: Proof::Signature(forged),
            },
        ),
        Err(Refusal::CredentialInvalid)
    );
    let sig = ledger_kernel::keys::RootKeys::new([0x51; 32]).sign(&sk, &challenge);
    reclaim(
        &mut fx,
        alice,
        acct,
        CredentialProof {
            credential: ids[0],
            proof: Proof::Signature(sig),
        },
    )
    .unwrap();
    // Owned now: no further import into this identity (REQ-MG-8).
    assert_eq!(
        import(&mut fx, ops, alice.principal, vec![passkey(11)]),
        Err(Refusal::NotAuthorised)
    );
}
