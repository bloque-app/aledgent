// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of the commands, over `F-001`'s in-memory book. The service-layer acceptance and
//! error cases run in the conformance suite (`crates/conformance/src/cases/f003.rs`) on both
//! stores; these cover what the suite does not: principal resolution in detail, scoped
//! credentials, and the property tests of `T-003-07`.

use super::*;
use alloc::collections::BTreeSet;
use core::num::NonZeroU64;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;
use proptest::prelude::*;

use crate::principal::CredentialStatus;

type MemLedger = Ledger<MemStore, MemVault>;

struct Fx {
    ledger: MemLedger,
    founded: Founded,
    next: u64,
    issuance_grant: Option<GrantId>,
}

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn old() -> AssetId {
    AssetId::parse("OLD").unwrap()
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

impl Fx {
    fn new() -> Fx {
        let book = Book::bootstrap(
            MemStore::new(),
            MemVault::new(),
            KeyRing::from_root([7; 32]),
        )
        .unwrap();
        let genesis = Genesis {
            assets: vec![
                AssetDecl {
                    id: usd(),
                    location: Some(crate::asset::AssetLocation::parse("Here(101)").unwrap()),
                    precision: 2,
                    kind: AssetKind::Fiat,
                    deprecated: false,
                },
                AssetDecl {
                    id: old(),
                    location: Some(crate::asset::AssetLocation::parse("Here(102)").unwrap()),
                    precision: 2,
                    kind: AssetKind::Fiat,
                    deprecated: true,
                },
            ],
            operator_verifier: Verifier::new(vec![0]),
            lifecycle: crate::lifecycle::LifecycleConfig::new([7; 16], 60, 3_600)
                .with_default_deadline(OperationKind::Reserve, 1_000)
                .with_recovery(crate::custody::testing::config())
                .with_anchoring(crate::anchor::AnchoringConfig {
                    interval: 100,
                    assurance: Some(crate::anchor::AssuranceDeclaration {
                        digest: [0xA5; 32],
                        summary: "Unit-test deployment.".into(),
                    }),
                }),
            chainspec: None,
            spec: None,
        };
        let (ledger, founded) = Ledger::genesis(
            book,
            genesis,
            OperationId::from_ref(OperationRef([0; 16])),
            1,
        )
        .unwrap();
        Fx {
            ledger,
            founded,
            next: 1,
            issuance_grant: None,
        }
    }

    fn op(&mut self) -> OperationId {
        self.next += 1;
        let mut b = [0u8; 16];
        b[8..].copy_from_slice(&self.next.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        Ctx {
            caller,
            op: self.op(),
            now: 10,
        }
    }

    fn operator(&self) -> Caller {
        Caller::new(self.founded.operator, self.founded.operator_credential)
    }

    fn person(&mut self) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .ledger
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn agent(&mut self, of: IdentityId) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .ledger
            .open_identity(&ctx, IdentityKind::Agent, Some(of), Verifier::new(vec![2]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    /// The acknowledgement of owned custody `owner` carries (`F-012`, `AC-A5.3`).
    fn ack(&self, owner: &Caller, custody: Custody) -> Option<crate::custody::MechanismsDigest> {
        (custody == Custody::Owned).then(|| {
            self.ledger
                .recovery_terms(owner, &owner.principal)
                .unwrap()
                .digest()
        })
    }

    fn account(&mut self, owner: &Caller, custody: Custody) -> AccountId {
        let ctx = self.ctx(*owner);
        let acknowledged = self.ack(owner, custody);
        self.ledger
            .open_account(
                &ctx,
                OpenAccount {
                    owner: owner.principal,
                    kind: AccountKind::Customer,
                    custody,
                    treasury_limits: BTreeMap::new(),
                    acknowledged,
                },
            )
            .unwrap()
            .value
    }

    /// An issuance grant for USD to the operator, bounded per window and per asset.
    fn issuer(&mut self) -> Caller {
        let op = self.operator();
        let ctx = self.ctx(op);
        let grant = self
            .ledger
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Issuance,
                    principal: op.principal,
                    subject: Subject::Asset(usd()),
                    ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                        .unwrap(),
                    per_operation: None,
                    per_window: Some(Window {
                        length: NonZeroU64::new(1_000_000).unwrap(),
                        limit: nz(u64::MAX / 2),
                    }),
                    assets: Some(NonEmptySet::one(usd())),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap()
            .value;
        self.issuance_grant = Some(grant);
        op
    }

    fn delegate(
        &mut self,
        owner: &Caller,
        to: IdentityId,
        account: AccountId,
        ops: &[OperationKind],
        per_op: u64,
    ) -> GrantId {
        let ctx = self.ctx(*owner);
        self.ledger
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
                    parent: None,
                    expires: None,
                },
            )
            .unwrap()
            .value
    }

    fn transfer(&self, account: AccountId, amount: u64) -> Request {
        Request {
            kind: OperationKind::Transfer,
            account,
            asset: Some(usd()),
            amount: Some(nz(amount)),
            counterparty: Some(self.founded.issuance),
        }
    }
}

#[test]
fn req_pn_1_an_unresolvable_initiator_is_credential_invalid() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let bob = fx.person();
    let acct = fx.account(&alice, Custody::Owned);
    let r = fx.transfer(acct, 1);
    let bad = [
        // A credential of another identity.
        Caller::new(alice.principal, bob.credential),
        // An identity that does not exist.
        Caller::new(IdentityId::mint(&fx.op(), 0), alice.credential),
        // A credential that does not exist.
        Caller::new(alice.principal, CredentialId::mint(&fx.op(), 1)),
        // Acting for an identity that does not exist.
        alice.on_behalf_of(IdentityId::mint(&fx.op(), 0)),
    ];
    for caller in bad {
        let ctx = fx.ctx(caller);
        assert_eq!(
            fx.ledger.authorise(&ctx, &r),
            Err(Refusal::CredentialInvalid)
        );
    }
    // A removed credential stops working at once.
    fx.ledger
        .state
        .commit(&[Write::CredentialStatus(
            alice.credential,
            CredentialStatus::Removed,
        )])
        .unwrap();
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger.authorise(&ctx, &r),
        Err(Refusal::CredentialInvalid)
    );
}

