// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of `F-012`: the credential set, custody switching, recovery and succession. The
//! acceptance and error cases run in the conformance suite (`crates/conformance/src/cases/f012.rs`)
//! on both stores; these cover the rules one by one, the `INV-22` property test, the store's
//! rules under the commands, and what the suite does not see.

use super::*;
use crate::audit::{AuditAction, AuditClass, AuditOutcome};
use crate::custody::{
    ChangeKind, ChangeState, ChangeTarget, Closed, CustodyRefusal, CustodyWrite, PendingChange,
    PendingId, RecoveryConfig, RecoveryKey, StartupError, approval_message, reclaim_challenge,
    recovery_rules, testing,
};
use crate::ledger::custody::{
    Claimed, CredentialProof, InitiateRecovery, Proof, ReclaimCustody, RegisterCredential,
    scope_within,
};
use crate::principal::CredentialStatus;
use crate::state::StoreRefusal;
use cipher_clerk::crypto::SecretKey;

const JUSTIFY: &[u8] = b"holder verified out of band, case 42";

fn secret(n: u8) -> SecretKey {
    let mut b = [n; 32];
    b[31] = 0;
    SecretKey::from_bytes(b).expect("a canonical scalar")
}

fn verifier_of(sk: &SecretKey) -> Verifier {
    Verifier::new(sk.public_key().0.to_vec())
}

fn signer() -> ledger_kernel::keys::RootKeys {
    ledger_kernel::keys::RootKeys::new([0x51; 32])
}

fn at(caller: Caller, op: OperationId, now: Time) -> Ctx {
    Ctx { caller, op, now }
}

/// Register a credential into `owner`'s set with `sk`'s public key.
fn register(
    fx: &mut Fx,
    owner: Caller,
    sk: &SecretKey,
    marker: CredentialMarker,
    scope: Option<NonEmptySet<AccountId>>,
) -> Result<CredentialId, Refusal> {
    let ctx = fx.ctx(owner);
    fx.ledger
        .register_credential(
            &ctx,
            RegisterCredential {
                marker,
                scope,
                verifier: verifier_of(sk),
            },
        )
        .map(|a| a.value)
}

/// A proof that `credential` (with secret `sk`) works, for a reclaim of `account` in custody
/// epoch `epoch`.
fn proof(
    sk: &SecretKey,
    credential: CredentialId,
    account: AccountId,
    epoch: u64,
) -> CredentialProof {
    CredentialProof {
        credential,
        proof: Proof::Signature(signer().sign(sk, &reclaim_challenge(&account, epoch))),
    }
}

fn epoch(fx: &Fx, account: AccountId) -> u64 {
    fx.ledger.state().custody().epoch(&account)
}

fn reclaim(
    fx: &mut Fx,
    owner: Caller,
    account: AccountId,
    sk: &SecretKey,
    credential: CredentialId,
) -> Result<Accepted<()>, Refusal> {
    let ctx = fx.ctx(owner);
    let acknowledged = Some(
        fx.ledger
            .recovery_mechanisms(&owner, &account, 10)
            .unwrap()
            .digest(),
    );
    fx.ledger.reclaim_custody(
        &ctx,
        ReclaimCustody {
            account,
            proof: proof(sk, credential, account, epoch(fx, account)),
            acknowledged,
        },
    )
}

// ---------------------------------------------------------------- start-up (T-012-08)

#[test]
fn req_au_15_genesis_fails_when_the_recovery_key_is_the_operators() {
    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([7; 32]),
    )
    .unwrap();
    let key = testing::config().recovery_key.unwrap();
    let genesis = Genesis {
        assets: Vec::new(),
        operator_verifier: Verifier::new(key.0.to_vec()),
        lifecycle: crate::lifecycle::LifecycleConfig::new([7; 16], 60, 3_600)
            .with_recovery(testing::config()),
        chainspec: None,
        spec: None,
    };
    let r = Ledger::genesis(
        book,
        genesis,
        OperationId::from_ref(OperationRef([0; 16])),
        1,
    );
    assert_eq!(
        r.err(),
        Some(GenesisError::Recovery(
            StartupError::RecoveryKeyIsOperatorKey
        ))
    );
}

#[test]
fn req_au_9_genesis_fails_when_owned_custody_has_no_recovery_authority() {
    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([7; 32]),
    )
    .unwrap();
    let mut config = testing::config();
    config.recovery_key = None;
    let genesis = Genesis {
        assets: Vec::new(),
        operator_verifier: Verifier::new(vec![0]),
        lifecycle: crate::lifecycle::LifecycleConfig::new([7; 16], 60, 3_600).with_recovery(config),
        chainspec: None,
        spec: None,
    };
    let r = Ledger::genesis(
        book,
        genesis,
        OperationId::from_ref(OperationRef([0; 16])),
        1,
    );
    assert_eq!(
        r.err(),
        Some(GenesisError::Recovery(
            StartupError::OwnedCustodyWithoutRecoveryKey
        ))
    );
}