#[test]
fn req_ag_1_an_agent_acts_for_its_principal_only() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let bob = fx.person();
    let agent = fx.agent(alice.principal);
    let acct = fx.account(&alice, Custody::Owned);
    fx.delegate(
        &alice,
        agent.principal,
        acct,
        &[OperationKind::Transfer],
        100,
    );
    let r = fx.transfer(acct, 10);
    let ctx = fx.ctx(agent);
    let d = fx.ledger.authorise(&ctx, &r).unwrap();
    assert_eq!(d.used.kind, AuthorityKind::Delegated);
    assert_eq!(d.used.principal, agent.principal);
    assert_eq!(d.used.on_behalf_of, Some(alice.principal));
    // Claiming to act for someone else is not a resolvable initiator.
    let ctx = fx.ctx(agent.on_behalf_of(bob.principal));
    assert_eq!(
        fx.ledger.authorise(&ctx, &r),
        Err(Refusal::CredentialInvalid)
    );
    // Nor is the agent using its principal's credential.
    let ctx = fx.ctx(Caller::new(agent.principal, alice.credential));
    assert_eq!(
        fx.ledger.authorise(&ctx, &r),
        Err(Refusal::CredentialInvalid)
    );
}

#[test]
fn req_au_20_a_scoped_credential_cannot_act_outside_its_scope() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let a1 = fx.account(&alice, Custody::Owned);
    let a2 = fx.account(&alice, Custody::Owned);
    let op = fx.op();
    let scoped = Credential::new(
        Mint::new(op, 0),
        alice.principal,
        CredentialMarker::Ordinary,
        Some(NonEmptySet::one(a1)),
        Verifier::new(vec![9]),
    );
    let scoped_id = scoped.id();
    fx.ledger
        .state
        .commit(&[Write::PutCredential(scoped)])
        .unwrap();
    let caller = Caller::new(alice.principal, scoped_id);
    let ctx = fx.ctx(caller);
    let d = fx.ledger.authorise(&ctx, &fx.transfer(a1, 5)).unwrap();
    assert_eq!(d.used.kind, AuthorityKind::Owner);
    assert_eq!(d.used.credential, scoped_id);
    assert_eq!(
        fx.ledger.authorise(&ctx, &fx.transfer(a2, 5)),
        Err(Refusal::UnknownAccount)
    );
    assert_eq!(
        fx.ledger.reach_identity(&caller, &alice.principal, 10),
        Ok(BTreeSet::from([a1]))
    );
    // Nor can it open accounts or hold issuance authority over an asset.
    let ctx = fx.ctx(caller);
    assert_eq!(
        fx.ledger
            .open_account(
                &ctx,
                OpenAccount {
                    owner: alice.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Owned,
                    treasury_limits: BTreeMap::new(),
                    acknowledged: None,
                }
            )
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn req_au_5_operator_authority_needs_delegated_custody_at_evaluation() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let owned = fx.account(&alice, Custody::Owned);
    let delegated = fx.account(&alice, Custody::Delegated);
    let op = fx.operator();
    let ctx = fx.ctx(op);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Operator,
                principal: op.principal,
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
    // A grant naming the owned account itself is no better.
    let ctx = fx.ctx(op);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Operator,
                principal: op.principal,
                subject: Subject::Account(owned),
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
    let ctx = fx.ctx(op);
    let d = fx
        .ledger
        .authorise(&ctx, &fx.transfer(delegated, 10))
        .unwrap();
    assert_eq!(d.used.kind, AuthorityKind::Operator);
    // In owned custody no operator authority exists over the account: it does not even reach it.
    assert_eq!(
        fx.ledger.authorise(&ctx, &fx.transfer(owned, 10)),
        Err(Refusal::UnknownAccount)
    );
    // Operator grants name kinds: closing is not among them.
    let close = Request {
        kind: OperationKind::CloseAccount,
        account: delegated,
        asset: None,
        amount: None,
        counterparty: None,
    };
    assert_eq!(
        fx.ledger.authorise(&ctx, &close),
        Err(Refusal::NotAuthorised)
    );
    // And the operator cannot redeem from an owned account, even with issuance authority.
    let issuer = fx.issuer();
    let ctx = fx.ctx(issuer);
    fx.ledger.issue(&ctx, usd(), owned, Minor::new(50)).unwrap();
    let ctx = fx.ctx(issuer);
    assert_eq!(
        fx.ledger
            .redeem(&ctx, usd(), owned, Minor::new(50))
            .map(|a| a.value),
        Err(Refusal::UnknownAccount)
    );
}

#[test]
fn req_pv_2_a_grant_never_reaches_the_grantors_other_accounts() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let bob = fx.person();
    let a1 = fx.account(&alice, Custody::Owned);
    let a2 = fx.account(&alice, Custody::Owned);
    fx.delegate(&alice, bob.principal, a1, &[OperationKind::Transfer], 10);
    assert_eq!(
        fx.ledger.reach_account(&bob, &a1, 10),
        Ok(AuthorityKind::Delegated)
    );
    assert_eq!(
        fx.ledger.reach_account(&bob, &a2, 10),
        Err(Refusal::UnknownAccount)
    );
    assert_eq!(
        fx.ledger.reach_identity(&bob, &alice.principal, 10),
        Ok(BTreeSet::from([a1]))
    );
    let carol = fx.person();
    assert_eq!(
        fx.ledger.reach_identity(&carol, &alice.principal, 10),
        Err(Refusal::UnknownAccount)
    );
    assert_eq!(
        fx.ledger.available(&carol, &a1, &usd(), 10),
        Err(Refusal::UnknownAccount)
    );
}

#[test]
fn window_charges_are_made_only_with_the_posting() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Owned);
    let op = fx.operator();
    let ctx = fx.ctx(op);
    let grant = fx
        .ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Issuance,
                principal: op.principal,
                subject: Subject::Asset(usd()),
                ops: NonEmptySet::one(OperationKind::Issue),
                per_operation: None,
                per_window: Some(Window {
                    length: NonZeroU64::new(100).unwrap(),
                    limit: nz(100),
                }),
                assets: Some(NonEmptySet::one(usd())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap()
        .value;
    let ctx = fx.ctx(op);
    fx.ledger.issue(&ctx, usd(), acct, Minor::new(60)).unwrap();
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger
            .issue(&ctx, usd(), acct, Minor::new(41))
            .map(|a| a.value),
        Err(Refusal::BoundExceeded)
    );
    let ctx = fx.ctx(op);
    fx.ledger.issue(&ctx, usd(), acct, Minor::new(40)).unwrap();
    let key = crate::authority::WindowKey {
        grant,
        window: 0,
        asset: usd(),
    };
    assert_eq!(fx.ledger.state().used(&key), Minor::new(100));
    // A new window starts from zero.
    let mut ctx = fx.ctx(op);
    ctx.now = 150;
    fx.ledger.issue(&ctx, usd(), acct, Minor::new(100)).unwrap();
    // A refused posting charges nothing: a deprecated asset is refused before any charge.
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger
            .issue(&ctx, old(), acct, Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::AssetDeprecated)
    );
    assert_eq!(fx.ledger.available(&alice, &acct, &usd(), 10), Ok(200));
}