#[test]
fn a_deployment_without_recovery_offers_no_owned_custody() {
    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([7; 32]),
    )
    .unwrap();
    let genesis = Genesis {
        assets: Vec::new(),
        operator_verifier: Verifier::new(vec![0]),
        lifecycle: crate::lifecycle::LifecycleConfig::new([7; 16], 60, 3_600),
        chainspec: None,
        spec: None,
    };
    let (mut ledger, founded) = Ledger::genesis(
        book,
        genesis,
        OperationId::from_ref(OperationRef([0; 16])),
        1,
    )
    .unwrap();
    let op = Caller::new(founded.operator, founded.operator_credential);
    assert_eq!(
        ledger.recovery_terms(&op, &founded.operator),
        Err(Refusal::NotAuthorised)
    );
    let ctx = Ctx {
        caller: op,
        op: OperationId::from_ref(OperationRef([1; 16])),
        now: 2,
    };
    assert_eq!(
        ledger
            .open_account(
                &ctx,
                OpenAccount {
                    owner: founded.operator,
                    kind: AccountKind::Customer,
                    custody: Custody::Owned,
                    treasury_limits: BTreeMap::new(),
                    acknowledged: None,
                },
            )
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

// ---------------------------------------------------------------- the set (T-012-01, T-012-02)

#[test]
fn the_scope_subset_rule() {
    let fx = &mut Fx::new();
    let alice = fx.person();
    let a = fx.account(&alice, Custody::Owned);
    let b = fx.account(&alice, Custody::Owned);
    let only_a = register(
        fx,
        alice,
        &secret(1),
        CredentialMarker::Ordinary,
        Some(NonEmptySet::one(a)),
    )
    .unwrap();
    let scoped = Caller::new(alice.principal, only_a);
    // A scoped credential registers within its scope, and nothing wider.
    assert!(
        register(
            fx,
            scoped,
            &secret(2),
            CredentialMarker::Ordinary,
            Some(NonEmptySet::one(a))
        )
        .is_ok()
    );
    assert_eq!(
        register(fx, scoped, &secret(3), CredentialMarker::Ordinary, None),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        register(
            fx,
            scoped,
            &secret(3),
            CredentialMarker::Ordinary,
            Some(NonEmptySet::one(b))
        ),
        Err(Refusal::NotAuthorised)
    );
    // Nor can it remove a wider member.
    let ctx = fx.ctx(scoped);
    assert_eq!(
        fx.ledger
            .remove_credential(&ctx, alice.credential)
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // A scope naming another identity's account is refused.
    let bob = fx.person();
    let theirs = fx.account(&bob, Custody::Owned);
    assert_eq!(
        register(
            fx,
            alice,
            &secret(4),
            CredentialMarker::Ordinary,
            Some(NonEmptySet::one(theirs))
        ),
        Err(Refusal::NotAuthorised)
    );
    assert!(scope_within(None, None));
    assert!(!scope_within(None, Some(&NonEmptySet::one(a))));
}

#[test]
fn req_au_3_a_removed_credential_is_refused_on_the_next_command() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 50);
    let second = register(&mut fx, alice, &secret(1), CredentialMarker::Ordinary, None).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.remove_credential(&ctx, second).unwrap();
    let with_removed = Caller::new(alice.principal, second);
    let ctx = fx.ctx(with_removed);
    assert_eq!(
        fx.ledger
            .reserve(&ctx, acct, usd(), Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::CredentialInvalid)
    );
    assert_eq!(
        fx.ledger.state().credential(&second).unwrap().status(),
        CredentialStatus::Removed
    );
    // The set changes are recorded and audited as the holder's authority changes.
    let r = fx.ledger.state().audit().outbox().last().unwrap().clone();
    assert_eq!(r.action, AuditAction::Command(Command::RemoveCredential));
    assert_eq!(r.class, AuditClass::AuthorityChange);
    assert_eq!(r.authorities[0].kind, AuthorityKind::Owner);
}

#[test]
fn req_au_20_the_last_credential_cannot_be_removed() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .remove_credential(&ctx, alice.credential)
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    assert!(!recovery_rules::may_remove(0));
    // With a second one, the first can go — even by itself.
    register(&mut fx, alice, &secret(1), CredentialMarker::Ordinary, None).unwrap();
    let ctx = fx.ctx(alice);
    assert!(fx.ledger.remove_credential(&ctx, alice.credential).is_ok());
}