#[test]
fn a_kernel_refusal_changes_nothing_in_the_store() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let issuer = fx.issuer();
    let ctx = fx.ctx(issuer);
    fx.ledger.issue(&ctx, usd(), acct, Minor::new(10)).unwrap();
    // The operator may redeem from a delegated account with an operator grant for Redeem.
    let ctx = fx.ctx(issuer);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Operator,
                principal: issuer.principal,
                subject: Subject::DelegatedAccounts,
                ops: NonEmptySet::one(OperationKind::Redeem),
                per_operation: Some(nz(1_000)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    let ctx = fx.ctx(issuer);
    let op = ctx.op;
    assert_eq!(
        fx.ledger
            .redeem(&ctx, usd(), acct, Minor::new(11))
            .map(|a| a.value),
        Err(Refusal::InsufficientAvailable)
    );
    assert!(fx.ledger.state().record(&op).is_none());
    let ctx = fx.ctx(issuer);
    let r = fx.ledger.redeem(&ctx, usd(), acct, Minor::new(10)).unwrap();
    assert_eq!(r.record.authorities.len(), 2);
    assert_eq!(r.record.authorities[0].kind, AuthorityKind::Operator);
    assert_eq!(r.record.authorities[1].kind, AuthorityKind::Issuance);
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
    // The refused redemption charged nothing: 10 issued and 10 redeemed.
    let key = crate::authority::WindowKey {
        grant: fx.issuance_grant.unwrap(),
        window: 0,
        asset: usd(),
    };
    assert_eq!(fx.ledger.state().used(&key), Minor::new(20));
}

/// `F-007` (`AC-A1.3`, `REQ-OP-10`): an operation reference is accepted once. The same request
/// again returns the original outcome and writes nothing; any other request under it is
/// `ERR-ClientKeyReused`.
#[test]
fn an_operation_reference_is_used_once() {
    let mut fx = Fx::new();
    let alice = fx.person();
    let bob = fx.person();
    let ctx = fx.ctx(alice);
    let req = OpenAccount {
        owner: alice.principal,
        kind: AccountKind::Customer,
        custody: Custody::Owned,
        treasury_limits: BTreeMap::new(),
        acknowledged: fx.ack(&alice, Custody::Owned),
    };
    let first = fx.ledger.open_account(&ctx, req.clone()).unwrap();
    let accounts = fx.ledger.state().all_accounts().count();
    let again = fx.ledger.open_account(&ctx, req.clone()).unwrap();
    assert_eq!(again, first);
    assert_eq!(fx.ledger.state().all_accounts().count(), accounts);
    // A different body under the same reference.
    let mut other = req.clone();
    other.custody = Custody::Delegated;
    assert_eq!(
        fx.ledger.open_account(&ctx, other).map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
    // Another principal under the same reference.
    let theirs = Ctx { caller: bob, ..ctx };
    let req_b = OpenAccount {
        owner: bob.principal,
        ..req
    };
    assert_eq!(
        fx.ledger.open_account(&theirs, req_b).map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
    // Another command under the same reference.
    assert_eq!(
        fx.ledger.close_account(&ctx, first.value).map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
}

#[test]
fn treasury_accounts_redeem_down_to_their_limit() {
    let mut fx = Fx::new();
    let org = fx.person();
    let op = fx.operator();
    let ctx = fx.ctx(op);
    let treasury = fx
        .ledger
        .open_account(
            &ctx,
            OpenAccount {
                owner: org.principal,
                kind: AccountKind::Treasury,
                custody: Custody::Delegated,
                treasury_limits: BTreeMap::from([(usd(), Minor::new(30))]),
                acknowledged: None,
            },
        )
        .unwrap()
        .value;
    // A holder cannot open a Treasury account for itself.
    let ctx = fx.ctx(org);
    assert_eq!(
        fx.ledger
            .open_account(
                &ctx,
                OpenAccount {
                    owner: org.principal,
                    kind: AccountKind::Treasury,
                    custody: Custody::Owned,
                    treasury_limits: BTreeMap::new(),
                    acknowledged: None,
                }
            )
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let issuer = fx.issuer();
    let ctx = fx.ctx(issuer);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Operator,
                principal: issuer.principal,
                subject: Subject::Account(treasury),
                ops: NonEmptySet::one(OperationKind::Redeem),
                per_operation: Some(nz(1_000)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    let ctx = fx.ctx(issuer);
    fx.ledger
        .redeem(&ctx, usd(), treasury, Minor::new(30))
        .unwrap();
    let ctx = fx.ctx(issuer);
    assert_eq!(
        fx.ledger
            .redeem(&ctx, usd(), treasury, Minor::new(1))
            .map(|a| a.value),
        Err(Refusal::InsufficientAvailable)
    );
    assert_eq!(fx.ledger.available(&org, &treasury, &usd(), 10), Ok(-30));
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    // INV-5, T-003-07: the trial balance stays zero across random issue and redeem schedules, and
    // every account's balance equals what the schedule says it should be.
    #[test]
    fn inv_5_trial_balance_stays_zero_across_issue_and_redeem(
        steps in prop::collection::vec((0usize..3, any::<bool>(), 1u64..500), 1..12),
    ) {
        let mut fx = Fx::new();
        let alice = fx.person();
        let accounts: Vec<AccountId> = (0..3).map(|_| fx.account(&alice, Custody::Delegated)).collect();
        let issuer = fx.issuer();
        let ctx = fx.ctx(issuer);
        fx.ledger.grant_authority(&ctx, GrantRequest {
            kind: GrantKind::Operator,
            principal: issuer.principal,
            subject: Subject::DelegatedAccounts,
            ops: NonEmptySet::one(OperationKind::Redeem),
            per_operation: Some(nz(u64::MAX)),
            per_window: None,
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        }).unwrap();
        let mut expected = [0i128; 3];
        for (i, issue, amount) in steps {
            let ctx = fx.ctx(issuer);
            if issue {
                fx.ledger.issue(&ctx, usd(), accounts[i], Minor::new(amount)).unwrap();
                expected[i] += i128::from(amount);
            } else {
                let got = fx.ledger.redeem(&ctx, usd(), accounts[i], Minor::new(amount));
                if expected[i] >= i128::from(amount) {
                    prop_assert!(got.is_ok());
                    expected[i] -= i128::from(amount);
                } else {
                    prop_assert_eq!(got.map(|a| a.value), Err(Refusal::InsufficientAvailable));
                }
            }
            prop_assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
        }
        for (i, a) in accounts.iter().enumerate() {
            prop_assert_eq!(fx.ledger.available(&alice, a, &usd(), 10), Ok(expected[i]));
        }
    }
}

// ---------------------------------------------------------------- reservations (F-004)

/// A funded customer account of a new person: `(person, account)`.
fn funded(fx: &mut Fx, amount: u64) -> (Caller, AccountId) {
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Owned);
    if fx.issuance_grant.is_none() {
        fx.issuer();
    }
    let issuer = fx.operator();
    let ctx = fx.ctx(issuer);
    fx.ledger
        .issue(&ctx, usd(), acct, Minor::new(amount))
        .unwrap();
    (alice, acct)
}

#[test]
fn req_la_4_close_account_refuses_while_a_reservation_is_open() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve(&reserve, acct, usd(), Minor::new(100))
        .unwrap();
    // Available is zero; reserved is not. Before F-004 this account would have closed.
    assert_eq!(fx.ledger.available(&alice, &acct, &usd(), 10), Ok(0));
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger.close_account(&ctx, acct).map(|a| a.value),
        Err(Refusal::AccountNotEmpty)
    );
    // Released, the value is available again, and still not zero.
    let release = Ctx {
        op: reserve.op,
        ..fx.ctx(alice)
    };
    fx.ledger.release(&release).unwrap();
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger.close_account(&ctx, acct).map(|a| a.value),
        Err(Refusal::AccountNotEmpty)
    );
    // Captured elsewhere in full, the account is empty in every kind and closes.
    let (_, other) = funded(&mut fx, 1);
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve(&reserve, acct, usd(), Minor::new(100))
        .unwrap();
    let capture = Ctx {
        op: reserve.op,
        ..fx.ctx(alice)
    };
    fx.ledger.capture(&capture, other, Minor::new(100)).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.close_account(&ctx, acct).unwrap();
}

#[test]
fn inv_5_holding_positions_are_part_of_the_trial_balance() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let reserve = fx.ctx(alice);
    fx.ledger
        .reserve(&reserve, acct, usd(), Minor::new(40))
        .unwrap();
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
    let view = fx.ledger.balances(&alice, &acct, &usd(), 10).unwrap();
    assert_eq!(
        (view.available, view.reserved, view.encumbered, view.total),
        (60, 40, 0, 100)
    );
    let (_, other) = funded(&mut fx, 1);
    let capture = Ctx {
        op: reserve.op,
        ..fx.ctx(alice)
    };
    fx.ledger.capture(&capture, other, Minor::new(25)).unwrap();
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
    let view = fx.ledger.balances(&alice, &acct, &usd(), 10).unwrap();
    assert_eq!((view.available, view.reserved, view.total), (75, 0, 75));
    assert_eq!(fx.ledger.check_holdings(), Ok(()));
    // Nobody without reach reads the view (INV-16).
    let bob = fx.person();
    assert_eq!(
        fx.ledger.balances(&bob, &acct, &usd(), 10),
        Err(Refusal::UnknownAccount)
    );
}