#[test]
fn a_holder_held_credential_needs_the_deployment_to_offer_it() {
    // The unit fixture's deployment does not offer it.
    let mut fx = Fx::new();
    let alice = fx.person();
    assert_eq!(
        register(
            &mut fx,
            alice,
            &secret(1),
            CredentialMarker::RecoveryHeld,
            None
        ),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn the_recovery_key_is_never_a_credential() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let ctx = fx.ctx(alice);
    let key = testing::config().recovery_key.unwrap();
    assert_eq!(
        fx.ledger
            .register_credential(
                &ctx,
                RegisterCredential {
                    marker: CredentialMarker::Ordinary,
                    scope: None,
                    verifier: Verifier::new(key.0.to_vec()),
                },
            )
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn req_au_15_the_recovery_authority_cannot_act_as_a_principal() {
    let mut fx = Fx::new();
    let (_, acct) = funded(&mut fx, 50);
    let ra = testing::config().authority().unwrap();
    let ctx = fx.ctx(Caller::new(ra.principal, ra.credential));
    assert_eq!(
        fx.ledger
            .reserve(&ctx, acct, usd(), Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::CredentialInvalid)
    );
    assert_eq!(
        fx.ledger.delegate_custody(&ctx, acct).map(|a| a.value),
        Err(Refusal::CredentialInvalid)
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// `INV-22`: no sequence of set commands from a scoped credential produces a member wider than
    /// that credential. Each step registers or removes, from one of the members the scoped
    /// credential produced (all of them within its scope), with a random scope over the holder's
    /// three accounts.
    #[test]
    fn inv_22_no_set_command_from_a_scoped_credential_widens(
        steps in proptest::collection::vec((0u8..2, 0usize..8, 0u8..8), 1..16)
    ) {
        let mut fx = Fx::new();
        let alice = fx.person();
        let accounts: Vec<AccountId> = (0..3).map(|_| fx.account(&alice, Custody::Owned)).collect();
        let root_scope = NonEmptySet::new([accounts[0], accounts[1]].into()).unwrap();
        let root = register(&mut fx, alice, &secret(1), CredentialMarker::Ordinary, Some(root_scope.clone())).unwrap();
        let mut members = vec![root];
        for (n, (kind, who, mask)) in steps.into_iter().enumerate() {
            let by = members[who % members.len()];
            let caller = Caller::new(alice.principal, by);
            let scope: BTreeSet<AccountId> = (0..3).filter(|i| mask & (1 << i) != 0).map(|i| accounts[i]).collect();
            let scope = NonEmptySet::new(scope);
            if kind == 0 {
                if let Ok(id) = register(&mut fx, caller, &secret(10 + n as u8), CredentialMarker::Ordinary, scope) {
                    members.push(id);
                }
            } else {
                let target = members[(who + 1) % members.len()];
                let ctx = fx.ctx(caller);
                let _ = fx.ledger.remove_credential(&ctx, target);
            }
            // Every member the scoped credential's line produced is within the root's scope.
            for id in &members {
                let c = fx.ledger.state().credential(id).unwrap();
                prop_assert!(scope_within(c.scope(), Some(&root_scope)), "{id:?} widened");
            }
        }
    }
}

// ---------------------------------------------------------------- custody (T-012-03, T-012-04)

#[test]
fn ac_a7_3_a_reservation_the_operator_made_is_captured_after_the_reclaim() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let sk = secret(1);
    let cred = register(&mut fx, alice, &sk, CredentialMarker::Ordinary, None).unwrap();
    let acct = fx.account(&alice, Custody::Delegated);
    let issuer = fx.issuer();
    let ctx = fx.ctx(issuer);
    fx.ledger.issue(&ctx, usd(), acct, Minor::new(100)).unwrap();
    let (_, dest) = funded(&mut fx, 1);
    // An operator grant to capture from delegated accounts.
    let ops = fx.person();
    let ctx = fx.ctx(fx.operator());
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Operator,
                principal: ops.principal,
                subject: Subject::DelegatedAccounts,
                ops: NonEmptySet::new([OperationKind::Reserve, OperationKind::Capture].into())
                    .unwrap(),
                per_operation: Some(nz(1_000)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    let reserved = fx.ctx(ops);
    fx.ledger
        .reserve(&reserved, acct, usd(), Minor::new(40))
        .unwrap();
    let before = fx.ctx(ops);
    fx.ledger
        .reserve(&before, acct, usd(), Minor::new(10))
        .unwrap();
    reclaim(&mut fx, alice, acct, &sk, cred).unwrap();
    // No operator authority remains for new operations...
    let new = fx.ctx(ops);
    assert_eq!(
        fx.ledger
            .reserve(&new, acct, usd(), Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::UnknownAccount)
    );
    // ...but the reservation it made before finishes under the authority that started it.
    let capture = Ctx {
        now: 11,
        ..reserved
    };
    fx.ledger.capture(&capture, dest, Minor::new(40)).unwrap();
    assert_eq!(fx.ledger.available(&alice, &acct, &usd(), 12), Ok(50));
    // The holder, of course, can end the other one too.
    let release = Ctx {
        caller: alice,
        now: 12,
        ..before
    };
    fx.ledger.release(&release).unwrap();
}

#[test]
fn a_reclaim_proof_binds_its_account_and_its_custody_epoch() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let sk = secret(1);
    let cred = register(&mut fx, alice, &sk, CredentialMarker::Ordinary, None).unwrap();
    let acct = fx.account(&alice, Custody::Delegated);
    let other = fx.account(&alice, Custody::Delegated);
    let ack = |fx: &Fx, a| {
        Some(
            fx.ledger
                .recovery_mechanisms(&alice, &a, 10)
                .unwrap()
                .digest(),
        )
    };
    // A proof made for another custody epoch, or another account, proves nothing.
    let ctx = fx.ctx(alice);
    let stale = proof(&sk, cred, acct, epoch(&fx, acct) + 1);
    let r = ReclaimCustody {
        account: acct,
        proof: stale,
        acknowledged: ack(&fx, acct),
    };
    assert_eq!(
        fx.ledger.reclaim_custody(&ctx, r).map(|a| a.value),
        Err(Refusal::CredentialInvalid)
    );
    let ctx = fx.ctx(alice);
    let elsewhere = proof(&sk, cred, other, epoch(&fx, other));
    let r = ReclaimCustody {
        account: acct,
        proof: elsewhere,
        acknowledged: ack(&fx, acct),
    };
    assert_eq!(
        fx.ledger.reclaim_custody(&ctx, r).map(|a| a.value),
        Err(Refusal::CredentialInvalid)
    );
    // A credential whose verifier is not a key cannot prove anything: the fixture's first one.
    let ctx = fx.ctx(alice);
    let r = ReclaimCustody {
        account: acct,
        proof: proof(&sk, alice.credential, acct, epoch(&fx, acct)),
        acknowledged: ack(&fx, acct),
    };
    assert_eq!(
        fx.ledger.reclaim_custody(&ctx, r).map(|a| a.value),
        Err(Refusal::CredentialInvalid)
    );
    // A good proof with no acknowledgement is refused; with it, the reclaim succeeds, and the
    // record names the proving credential beside the presenting one.
    let ctx = fx.ctx(alice);
    let r = ReclaimCustody {
        account: acct,
        proof: proof(&sk, cred, acct, epoch(&fx, acct)),
        acknowledged: None,
    };
    assert_eq!(
        fx.ledger.reclaim_custody(&ctx, r).map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let done = reclaim(&mut fx, alice, acct, &sk, cred).unwrap();
    assert_eq!(done.record.authorities.len(), 2);
    assert_eq!(done.record.authorities[1].credential, cred);
    assert_eq!(
        fx.ledger.state().account(&acct).unwrap().custody(),
        Custody::Owned
    );
}

#[test]
fn an_acknowledgement_goes_stale_when_the_mechanisms_change() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Owned);
    let stale = fx
        .ledger
        .recovery_mechanisms(&alice, &acct, 10)
        .unwrap()
        .digest();
    let bob = fx.person();
    let ctx = fx.ctx(alice);
    fx.ledger
        .name_successor(&ctx, acct, Some(bob.principal))
        .unwrap();
    let fresh = fx
        .ledger
        .recovery_mechanisms(&alice, &acct, 10)
        .unwrap()
        .digest();
    assert_ne!(stale, fresh);
    let ctx = fx.ctx(alice);
    fx.ledger.delegate_custody(&ctx, acct).unwrap();
    let sk = secret(1);
    let cred = register(&mut fx, alice, &sk, CredentialMarker::Ordinary, None).unwrap();
    let ctx = fx.ctx(alice);
    let r = ReclaimCustody {
        account: acct,
        proof: proof(&sk, cred, acct, epoch(&fx, acct)),
        acknowledged: Some(stale),
    };
    assert_eq!(
        fx.ledger.reclaim_custody(&ctx, r).map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn custody_changes_notify_every_authority_holder_and_touch_no_grant() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Owned);
    let agent = fx.agent(alice.principal);
    let grant = fx.delegate(
        &alice,
        agent.principal,
        acct,
        &[OperationKind::Transfer],
        10,
    );
    let ctx = fx.ctx(alice);
    fx.ledger.delegate_custody(&ctx, acct).unwrap();
    assert_eq!(fx.ledger.custody_notices(&agent.principal).len(), 1);
    assert_eq!(fx.ledger.custody_notices(&alice.principal).len(), 1);
    assert!(fx.ledger.state().grant(&grant).unwrap().revoked().is_none());
    // Delegating a delegated account again is refused, and changes nothing.
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger.delegate_custody(&ctx, acct).map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

// ---------------------------------------------------------------- recovery (T-012-06)

struct Recovery {
    fx: Fx,
    alice: Caller,
    acct: AccountId,
    new_key: SecretKey,
}

impl Recovery {
    fn new() -> Recovery {
        let mut fx = Fx::new();
        let (alice, acct) = funded(&mut fx, 100);
        Recovery {
            fx,
            alice,
            acct,
            new_key: secret(9),
        }
    }

    fn initiate(&mut self, now: Time) -> Result<PendingId, Refusal> {
        let op = self.fx.op();
        let ctx = at(self.fx.operator(), op, now);
        self.fx
            .ledger
            .initiate_recovery(
                &ctx,
                InitiateRecovery {
                    account: self.acct,
                    verifier: verifier_of(&self.new_key),
                    scope: None,
                },
                JUSTIFY,
            )
            .map(|a| a.value)
    }

    fn approval(&self, pending: PendingId) -> crate::custody::Signature {
        let msg = approval_message(&pending, &self.acct, &verifier_of(&self.new_key));
        signer().sign(&testing::secret(), &msg)
    }

    fn complete(
        &mut self,
        pending: PendingId,
        approval: crate::custody::Signature,
        now: Time,
    ) -> Result<Accepted<CredentialId>, Refusal> {
        let op = self.fx.op();
        let ctx = at(self.fx.operator(), op, now);
        self.fx
            .ledger
            .complete_recovery(&ctx, pending, approval, JUSTIFY)
    }
}

#[test]
fn a_recovery_completes_only_with_the_approval_and_after_the_delay() {
    let mut r = Recovery::new();
    let delay = testing::config().recovery_delay;
    let agent = r.fx.agent(r.alice.principal);
    let grant = r.fx.delegate(
        &r.alice,
        agent.principal,
        r.acct,
        &[OperationKind::Transfer],
        10,
    );
    let pending = r.initiate(100).unwrap();
    // Visible to the holder and every authority holder of the account.
    let view = r.fx.ledger.custody_view(&r.alice, &r.acct, 101).unwrap();
    assert_eq!(view.changes.len(), 1);
    assert_eq!(view.changes[0].effective_at, 100 + delay);
    assert_eq!(view.changes[0].state, ChangeState::Pending);
    assert!(
        r.fx.ledger
            .custody_notices(&agent.principal)
            .iter()
            .any(|n| n.op == pending.0)
    );
    // A second one while it is pending is refused.
    assert_eq!(r.initiate(101), Err(Refusal::NotAuthorised));
    // The operator's own signature is no approval.
    let forged = signer().sign(
        &secret(3),
        &approval_message(&pending, &r.acct, &verifier_of(&r.new_key)),
    );
    assert_eq!(
        r.complete(pending, forged, 100 + delay).map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // The approval, early.
    let approval = r.approval(pending);
    assert_eq!(
        r.complete(pending, approval, 100 + delay - 1)
            .map(|a| a.value),
        Err(Refusal::RecoveryNotElapsed)
    );
    // Both refusals are on the audit trail.
    let refused = r.fx.ledger.state().audit().outbox().last().unwrap().clone();
    assert_eq!(
        refused.outcome,
        AuditOutcome::Refused(Refusal::RecoveryNotElapsed)
    );
    // Then it completes.
    let done = r.complete(pending, approval, 100 + delay).unwrap();
    let new = done.value;
    let kinds: Vec<AuthorityKind> = done.record.authorities.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        [
            AuthorityKind::Operator,
            AuthorityKind::Recovery,
            AuthorityKind::Operator
        ]
    );
    assert_eq!(
        fx_cred_status(&r.fx, r.alice.credential),
        CredentialStatus::Removed
    );
    assert_eq!(fx_cred_status(&r.fx, new), CredentialStatus::Active);
    // Recovery keeps the grants the holder made (`REQ-AU-17`, amendment 0006 item 10).
    assert!(!recovery_rules::grants_revoked_on(ChangeKind::Recovery));
    assert!(
        r.fx.ledger
            .state()
            .grant(&grant)
            .unwrap()
            .revoked()
            .is_none()
    );
    // The old credential is refused; the new one acts as the owner.
    let old = r.fx.ctx(r.alice);
    assert_eq!(
        r.fx.ledger
            .reserve(&old, r.acct, usd(), Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::CredentialInvalid)
    );
    let holder = Caller::new(r.alice.principal, new);
    let ctx = r.fx.ctx(holder);
    assert_eq!(
        r.fx.ledger
            .reserve(&ctx, r.acct, usd(), Minor::new(1))
            .unwrap()
            .record
            .authorities[0]
            .kind,
        AuthorityKind::Owner
    );
    // A completed recovery absorbs.
    assert_eq!(
        r.complete(pending, approval, 100 + delay + 1)
            .map(|a| a.value),
        Err(Refusal::OperationTerminal)
    );
}

fn fx_cred_status(fx: &Fx, c: CredentialId) -> CredentialStatus {
    fx.ledger.state().credential(&c).unwrap().status()
}

#[test]
fn only_the_operator_initiates_and_a_refusal_is_audited() {
    let mut r = Recovery::new();
    let ctx = r.fx.ctx(r.alice);
    let req = InitiateRecovery {
        account: r.acct,
        verifier: verifier_of(&r.new_key),
        scope: None,
    };
    assert_eq!(
        r.fx.ledger
            .initiate_recovery(&ctx, req.clone(), JUSTIFY)
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let a = r.fx.ledger.state().audit().outbox().last().unwrap().clone();
    assert_eq!(a.action, AuditAction::Command(Command::InitiateRecovery));
    assert_eq!(a.outcome, AuditOutcome::Refused(Refusal::NotAuthorised));
    let ctx = r.fx.ctx(r.fx.operator());
    assert_eq!(
        r.fx.ledger
            .initiate_recovery(&ctx, req, b"  ")
            .map(|a| a.value),
        Err(Refusal::JustificationRequired)
    );
}

#[test]
fn ac_a8_2_ordinary_use_does_not_cancel_a_recovery() {
    let mut r = Recovery::new();
    let pending = r.initiate(100).unwrap();
    let ctx = r.fx.ctx(r.alice);
    r.fx.ledger
        .reserve(&ctx, r.acct, usd(), Minor::new(5))
        .unwrap();
    assert!(!recovery_rules::use_cancels_pending());
    assert_eq!(
        r.fx.ledger
            .state()
            .custody()
            .pending(&pending)
            .unwrap()
            .state,
        ChangeState::Pending
    );
    // An explicit cancel does.
    let ctx = r.fx.ctx(r.alice);
    r.fx.ledger.cancel_recovery(&ctx, pending).unwrap();
    assert_eq!(
        r.fx.ledger
            .state()
            .custody()
            .pending(&pending)
            .unwrap()
            .state,
        ChangeState::Cancelled
    );
    let approval = r.approval(pending);
    let delay = testing::config().recovery_delay;
    assert_eq!(
        r.complete(pending, approval, 100 + delay).map(|a| a.value),
        Err(Refusal::OperationTerminal)
    );
}

#[test]
fn a_recovery_does_not_complete_once_the_account_left_owned_custody() {
    let mut r = Recovery::new();
    let pending = r.initiate(100).unwrap();
    let ctx = r.fx.ctx(r.alice);
    r.fx.ledger.delegate_custody(&ctx, r.acct).unwrap();
    let approval = r.approval(pending);
    let delay = testing::config().recovery_delay;
    assert_eq!(
        r.complete(pending, approval, 100 + delay).map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

// ---------------------------------------------------------------- succession (T-012-07)

#[test]
fn a_succession_passes_the_account_and_revokes_the_previous_holders_grants() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let bob = fx.person();
    let agent = fx.agent(alice.principal);
    let grant = fx.delegate(
        &alice,
        agent.principal,
        acct,
        &[OperationKind::Transfer],
        10,
    );
    let ctx = fx.ctx(alice);
    fx.ledger
        .name_successor(&ctx, acct, Some(bob.principal))
        .unwrap();
    let delay = testing::config().succession_delay.unwrap();
    let claim = |fx: &mut Fx, now| {
        let op = fx.op();
        fx.ledger
            .claim_succession(&at(bob, op, now), acct)
            .map(|a| a.value)
    };
    let Ok(Claimed::Pending {
        pending,
        effective_at,
    }) = claim(&mut fx, 100)
    else {
        panic!("the first claim opens a pending succession");
    };
    assert_eq!(effective_at, 100 + delay);
    assert_eq!(
        claim(&mut fx, 100 + delay - 1),
        Err(Refusal::RecoveryNotElapsed)
    );
    assert_eq!(
        claim(&mut fx, 100 + delay),
        Ok(Claimed::Completed { pending })
    );
    let a = fx.ledger.state().account(&acct).unwrap();
    assert_eq!(a.owner(), bob.principal);
    assert!(recovery_rules::grants_revoked_on(ChangeKind::Succession));
    assert!(fx.ledger.state().grant(&grant).unwrap().revoked().is_some());
    // Alice's credentials no longer reach it; Bob's do.
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .reserve(&ctx, acct, usd(), Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::UnknownAccount)
    );
    let ctx = fx.ctx(bob);
    assert!(fx.ledger.reserve(&ctx, acct, usd(), Minor::new(1)).is_ok());
    assert_eq!(fx.ledger.state().custody().successor(&acct), None);
}

/// Finding F18's class, `REQ-RT-1`: the replay of a claim that completed a succession answers
/// the change it completed, read by key (the completion's notice to the claimant, then the
/// change), never by reading every change the book recorded.
#[test]
fn req_rt_1_a_completed_claims_replay_reads_the_change_by_key() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let bob = fx.person();
    let ctx = fx.ctx(alice);
    fx.ledger
        .name_successor(&ctx, acct, Some(bob.principal))
        .unwrap();
    let delay = testing::config().succession_delay.unwrap();
    let op = fx.op();
    let Ok(Claimed::Pending { pending, .. }) = fx
        .ledger
        .claim_succession(&at(bob, op, 100), acct)
        .map(|a| a.value)
    else {
        panic!("the first claim opens a pending succession");
    };
    let completing = at(bob, fx.op(), 100 + delay);
    assert_eq!(
        fx.ledger
            .claim_succession(&completing, acct)
            .map(|a| a.value),
        Ok(Claimed::Completed { pending })
    );
    fx.ledger.record_calls(true);
    let _ = fx.ledger.state().take_touched();
    assert_eq!(
        fx.ledger
            .claim_succession(&completing, acct)
            .map(|a| a.value),
        Ok(Claimed::Completed { pending })
    );
    let whole: Vec<u8> = fx
        .ledger
        .state()
        .take_touched()
        .into_iter()
        .filter(|t| t.whole)
        .map(|t| t.kind)
        .collect();
    assert_eq!(whole, Vec::<u8>::new());
}

#[test]
fn only_the_named_successor_claims() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 1);
    let bob = fx.person();
    let carol = fx.person();
    let ctx = fx.ctx(alice);
    fx.ledger
        .name_successor(&ctx, acct, Some(bob.principal))
        .unwrap();
    let ctx = fx.ctx(carol);
    assert_eq!(
        fx.ledger.claim_succession(&ctx, acct).map(|a| a.value),
        Err(Refusal::UnknownAccount)
    );
    // An agent cannot be named; nor can the holder itself.
    let agent = fx.agent(alice.principal);
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .name_successor(&ctx, acct, Some(agent.principal))
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .name_successor(&ctx, acct, Some(alice.principal))
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

// ---------------------------------------------------------------- the store's rules

fn store_with_change(now: Time, effective_at: Time) -> (State, PendingChange) {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 1);
    let owner = alice.principal;
    // A copy of the ledger's store rules is exercised on a fresh store, seeded by hand.
    let mut s = State::new();
    let person = *fx.ledger.state().identity(&owner).unwrap();
    let account = fx.ledger.state().account(&acct).unwrap().clone();
    s.commit(&[Write::PutIdentity(person), Write::PutAccount(account)])
        .unwrap();
    let change = PendingChange {
        id: PendingId(OperationId::from_ref(OperationRef([0xC1; 16]))),
        kind: ChangeKind::Recovery,
        account: acct,
        owner,
        initiated_by: crate::authority::AuthorityUsed {
            kind: AuthorityKind::Operator,
            grant: None,
            principal: owner,
            credential: alice.credential,
            on_behalf_of: None,
        },
        initiated_at: now,
        effective_at,
        target: ChangeTarget::Credential {
            verifier: Verifier::new(vec![1]),
            scope: None,
        },
        state: ChangeState::Pending,
        closed: None,
    };
    (s, change)
}

#[test]
fn req_au_16_the_store_refuses_a_change_without_a_delay_and_an_early_completion() {
    let (mut s, change) = store_with_change(10, 10);
    assert_eq!(
        s.commit(&[Write::Custody(CustodyWrite::Open(change.clone()))]),
        Err(StoreRefusal::Custody(CustodyRefusal::NotEntry))
    );
    let (mut s, change) = store_with_change(10, 20);
    s.commit(&[Write::Custody(CustodyWrite::Open(change.clone()))])
        .unwrap();
    // One pending per account and kind.
    let mut second = change.clone();
    second.id = PendingId(OperationId::from_ref(OperationRef([0xC2; 16])));
    assert_eq!(
        s.commit(&[Write::Custody(CustodyWrite::Open(second))]),
        Err(StoreRefusal::Custody(CustodyRefusal::AlreadyPending))
    );
    let close = |to, at| {
        Write::Custody(CustodyWrite::Close {
            id: change.id,
            to,
            closed: Closed {
                at,
                by: OperationId::from_ref(OperationRef([0xC3; 16])),
            },
        })
    };
    assert_eq!(
        s.commit(&[close(ChangeState::Completed, 19)]),
        Err(StoreRefusal::Custody(CustodyRefusal::NotElapsed))
    );
    assert_eq!(
        s.commit(&[close(ChangeState::Pending, 30)]),
        Err(StoreRefusal::Custody(CustodyRefusal::NotInTable))
    );
    s.commit(&[close(ChangeState::Completed, 20)]).unwrap();
    // Terminal states absorb, whoever writes.
    for to in ChangeState::ALL {
        assert_eq!(
            s.commit(&[close(to, 40)]),
            Err(StoreRefusal::Custody(CustodyRefusal::Terminal))
        );
    }
}

#[test]
fn the_store_refuses_a_custody_change_that_changes_nothing() {
    let (mut s, change) = store_with_change(10, 20);
    let custody = s.account(&change.account).unwrap().custody();
    assert_eq!(
        s.commit(&[Write::Custody(CustodyWrite::SetCustody {
            account: change.account,
            custody
        })]),
        Err(StoreRefusal::Custody(CustodyRefusal::NoChange))
    );
    assert_eq!(
        s.commit(&[Write::Custody(CustodyWrite::SetOwner {
            account: change.account,
            owner: change.owner
        })]),
        Err(StoreRefusal::Custody(CustodyRefusal::NoChange))
    );
    let _ = RecoveryConfig::not_offered();
    let _ = RecoveryKey([0; 32]);
}

// ---------------------------------------------------------------- the set index (issue #50)

/// The set index agrees with a scan of every credential of the book: the definition
/// `credentials_of` had before it was indexed, same members, same order.
fn sets_hold(s: &State) {
    assert!(s.credential_sets_consistent(), "the set index drifted");
    for i in s.identities() {
        let indexed: Vec<CredentialId> = s.credentials_of(&i.id()).map(Credential::id).collect();
        let scanned: Vec<CredentialId> = s
            .credentials()
            .filter(|c| c.identity() == i.id())
            .map(Credential::id)
            .collect();
        assert_eq!(indexed, scanned, "{:?}'s set", i.id());
    }
}

/// The store rebuilt from its export sections, which must come out byte-identical: the index is
/// derived state, not a section.
fn rebuilt(s: &State) -> State {
    let head = crate::export::AuditHead {
        emitted: s.audit().emitted(),
        head: s.audit().head_hash(),
        shipped: s.audit().shipped(),
    };
    let back = State::from_sections(&s.sections(), &head).unwrap();
    assert!(back.sections() == s.sections());
    back
}

/// `REQ-AU-20`: every command that changes a credential set — genesis, `open_identity`,
/// `register_credential`, `remove_credential`, `complete_recovery` — keeps the set index equal
/// to the credentials, in the same commit; so do refused commands, the commands that change
/// custody or owner without touching a set (a delegation, a reclaim, a succession), and the
/// rebuild from an export.
#[test]
fn req_au_20_the_set_index_follows_every_credential_changing_command() {
    let mut r = Recovery::new();
    let (alice, acct) = (r.alice, r.acct);
    sets_hold(r.fx.ledger.state());
    // open_identity: a person and an agent.
    let bob = r.fx.person();
    r.fx.agent(bob.principal);
    sets_hold(r.fx.ledger.state());
    // register_credential, and a refused one.
    let second = register(
        &mut r.fx,
        alice,
        &secret(1),
        CredentialMarker::Ordinary,
        None,
    )
    .unwrap();
    let scoped = register(
        &mut r.fx,
        alice,
        &secret(2),
        CredentialMarker::Ordinary,
        Some(NonEmptySet::one(acct)),
    )
    .unwrap();
    let key = testing::config().recovery_key.unwrap();
    let ctx = r.fx.ctx(alice);
    assert_eq!(
        r.fx.ledger
            .register_credential(
                &ctx,
                RegisterCredential {
                    marker: CredentialMarker::Ordinary,
                    scope: None,
                    verifier: Verifier::new(key.0.to_vec()),
                },
            )
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    sets_hold(r.fx.ledger.state());
    // remove_credential, and the refused removal of Bob's last one.
    let ctx = r.fx.ctx(alice);
    r.fx.ledger.remove_credential(&ctx, second).unwrap();
    let ctx = r.fx.ctx(bob);
    assert_eq!(
        r.fx.ledger
            .remove_credential(&ctx, bob.credential)
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    sets_hold(r.fx.ledger.state());
    // A delegation and a reclaim change custody, not a set.
    let own = register(
        &mut r.fx,
        alice,
        &secret(3),
        CredentialMarker::Ordinary,
        None,
    )
    .unwrap();
    let ctx = r.fx.ctx(alice);
    r.fx.ledger.delegate_custody(&ctx, acct).unwrap();
    reclaim(&mut r.fx, alice, acct, &secret(3), own).unwrap();
    sets_hold(r.fx.ledger.state());
    // complete_recovery: every member covering the account removed and a new one put, in one
    // commit.
    let delay = testing::config().recovery_delay;
    let pending = r.initiate(100).unwrap();
    let approval = r.approval(pending);
    let new = r.complete(pending, approval, 100 + delay).unwrap().value;
    for old in [alice.credential, scoped, own] {
        assert_eq!(fx_cred_status(&r.fx, old), CredentialStatus::Removed);
    }
    sets_hold(r.fx.ledger.state());
    // A succession passes the account; the sets stay as they are.
    let ctx = r.fx.ctx(Caller::new(alice.principal, new));
    r.fx.ledger
        .name_successor(&ctx, acct, Some(bob.principal))
        .unwrap();
    let t = 100 + delay + 1;
    let succession = testing::config().succession_delay.unwrap();
    for now in [t, t + succession] {
        let op = r.fx.op();
        r.fx.ledger
            .claim_succession(&at(bob, op, now), acct)
            .unwrap();
    }
    assert_eq!(
        r.fx.ledger.state().account(&acct).unwrap().owner(),
        bob.principal
    );
    sets_hold(r.fx.ledger.state());
    // The rebuild from an export derives the index again.
    sets_hold(&rebuilt(r.fx.ledger.state()));
}

// ---------------------------------------------------------------- the owned-custody index

/// The owned-custody index agrees with a scan of every account of the book, and answers
/// `holds_owned_custody` as a scan of the owner's accounts did.
fn owned_hold(s: &State) {
    assert!(
        s.owned_custody_consistent(),
        "the owned-custody index drifted"
    );
    for i in s.identities() {
        assert_eq!(
            s.holds_owned_custody(&i.id()),
            s.accounts_of(&i.id())
                .any(|a| a.custody() == Custody::Owned),
            "{:?}",
            i.id()
        );
    }
}

/// `REQ-RT-1` (a verify-genesis halt of 2026-10-11): whether an identity holds an
/// account in owned custody is one index leaf, not every account it owns. The index follows every
/// write that opens an account, switches its custody or passes it to another owner: an opening
/// in owned custody, a delegation and a reclaim, a succession, a switch taken back, and the
/// rebuild from an export.
#[test]
fn req_rt_1_the_owned_custody_index_follows_custody_and_owner() {
    let mut r = Recovery::new();
    let (alice, acct) = (r.alice, r.acct);
    owned_hold(r.fx.ledger.state());
    assert!(r.fx.ledger.state().holds_owned_custody(&alice.principal));
    let bob = r.fx.person();
    owned_hold(r.fx.ledger.state());
    assert!(!r.fx.ledger.state().holds_owned_custody(&bob.principal));
    // A delegation leaves Alice none; a reclaim gives it back.
    let own = register(
        &mut r.fx,
        alice,
        &secret(3),
        CredentialMarker::Ordinary,
        None,
    )
    .unwrap();
    let ctx = r.fx.ctx(alice);
    r.fx.ledger.delegate_custody(&ctx, acct).unwrap();
    owned_hold(r.fx.ledger.state());
    assert!(!r.fx.ledger.state().holds_owned_custody(&alice.principal));
    reclaim(&mut r.fx, alice, acct, &secret(3), own).unwrap();
    owned_hold(r.fx.ledger.state());
    assert!(r.fx.ledger.state().holds_owned_custody(&alice.principal));
    // A switch taken back leaves the index as it was.
    let mut s = rebuilt(r.fx.ledger.state());
    let c = s
        .commit_undoable(&[Write::Custody(CustodyWrite::SetCustody {
            account: acct,
            custody: Custody::Delegated,
        })])
        .unwrap();
    owned_hold(&s);
    assert!(!s.holds_owned_custody(&alice.principal));
    s.undo(c);
    owned_hold(&s);
    assert!(s.holds_owned_custody(&alice.principal));
    // A succession passes the account in owned custody to Bob.
    let ctx = r.fx.ctx(alice);
    r.fx.ledger
        .name_successor(&ctx, acct, Some(bob.principal))
        .unwrap();
    let succession = testing::config().succession_delay.unwrap();
    for now in [100, 100 + succession] {
        let op = r.fx.op();
        r.fx.ledger
            .claim_succession(&at(bob, op, now), acct)
            .unwrap();
    }
    assert_eq!(
        r.fx.ledger.state().account(&acct).unwrap().owner(),
        bob.principal
    );
    owned_hold(r.fx.ledger.state());
    assert!(r.fx.ledger.state().holds_owned_custody(&bob.principal));
    assert!(!r.fx.ledger.state().holds_owned_custody(&alice.principal));
    // The rebuild from an export derives the index again.
    owned_hold(&rebuilt(r.fx.ledger.state()));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// `REQ-AU-20`, at the store: whatever sequence of credential writes is applied, refused,
    /// checked or rolled back, `credentials_of` is the identity's set — the scan of every
    /// credential it replaced — and the index equals a recomputation.
    #[test]
    fn req_au_20_the_set_index_is_the_scan_of_the_credentials(
        steps in proptest::collection::vec((0u8..6, 0usize..4, 0usize..64), 1..48)
    ) {
        fn op(a: u8, b: u8) -> OperationId {
            let mut r = [0u8; 16];
            r[0] = a;
            r[1] = b;
            OperationId::from_ref(OperationRef(r))
        }
        let mut s = State::new();
        let people: Vec<Identity> = (0..4)
            .map(|i| Identity::new(Mint::new(op(1, i), 0), IdentityKind::Person, None).unwrap())
            .collect();
        s.commit(&people.iter().map(|p| Write::PutIdentity(*p)).collect::<Vec<_>>()).unwrap();
        let mut made: Vec<CredentialId> = Vec::new();
        for (n, (kind, who, pick)) in steps.into_iter().enumerate() {
            let fresh = Credential::new(
                Mint::new(op(2, n as u8), 0),
                people[who].id(),
                CredentialMarker::Ordinary,
                None,
                Verifier::new(vec![n as u8]),
            );
            let existing = (!made.is_empty()).then(|| made[pick % made.len()]);
            match kind {
                // Put a credential.
                0 | 1 => {
                    s.commit(&[Write::PutCredential(fresh.clone())]).unwrap();
                    made.push(fresh.id());
                }
                // Remove one.
                2 => {
                    if let Some(id) = existing {
                        s.commit(&[Write::CredentialStatus(id, CredentialStatus::Removed)]).unwrap();
                    }
                }
                // A refused batch: a put and a status change, then a write the store refuses.
                3 => {
                    let mut batch = vec![Write::PutCredential(fresh.clone())];
                    if let Some(id) = existing {
                        batch.push(Write::CredentialStatus(id, CredentialStatus::Removed));
                    }
                    batch.push(Write::PutCredential(fresh.clone()));
                    prop_assert_eq!(s.commit(&batch), Err(StoreRefusal::Exists));
                }
                // A check changes nothing.
                4 => {
                    prop_assert!(s.check(&[Write::PutCredential(fresh.clone())]).is_ok());
                }
                // A fault after the put rolls it back (`F-007`'s undo path).
                _ => {
                    prop_assert!(s.commit_faulty(&[Write::PutCredential(fresh.clone())], Some(1)).is_err());
                }
            }
            sets_hold(&s);
        }
        prop_assert_eq!(s.credentials().count(), made.len());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `REQ-RT-1`, at the store: whatever sequence of account writes is applied, refused, checked
    /// or rolled back, the owned-custody index equals a recomputation from the accounts.
    #[test]
    fn req_rt_1_the_owned_custody_index_is_the_scan_of_the_accounts(
        steps in proptest::collection::vec((0u8..7, 0usize..4, 0usize..64, any::<bool>()), 1..64)
    ) {
        fn op(a: u8, b: u8) -> OperationId {
            let mut r = [0u8; 16];
            r[0] = a;
            r[1] = b;
            OperationId::from_ref(OperationRef(r))
        }
        let mut s = State::new();
        let people: Vec<Identity> = (0..4)
            .map(|i| Identity::new(Mint::new(op(1, i), 0), IdentityKind::Person, None).unwrap())
            .collect();
        s.commit(&people.iter().map(|p| Write::PutIdentity(*p)).collect::<Vec<_>>()).unwrap();
        let mut made: Vec<AccountId> = Vec::new();
        for (n, (kind, who, pick, owned)) in steps.into_iter().enumerate() {
            let fresh = LedgerAccount::new(
                Mint::new(op(2, n as u8), 0),
                people[who].id(),
                AccountKind::Customer,
                if owned { Custody::Owned } else { Custody::Delegated },
                0,
                BTreeMap::new(),
            ).unwrap();
            let existing = (!made.is_empty()).then(|| made[pick % made.len()]);
            let toggle = |s: &State, id: AccountId| Write::Custody(CustodyWrite::SetCustody {
                account: id,
                custody: match s.account(&id).unwrap().custody() {
                    Custody::Owned => Custody::Delegated,
                    Custody::Delegated => Custody::Owned,
                },
            });
            let move_to = |id: AccountId| Write::Custody(CustodyWrite::SetOwner {
                account: id,
                owner: people[who].id(),
            });
            match kind {
                0 => {
                    s.commit(&[Write::PutAccount(fresh.clone())]).unwrap();
                    made.push(fresh.id());
                }
                1 => if let Some(id) = existing { let w = toggle(&s, id); let _ = s.commit(&[w]); },
                2 => if let Some(id) = existing { let _ = s.commit(&[move_to(id)]); },
                3 => if let Some(id) = existing {
                    let _ = s.commit(&[Write::AccountStatus(id, AccountStatus::Closed)]);
                },
                // A check changes nothing.
                4 => {
                    let mut b = vec![Write::PutAccount(fresh.clone())];
                    if let Some(id) = existing { b.push(toggle(&s, id)); b.push(move_to(id)); }
                    let _ = s.check(&b);
                }
                // A refused batch.
                5 => {
                    let mut b = Vec::new();
                    if let Some(id) = existing { b.push(move_to(id)); b.push(toggle(&s, id)); }
                    b.push(Write::PutAccount(fresh.clone()));
                    b.push(Write::PutAccount(fresh.clone()));
                    prop_assert!(s.commit(&b).is_err());
                }
                // A fault after every write rolls the batch back.
                _ => {
                    let mut b = vec![Write::PutAccount(fresh.clone())];
                    if let Some(id) = existing { b.push(move_to(id)); b.push(toggle(&s, id)); }
                    let l = b.len();
                    prop_assert!(s.commit_faulty(&b, Some(l)).is_err());
                }
            }
            owned_hold(&s);
        }
    }
}