#[test]
fn capture_and_release_charge_no_window_and_are_recorded_as_transitions() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let agent = fx.agent(alice.principal);
    let ctx = fx.ctx(alice);
    let grant = fx
        .ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: agent.principal,
                subject: Subject::Account(acct),
                ops: NonEmptySet::new(
                    [
                        OperationKind::Reserve,
                        OperationKind::Capture,
                        OperationKind::Release,
                    ]
                    .into(),
                )
                .unwrap(),
                per_operation: None,
                per_window: Some(Window {
                    length: NonZeroU64::new(1_000).unwrap(),
                    limit: nz(50),
                }),
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap()
        .value;
    let key = crate::authority::WindowKey {
        grant,
        window: 0,
        asset: usd(),
    };
    let reserve = fx.ctx(agent);
    let r = fx
        .ledger
        .reserve(&reserve, acct, usd(), Minor::new(30))
        .unwrap();
    assert_eq!(r.record.command, Command::Reserve);
    assert_eq!(fx.ledger.state().used(&key), Minor::new(30));
    let release = Ctx {
        op: reserve.op,
        ..fx.ctx(agent)
    };
    let t = fx.ledger.release(&release).unwrap();
    assert_eq!(fx.ledger.state().used(&key), Minor::new(30));
    assert_eq!(t.record.command, Command::Release);
    assert_eq!(t.record.authorities[0].grant, Some(grant));
    assert_eq!(fx.ledger.state().transitions(&reserve.op), &[t.record]);
    // The window still holds the reservation's charge: another 30 would pass the 50.
    let again = fx.ctx(agent);
    assert_eq!(
        fx.ledger
            .reserve(&again, acct, usd(), Minor::new(21))
            .map(|a| a.value),
        Err(Refusal::BoundExceeded)
    );
}

mod aliases;
mod anchoring;
mod asset_binding;
mod custody;
mod epochs;
mod identities;
mod lifecycle;
mod opening;
mod operator;
mod parties;
mod queries;
mod resolution;
mod reversal;
mod schemes;
mod seeding;
mod settle_adapter;
mod transfers;
mod upgrade;

#[cfg(feature = "sim")]
mod settlement;
