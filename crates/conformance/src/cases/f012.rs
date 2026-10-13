// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-012` — Custody switching, credentials and recovery: its registered cases (`F-015` §6, §7).
//!
//! Every case drives `ledger-core` through [`ServicePort`] and its custody port
//! ([`CustodyPort`]) as principals: holders with credential sets, the operator's organisation
//! identity with its policy authority and staff with operator grants, a named successor. The suite
//! plays the recovery authority with `services/recovery` over a test key
//! ([`service_recovery_authority`]), and signs holders' proofs with keys of its own. Time moves
//! only by the time input: no case sleeps.
//!
//! Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::BTreeMap;

use cipher_clerk::crypto::SecretKey;
use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::audit::AuditAction;
use ledger_core::authority::{AuthorityKind, GrantKind, OperationKind, Subject};
use ledger_core::custody::{
    ChangeKind, ChangeState, Mechanism, PendingId, Signature, reclaim_challenge,
};
use ledger_core::eval::Request;
use ledger_core::id::{AccountId, CredentialId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::custody::{
    Claimed, CredentialProof, InitiateRecovery, Proof, ReclaimCustody, RegisterCredential,
};
use ledger_core::ledger::{Ctx, Founded, GrantRequest, OpenAccount};
use ledger_core::operator::OperatorOpKind;
use ledger_core::principal::{Caller, CredentialMarker, Scheme, Verifier};
use ledger_core::query::{MovementFilter, MovementsRequest, Subject as Whose};
use ledger_core::set::NonEmptySet;
use ledger_core::state::Command;
use ledger_kernel::posting::OperationRef;
use recovery::ApprovalRequest;

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::custody::CustodyPort;
use crate::harness::{
    Harness, SERVICE_RECOVERY_DELAY, SERVICE_SUCCESSION_DELAY, ServicePort, acknowledgement,
    service_recovery_authority,
};
use crate::shape::expect_refusal;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F012,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-012`'s registered cases.
pub const CASES: &[Case] = &[
    // Errors (SPEC §10).
    service_case!(
        "ERR-RecoveryNotElapsed",
        err_recovery_not_elapsed__service,
        &["REQ-AU-16"]
    ),
    // Acceptance criteria of the V0 stories.
    service_case!("AC-A5.1", ac_a5_1__service, &["REQ-AU-6", "INV-20"]),
    service_case!("AC-A5.2", ac_a5_2__service, &["REQ-AU-7", "INV-20"]),
    service_case!("AC-A5.3", ac_a5_3__service, &["REQ-AU-9"]),
    service_case!(
        "AC-A5.4",
        ac_a5_4__service,
        &["REQ-AU-20", "REQ-AU-4", "REQ-AU-3", "INV-22"]
    ),
    service_case!("AC-A7.1", ac_a7_1__service, &["REQ-AU-11", "REQ-AU-12"]),
    service_case!("AC-A7.2", ac_a7_2__service, &["REQ-AU-12"]),
    service_case!("AC-A7.3", ac_a7_3__service, &["REQ-AU-13", "REQ-OP-9"]),
    service_case!(
        "AC-A7.4",
        ac_a7_4__service,
        &["REQ-AU-13", "ERR-CredentialInvalid"]
    ),
    service_case!("AC-A7.5", ac_a7_5__service, &["REQ-AU-14"]),
    service_case!("AC-A8.1", ac_a8_1__service, &["REQ-AU-15"]),
    service_case!("AC-A8.2", ac_a8_2__service, &["REQ-AU-16", "INV-20"]),
    service_case!(
        "AC-A8.3",
        ac_a8_3__service,
        &["REQ-AU-16", "REQ-AT-1", "REQ-AU-20"]
    ),
    service_case!(
        "AC-A8.4",
        ac_a8_4__service,
        &["REQ-AU-16", "ERR-RecoveryNotElapsed"]
    ),
    service_case!("AC-A8.5", ac_a8_5__service, &["REQ-AU-17", "REQ-AU-16"]),
    service_case!("AC-A8.6", ac_a8_6__service, &["REQ-AU-18"]),
];

// ---------------------------------------------------------------------- the world

const JUSTIFY: &[u8] = b"holder verified out of band, case 7";

fn usd() -> AssetId {
    AssetId::parse("USD").expect("a valid asset id")
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).expect("non-zero")
}

/// A holder's key, as a wallet would hold it: never the book's.
fn secret(n: u8) -> SecretKey {
    let mut b = [n; 32];
    b[31] = 0;
    SecretKey::from_bytes(b).expect("a canonical scalar")
}

fn verifier_of(sk: &SecretKey) -> Verifier {
    Verifier::new(sk.public_key().0.to_vec())
}

/// The holders' signing device.
fn wallet() -> ledger_kernel::keys::RootKeys {
    ledger_kernel::keys::RootKeys::new([0x3C; 32])
}

struct World<'a> {
    s: &'a mut dyn ServicePort,
    f: Founded,
    n: u64,
    now: Time,
}

impl<'a> World<'a> {
    fn new(h: &'a mut dyn Harness) -> World<'a> {
        let now = h.clock().now() + 10;
        let s = h.service().expect("the runner checked the service port");
        let f = s.founded();
        let mut w = World { s, f, n: 0, now };
        w.issuer();
        w
    }

    fn c(&mut self) -> &mut dyn CustodyPort {
        self.s.custody()
    }

    fn op(&mut self) -> OperationId {
        self.n += 1;
        let mut b = [0xF2; 16];
        b[8..].copy_from_slice(&self.n.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        Ctx {
            caller,
            op: self.op(),
            now: self.now,
        }
    }

    fn operator(&self) -> Caller {
        Caller::new(self.f.operator, self.f.operator_credential)
    }

    /// A person, whose first credential is `sk`'s key.
    fn holder(&mut self, sk: &SecretKey) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .s
            .open_identity(&ctx, IdentityKind::Person, None, verifier_of(sk))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn agent(&mut self, of: &Caller) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .s
            .open_identity(
                &ctx,
                IdentityKind::Agent,
                Some(of.principal),
                verifier_of(&secret(0x61)),
            )
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn account(&mut self, owner: &Caller, custody: Custody) -> AccountId {
        let ctx = self.ctx(*owner);
        let acknowledged = acknowledgement(&*self.s, owner, owner.principal, custody);
        self.s
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

    fn issuer(&mut self) {
        let op = self.operator();
        let ctx = self.ctx(op);
        self.s
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Issuance,
                    principal: op.principal,
                    subject: Subject::Asset(usd()),
                    ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                        .unwrap(),
                    per_operation: None,
                    per_window: Some(ledger_core::authority::Window {
                        length: std::num::NonZeroU64::new(1_000_000).unwrap(),
                        limit: nz(1_000_000_000),
                    }),
                    assets: Some(NonEmptySet::one(usd())),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
    }

    fn issue(&mut self, to: AccountId, amount: u64) {
        let ctx = self.ctx(self.operator());
        self.s.issue(&ctx, usd(), to, m(amount)).unwrap();
    }

    /// A holder with `sk` as its first credential and one funded account in `custody`.
    fn funded(&mut self, sk: &SecretKey, custody: Custody, amount: u64) -> (Caller, AccountId) {
        let p = self.holder(sk);
        let a = self.account(&p, custody);
        if amount > 0 {
            self.issue(a, amount);
        }
        (p, a)
    }

    /// A member of the operator's staff with operator authority over every delegated-custody
    /// account for every value-moving kind, and operator-surface grants for `kinds`.
    fn staff(&mut self, kinds: &[OperatorOpKind]) -> Caller {
        let who = self.holder(&secret(0x71));
        let ctx = self.ctx(self.operator());
        self.s
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Operator,
                    principal: who.principal,
                    subject: Subject::DelegatedAccounts,
                    ops: NonEmptySet::new(
                        OperationKind::ALL
                            .into_iter()
                            // Issuance is over an asset; a reversal's authority is the operator
                            // surface's, never a grant (amendment 0006 item 1); opening an account
                            // is granted over an identity (amendment 0025).
                            .filter(|k| {
                                !matches!(
                                    k,
                                    OperationKind::Issue
                                        | OperationKind::Reversal
                                        | OperationKind::OpenAccount
                                )
                            })
                            .collect(),
                    )
                    .unwrap(),
                    per_operation: Some(nz(1_000_000)),
                    per_window: None,
                    assets: None,
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
        if !kinds.is_empty() {
            let ctx = self.ctx(self.operator());
            self.s
                .operator()
                .grant_operator(
                    &ctx,
                    who.principal,
                    NonEmptySet::new(kinds.iter().copied().collect()).unwrap(),
                    None,
                    b"on-call rota",
                )
                .unwrap();
        }
        who
    }

    /// A delegated grant from `owner` to `to` over `account`.
    fn delegate(
        &mut self,
        owner: &Caller,
        to: &Caller,
        account: AccountId,
    ) -> ledger_core::id::GrantId {
        let ctx = self.ctx(*owner);
        self.s
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Delegated,
                    principal: to.principal,
                    subject: Subject::Account(account),
                    ops: NonEmptySet::new([OperationKind::Transfer, OperationKind::Reserve].into())
                        .unwrap(),
                    per_operation: Some(nz(50)),
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

    fn reserve(&mut self, by: Caller, account: AccountId, amount: u64) -> Result<Ctx, Refusal> {
        let ctx = self.ctx(by);
        self.s.reserve(&ctx, account, usd(), m(amount)).map(|_| ctx)
    }

    fn available(&self, account: AccountId) -> i128 {
        self.s.balance_view(&account, &usd()).unwrap().available
    }

    /// Reclaim `account` for `owner`, proving `credential` with `sk`, acknowledging the mechanisms.
    fn reclaim(
        &mut self,
        owner: Caller,
        account: AccountId,
        sk: &SecretKey,
        credential: CredentialId,
    ) -> Result<(), Refusal> {
        let epoch = self.s.state().custody().epoch(&account);
        let signature = wallet().sign(sk, &reclaim_challenge(&account, epoch));
        self.reclaim_proven(
            owner,
            account,
            CredentialProof {
                credential,
                proof: Proof::Signature(signature),
            },
        )
    }

    /// Reclaim `account` for `owner` with `proof`, acknowledging the mechanisms.
    fn reclaim_proven(
        &mut self,
        owner: Caller,
        account: AccountId,
        proof: CredentialProof,
    ) -> Result<(), Refusal> {
        let ctx = self.ctx(owner);
        let acknowledged = self
            .c()
            .recovery_mechanisms(&owner, &account, ctx.now)
            .ok()
            .map(|m| m.digest());
        self.c()
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

    /// The operator initiates a recovery of `account`, to issue `new`'s key.
    fn initiate(&mut self, account: AccountId, new: &SecretKey) -> PendingId {
        let ctx = self.ctx(self.operator());
        self.c()
            .initiate_recovery(
                &ctx,
                InitiateRecovery {
                    account,
                    verifier: verifier_of(new),
                    scope: None,
                },
                JUSTIFY,
            )
            .unwrap()
            .value
    }

    /// The recovery authority's approval, after its runbook.
    fn approval(pending: PendingId, account: AccountId, new: &SecretKey) -> Signature {
        service_recovery_authority().approve_recovery(&ApprovalRequest {
            pending,
            account,
            verifier: verifier_of(new),
        })
    }

    fn complete(
        &mut self,
        pending: PendingId,
        approval: Signature,
    ) -> Result<CredentialId, Refusal> {
        let ctx = self.ctx(self.operator());
        self.c()
            .complete_recovery(&ctx, pending, approval, JUSTIFY)
            .map(|a| a.value)
    }
}

// ---------------------------------------------------------------------- errors

fn err_recovery_not_elapsed__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, acct) = w.funded(&secret(1), Custody::Owned, 10);
    let new = secret(2);
    let pending = w.initiate(acct, &new);
    w.now += SERVICE_RECOVERY_DELAY - 1;
    let approval = World::approval(pending, acct, &new);
    let ctx = w.ctx(w.operator());
    expect_refusal(
        w.c().complete_recovery(&ctx, pending, approval, JUSTIFY),
        "ERR-RecoveryNotElapsed",
    )
}

// ---------------------------------------------------------------------- US-A5

/// Given owned custody, no operator authority exists over the account: the evaluator refuses
/// every operation kind to an operator holding every operator grant, and no operator-surface
/// command acts on it.
fn ac_a5_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, owned) = w.funded(&secret(1), Custody::Owned, 100);
    let (_, delegated) = w.funded(&secret(2), Custody::Delegated, 100);
    let staff = w.staff(&[OperatorOpKind::FreezeAccount]);
    for kind in OperationKind::ALL {
        if matches!(
            kind,
            OperationKind::Issue | OperationKind::Reversal | OperationKind::OpenAccount
        ) {
            // Over an asset; the operator surface's, never granted over an account; or over an
            // identity (amendment 0025).
            continue;
        }
        let req = |account| Request {
            kind,
            account,
            asset: kind.moves_value().then(usd),
            amount: kind.moves_value().then(|| nz(1)),
            counterparty: None,
        };
        let ctx = w.ctx(staff);
        if w.s.authorise(&ctx, &req(owned)).is_ok() {
            return Outcome::Fail(format!("{kind:?} authorised over an owned account"));
        }
        // The same grants do reach a delegated account: the control.
        if let Err(e) = w.s.authorise(&ctx, &req(delegated)) {
            return Outcome::Fail(format!("{kind:?} over the delegated control: {e:?}"));
        }
    }
    // It cannot move the value.
    if w.reserve(staff, owned, 1).is_ok() || w.available(owned) != 100 {
        return Outcome::Fail("the operator moved an owned account's value".into());
    }
    // Nor act on it through the operator surface.
    let ctx = w.ctx(staff);
    if w.s.operator().freeze_account(&ctx, owned, JUSTIFY).is_ok() {
        return Outcome::Fail("the operator froze an owned account".into());
    }
    Outcome::Pass
}

/// Given delegated custody, every operator action on the account is recorded as an operator
/// action and is visible to its holder: on the movements it reads and in the account's action
/// log.
fn ac_a5_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Delegated, 100);
    let staff = w.staff(&[]);
    w.reserve(staff, acct, 30).unwrap();
    let page =
        w.s.query_movements(
            &alice,
            &MovementsRequest {
                subject: Whose::Account(acct),
                filter: MovementFilter::default(),
                limit: Some(100),
                cursor: None,
            },
            w.now,
        )
        .unwrap();
    let by_operator = page.movements.iter().any(|mv| {
        mv.authorities
            .iter()
            .any(|a| a.kind == AuthorityKind::Operator && a.principal == staff.principal)
    });
    if !by_operator {
        return Outcome::Fail("the holder does not see the reservation as the operator's".into());
    }
    let actions =
        w.s.operator()
            .account_actions(&alice, &acct, w.now)
            .unwrap();
    let recorded = actions.iter().any(|r| {
        r.command == Command::Reserve
            && r.authorities[0].kind == AuthorityKind::Operator
            && r.authorities[0].principal == staff.principal
    });
    if !recorded {
        return Outcome::Fail("the action log does not name the operator".into());
    }
    Outcome::Pass
}

/// Given I choose owned custody, I am shown which recovery mechanisms apply before the choice
/// takes effect: opening an owned account, and reclaiming one, need the digest of what was
/// shown; a missing or stale one is refused.
fn ac_a5_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let sk = secret(1);
    let alice = w.holder(&sk);
    let shown = w.s.recovery_terms(&alice, &alice.principal).unwrap();
    let has_authority = shown.mechanisms.iter().any(|m| {
        matches!(m, Mechanism::RecoveryAuthority { delay, .. } if *delay == SERVICE_RECOVERY_DELAY)
    });
    let has_succession = shown.mechanisms.iter().any(
        |m| matches!(m, Mechanism::Succession { delay, .. } if *delay == SERVICE_SUCCESSION_DELAY),
    );
    if !has_authority || !has_succession {
        return Outcome::Fail(format!("the mechanisms shown: {shown:?}"));
    }
    let open = |w: &mut World, acknowledged| {
        let ctx = w.ctx(alice);
        w.s.open_account(
            &ctx,
            OpenAccount {
                owner: alice.principal,
                kind: AccountKind::Customer,
                custody: Custody::Owned,
                treasury_limits: BTreeMap::new(),
                acknowledged,
            },
        )
        .map(|a| a.value)
    };
    if open(&mut w, None) != Err(Refusal::NotAuthorised) {
        return Outcome::Fail("owned custody without the acknowledgement".into());
    }
    // A holder-held credential changes what applies: the old digest is stale.
    let ctx = w.ctx(alice);
    w.c()
        .register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::RecoveryHeld,
                scope: None,
                verifier: verifier_of(&secret(2)),
            },
        )
        .unwrap();
    if open(&mut w, Some(shown.digest())) != Err(Refusal::NotAuthorised) {
        return Outcome::Fail("owned custody with a stale acknowledgement".into());
    }
    let current = w.s.recovery_terms(&alice, &alice.principal).unwrap();
    let Ok(acct) = open(&mut w, Some(current.digest())) else {
        return Outcome::Fail("owned custody with the current acknowledgement".into());
    };
    // Reclaiming: the same rule.
    let ctx = w.ctx(alice);
    w.c().delegate_custody(&ctx, acct).unwrap();
    let ctx = w.ctx(alice);
    let epoch = w.s.state().custody().epoch(&acct);
    let signature = wallet().sign(&sk, &reclaim_challenge(&acct, epoch));
    let unacknowledged = w.c().reclaim_custody(
        &ctx,
        ReclaimCustody {
            account: acct,
            proof: CredentialProof {
                credential: alice.credential,
                proof: Proof::Signature(signature),
            },
            acknowledged: None,
        },
    );
    if unacknowledged.map(|a| a.value) != Err(Refusal::NotAuthorised) {
        return Outcome::Fail("reclaim without the acknowledgement".into());
    }
    match w.reclaim(alice, acct, &sk, alice.credential) {
        Ok(()) => Outcome::Pass,
        Err(e) => Outcome::Fail(format!("reclaim with the acknowledgement: {e:?}")),
    }
}

/// Given several credentials, any one of them acts on an owned account it is not scoped away
/// from, as owner authority; a set change without an existing credential of the set is refused
/// with `ERR-NotAuthorised`.
fn ac_a5_4__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(&secret(1), Custody::Owned, 100);
    let b = w.account(&alice, Custody::Owned);
    w.issue(b, 100);
    let ctx = w.ctx(alice);
    // The scoped member is an API key: the limit is the same whatever the scheme (`F-028`).
    let scoped = w
        .c()
        .register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::Ordinary,
                scope: Some(NonEmptySet::one(a)),
                verifier: Verifier::of(Scheme::ApiKey, vec![2; 32]),
            },
        )
        .unwrap()
        .value;
    let scoped = Caller::new(alice.principal, scoped);
    for (who, acct, ok) in [
        (alice, a, true),
        (alice, b, true),
        (scoped, a, true),
        (scoped, b, false),
    ] {
        let r = w.reserve(who, acct, 1);
        match (r, ok) {
            (Ok(ctx), true) => {
                let rec = w.s.state().record(&ctx.op).unwrap().clone();
                if rec.authorities[0].kind != AuthorityKind::Owner {
                    return Outcome::Fail("a member acted with other than owner authority".into());
                }
            }
            (Err(Refusal::UnknownAccount), false) => {}
            (r, _) => return Outcome::Fail(format!("{who:?} on {acct:?}: {:?}", r.map(|_| ()))),
        }
    }
    // Set changes without a member of the set: the operator for the holder, another holder.
    let bob = w.holder(&secret(3));
    let ctx = w.ctx(w.operator().on_behalf_of(alice.principal));
    let by_operator = w.c().register_credential(
        &ctx,
        RegisterCredential {
            marker: CredentialMarker::Ordinary,
            scope: None,
            verifier: verifier_of(&secret(4)),
        },
    );
    if by_operator.map(|x| x.value) != Err(Refusal::NotAuthorised) {
        return Outcome::Fail("the operator changed a holder's set".into());
    }
    let ctx = w.ctx(bob);
    let out = expect_refusal(
        w.c().remove_credential(&ctx, alice.credential),
        "ERR-NotAuthorised",
    );
    if out != Outcome::Pass {
        return out;
    }
    // The scoped member cannot widen itself (REQ-AU-4).
    let ctx = w.ctx(scoped);
    expect_refusal(
        w.c().register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::Ordinary,
                scope: None,
                verifier: verifier_of(&secret(5)),
            },
        ),
        "ERR-NotAuthorised",
    )
}

// ---------------------------------------------------------------------- US-A7

/// Given an owned account, delegating custody with my own credential gives the operator
/// authority from then on, and the change is recorded as mine.
fn ac_a7_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Owned, 100);
    let staff = w.staff(&[]);
    if w.reserve(staff, acct, 1).is_ok() {
        return Outcome::Fail("operator authority before delegation".into());
    }
    let ctx = w.ctx(alice);
    let done = w.c().delegate_custody(&ctx, acct).unwrap();
    let used = done.record.authorities[0];
    if used.kind != AuthorityKind::Owner || used.principal != alice.principal {
        return Outcome::Fail(format!("the change was recorded as {used:?}"));
    }
    if w.s.state().account(&acct).unwrap().custody() != Custody::Delegated {
        return Outcome::Fail("custody did not change".into());
    }
    match w.reserve(staff, acct, 1) {
        Ok(ctx)
            if w.s.state().record(&ctx.op).unwrap().authorities[0].kind
                == AuthorityKind::Operator =>
        {
            Outcome::Pass
        }
        r => Outcome::Fail(format!(
            "operator authority after delegation: {:?}",
            r.map(|_| ())
        )),
    }
}

/// Given an owned account, the operator cannot delegate its custody.
fn ac_a7_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Owned, 1);
    let ctx = w.ctx(w.operator());
    let out = expect_refusal(w.c().delegate_custody(&ctx, acct), "ERR-NotAuthorised");
    if out != Outcome::Pass {
        return out;
    }
    // Nor acting for the holder.
    let ctx = w.ctx(w.operator().on_behalf_of(alice.principal));
    let out = expect_refusal(w.c().delegate_custody(&ctx, acct), "ERR-NotAuthorised");
    if w.s.state().account(&acct).unwrap().custody() != Custody::Owned {
        return Outcome::Fail("custody changed".into());
    }
    out
}

/// Given a delegated account and a proven holder credential, reclaiming leaves no operator
/// authority; an operation the operator had reserved finishes under the authority that started
/// it (its capture, after the switch); no encumbrance stood (none can in V0, `DEF-11`).
fn ac_a7_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let sk = secret(1);
    let (alice, acct) = w.funded(&sk, Custody::Delegated, 100);
    let (_, dest) = w.funded(&secret(2), Custody::Delegated, 0);
    let staff = w.staff(&[]);
    let in_flight = w.reserve(staff, acct, 40).unwrap();
    if w.s.balance_view(&acct, &usd()).unwrap().encumbered != 0 {
        return Outcome::Fail("V0 has no encumbrance".into());
    }
    if let Err(e) = w.reclaim(alice, acct, &sk, alice.credential) {
        return Outcome::Fail(format!("reclaim: {e:?}"));
    }
    if w.reserve(staff, acct, 1).is_ok() {
        return Outcome::Fail("operator authority remains after the reclaim".into());
    }
    let capture = Ctx {
        now: w.now + 1,
        ..in_flight
    };
    match w.s.capture(&capture, dest, m(40)) {
        Ok(a) if a.record.authorities[0].kind == AuthorityKind::Operator => {}
        r => return Outcome::Fail(format!("the in-flight capture: {:?}", r.map(|a| a.value))),
    }
    if w.available(acct) != 60 || w.available(dest) != 40 {
        return Outcome::Fail("the capture did not move the reserved value".into());
    }
    Outcome::Pass
}

/// Given a delegated account and no working holder credential, reclaiming is refused with
/// `ERR-CredentialInvalid`: a removed credential presented, a proof by a removed credential, or
/// a signature that is not the credential's.
fn ac_a7_4__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let sk = secret(1);
    let (alice, acct) = w.funded(&sk, Custody::Delegated, 1);
    let sk2 = secret(2);
    let ctx = w.ctx(alice);
    let second = w
        .c()
        .register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::Ordinary,
                scope: None,
                verifier: verifier_of(&sk2),
            },
        )
        .unwrap()
        .value;
    let ctx = w.ctx(alice);
    w.c().remove_credential(&ctx, alice.credential).unwrap();
    // The removed credential presented.
    let out = expect_refusal(
        w.reclaim(alice, acct, &sk, alice.credential),
        "ERR-CredentialInvalid",
    );
    if out != Outcome::Pass {
        return out;
    }
    // A working caller, proving with the removed credential.
    let bob = Caller::new(alice.principal, second);
    let out = expect_refusal(
        w.reclaim(bob, acct, &sk, alice.credential),
        "ERR-CredentialInvalid",
    );
    if out != Outcome::Pass {
        return out;
    }
    // A working credential, with a signature by another key.
    let out = expect_refusal(w.reclaim(bob, acct, &sk, second), "ERR-CredentialInvalid");
    if out != Outcome::Pass {
        return out;
    }
    // `F-028`: the boundary's passkey mark proves nothing for a key, nor for an API key, which
    // proves nothing at all here.
    let ctx = w.ctx(bob);
    let api = w
        .c()
        .register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::Ordinary,
                scope: None,
                verifier: Verifier::of(Scheme::ApiKey, vec![9; 32]),
            },
        )
        .unwrap()
        .value;
    for credential in [second, api] {
        let out = expect_refusal(
            w.reclaim_proven(
                bob,
                acct,
                CredentialProof {
                    credential,
                    proof: Proof::PasskeyChecked,
                },
            ),
            "ERR-CredentialInvalid",
        );
        if out != Outcome::Pass {
            return out;
        }
    }
    if w.s.state().account(&acct).unwrap().custody() != Custody::Delegated {
        return Outcome::Fail("custody changed".into());
    }
    // The working one, proven, succeeds.
    match w.reclaim(bob, acct, &sk2, second) {
        Ok(()) => Outcome::Pass,
        Err(e) => Outcome::Fail(format!("the proven credential: {e:?}")),
    }
}

/// Given agents I have granted authority to, custody changes do not change their grants.
fn ac_a7_5__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let sk = secret(1);
    let (alice, acct) = w.funded(&sk, Custody::Owned, 100);
    let agent = w.agent(&alice);
    let grant = w.delegate(&alice, &agent, acct);
    let before = w.s.state().grant(&grant).unwrap().clone();
    let ctx = w.ctx(alice);
    w.c().delegate_custody(&ctx, acct).unwrap();
    if w.s.state().grant(&grant) != Some(&before) || w.reserve(agent, acct, 1).is_err() {
        return Outcome::Fail("delegation changed the agent's grant".into());
    }
    w.reclaim(alice, acct, &sk, alice.credential).unwrap();
    if w.s.state().grant(&grant) != Some(&before) {
        return Outcome::Fail("the reclaim changed the agent's grant".into());
    }
    match w.reserve(agent, acct, 1) {
        Ok(ctx)
            if w.s.state().record(&ctx.op).unwrap().authorities[0].kind
                == AuthorityKind::Delegated =>
        {
            Outcome::Pass
        }
        r => Outcome::Fail(format!("the agent after the reclaim: {:?}", r.map(|_| ()))),
    }
}

// ---------------------------------------------------------------------- US-A8

/// Given a recovery started for my account, it cannot complete without the recovery authority's
/// approval: the operator's own signature is no approval, and the recovery authority is no
/// principal of the book.
fn ac_a8_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, acct) = w.funded(&secret(1), Custody::Owned, 10);
    let new = secret(2);
    let pending = w.initiate(acct, &new);
    w.now += SERVICE_RECOVERY_DELAY;
    // The operator signs with a key of its own.
    let msg = ledger_core::custody::approval_message(&pending, &acct, &verifier_of(&new));
    let forged = wallet().sign(&secret(3), &msg);
    let ctx = w.ctx(w.operator());
    let out = expect_refusal(
        w.c().complete_recovery(&ctx, pending, forged, JUSTIFY),
        "ERR-NotAuthorised",
    );
    if out != Outcome::Pass {
        return out;
    }
    // The recovery authority cannot submit anything: it holds no identity in the book.
    let ra = ledger_core::custody::RecoveryConfig::authority(&crate::harness::service_recovery())
        .unwrap();
    let as_ra = w.ctx(Caller::new(ra.principal, ra.credential));
    let approval = World::approval(pending, acct, &new);
    if w.c()
        .complete_recovery(&as_ra, pending, approval, JUSTIFY)
        .map(|a| a.value)
        != Err(Refusal::CredentialInvalid)
    {
        return Outcome::Fail("the recovery authority acted as a principal".into());
    }
    match w.complete(pending, approval) {
        Ok(_) => Outcome::Pass,
        Err(e) => Outcome::Fail(format!("the approved completion: {e:?}")),
    }
}

/// Given a pending recovery, it is visible on my account — to me and to every authority holder
/// of the account — and presenting my existing credential to cancel it before the delay ends
/// cancels it.
fn ac_a8_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Owned, 10);
    let agent = w.agent(&alice);
    w.delegate(&alice, &agent, acct);
    let new = secret(2);
    let pending = w.initiate(acct, &new);
    for who in [alice, agent] {
        let view = {
            let now = w.now;
            w.c().custody_view(&who, &acct, now)
        }
        .unwrap();
        let shown = view.changes.iter().any(|c| {
            c.id == pending
                && c.kind == ChangeKind::Recovery
                && c.state == ChangeState::Pending
                && c.effective_at == w.now + SERVICE_RECOVERY_DELAY
        });
        let told = w
            .c()
            .custody_notices(&who.principal)
            .iter()
            .any(|n| n.op == pending.0);
        if !shown || !told {
            return Outcome::Fail(format!("{who:?} does not see the pending recovery"));
        }
    }
    // Using the account does not cancel it (`REQ-AU-16`, amendment 0006 item 10)...
    w.reserve(alice, acct, 1).unwrap();
    // ...presenting the credential to cancel it does.
    w.now += SERVICE_RECOVERY_DELAY - 1;
    let ctx = w.ctx(alice);
    w.c().cancel_recovery(&ctx, pending).unwrap();
    let view = {
        let now = w.now;
        w.c().custody_view(&alice, &acct, now)
    }
    .unwrap();
    if !view
        .changes
        .iter()
        .any(|c| c.id == pending && c.state == ChangeState::Cancelled)
    {
        return Outcome::Fail("not cancelled".into());
    }
    w.now += 1;
    let approval = World::approval(pending, acct, &new);
    expect_refusal(w.complete(pending, approval), "ERR-OperationTerminal")
}

/// Given the delay has ended and the recovery authority approved, a new credential is issued to
/// me, the previous one stops working, and the audit trail names every approving principal.
fn ac_a8_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Owned, 10);
    let new = secret(2);
    let pending = w.initiate(acct, &new);
    w.now += SERVICE_RECOVERY_DELAY;
    let approval = World::approval(pending, acct, &new);
    let issued = w.complete(pending, approval).unwrap();
    if w.reserve(alice, acct, 1) != Err(Refusal::CredentialInvalid) {
        return Outcome::Fail("the previous credential still works".into());
    }
    let holder = Caller::new(alice.principal, issued);
    match w.reserve(holder, acct, 1) {
        Ok(ctx)
            if w.s.state().record(&ctx.op).unwrap().authorities[0].kind == AuthorityKind::Owner => {
        }
        r => return Outcome::Fail(format!("the new credential: {:?}", r.map(|_| ()))),
    }
    // The audit record of the completion names the initiator, the recovery authority and the
    // submitter.
    let record =
        w.s.state()
            .audit()
            .outbox()
            .find(|r| r.action == AuditAction::Command(Command::CompleteRecovery))
            .cloned();
    let Some(record) = record else {
        return Outcome::Fail("no audit record of the completion".into());
    };
    let ra = crate::harness::service_recovery().authority().unwrap();
    let kinds: Vec<_> = record
        .authorities
        .iter()
        .map(|a| (a.kind, a.principal))
        .collect();
    let want = [
        (AuthorityKind::Operator, w.f.operator),
        (AuthorityKind::Recovery, ra.principal),
        (AuthorityKind::Operator, w.f.operator),
    ];
    if kinds != want {
        return Outcome::Fail(format!("the approvers named: {kinds:?}"));
    }
    Outcome::Pass
}

/// Given a recovery attempted before the delay has ended, completion is refused with
/// `ERR-RecoveryNotElapsed`, and nothing changes.
fn ac_a8_4__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Owned, 10);
    let new = secret(2);
    let pending = w.initiate(acct, &new);
    let approval = World::approval(pending, acct, &new);
    for early in [0, SERVICE_RECOVERY_DELAY - 1] {
        let ctx = Ctx {
            now: w.now + early,
            ..w.ctx(w.operator())
        };
        let out = expect_refusal(
            w.c().complete_recovery(&ctx, pending, approval, JUSTIFY),
            "ERR-RecoveryNotElapsed",
        );
        if out != Outcome::Pass {
            return out;
        }
    }
    if w.reserve(alice, acct, 1).is_err() {
        return Outcome::Fail("an early completion changed the set".into());
    }
    Outcome::Pass
}

/// Given I named a successor, when the successor claims after the delay and I did not cancel,
/// the successor holds owner authority and I no longer do. A claim I cancel during its delay does
/// not complete.
fn ac_a8_5__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Owned, 100);
    let bob = w.holder(&secret(2));
    let ctx = w.ctx(alice);
    w.c()
        .name_successor(&ctx, acct, Some(bob.principal))
        .unwrap();
    let claim = |w: &mut World| {
        let ctx = w.ctx(bob);
        w.c().claim_succession(&ctx, acct).map(|a| a.value)
    };
    // A first claim, cancelled by the holder during its delay.
    let Ok(Claimed::Pending { pending, .. }) = claim(&mut w) else {
        return Outcome::Fail("the first claim".into());
    };
    let view = {
        let now = w.now;
        w.c().custody_view(&alice, &acct, now)
    }
    .unwrap();
    if !view
        .changes
        .iter()
        .any(|c| c.id == pending && c.kind == ChangeKind::Succession)
    {
        return Outcome::Fail("the holder does not see the claim".into());
    }
    let ctx = w.ctx(alice);
    w.c().cancel_recovery(&ctx, pending).unwrap();
    w.now += SERVICE_SUCCESSION_DELAY;
    let Ok(Claimed::Pending { pending, .. }) = claim(&mut w) else {
        return Outcome::Fail("a cancelled claim completed".into());
    };
    // Early: refused.
    w.now += SERVICE_SUCCESSION_DELAY - 1;
    let ctx = w.ctx(bob);
    let out = expect_refusal(w.c().claim_succession(&ctx, acct), "ERR-RecoveryNotElapsed");
    if out != Outcome::Pass {
        return out;
    }
    w.now += 1;
    if claim(&mut w) != Ok(Claimed::Completed { pending }) {
        return Outcome::Fail("the claim after the delay".into());
    }
    match w.reserve(bob, acct, 1) {
        Ok(ctx)
            if w.s.state().record(&ctx.op).unwrap().authorities[0].kind == AuthorityKind::Owner => {
        }
        r => return Outcome::Fail(format!("the successor: {:?}", r.map(|_| ()))),
    }
    if w.reserve(alice, acct, 1) != Err(Refusal::UnknownAccount) {
        return Outcome::Fail("the previous holder still has owner authority".into());
    }
    Outcome::Pass
}

/// Given I registered a holder-held recovery credential, presenting it acts as my owner
/// authority at once, with no delay — and it can cancel a pending recovery.
fn ac_a8_6__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Owned, 100);
    let ctx = w.ctx(alice);
    let held = w
        .c()
        .register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::RecoveryHeld,
                scope: None,
                verifier: verifier_of(&secret(2)),
            },
        )
        .unwrap()
        .value;
    let with_held = Caller::new(alice.principal, held);
    // At once: the same time input as its registration.
    match w.reserve(with_held, acct, 5) {
        Ok(ctx)
            if w.s.state().record(&ctx.op).unwrap().authorities[0].kind == AuthorityKind::Owner => {
        }
        r => return Outcome::Fail(format!("the holder-held credential: {:?}", r.map(|_| ()))),
    }
    // It is listed among the account's mechanisms.
    let shown = {
        let now = w.now;
        w.c().recovery_mechanisms(&alice, &acct, now)
    }
    .unwrap();
    if !shown
        .mechanisms
        .iter()
        .any(|m| matches!(m, Mechanism::HolderHeld { credentials } if credentials.contains(&held)))
    {
        return Outcome::Fail("not listed among the mechanisms".into());
    }
    let pending = w.initiate(acct, &secret(3));
    let ctx = w.ctx(with_held);
    match w.c().cancel_recovery(&ctx, pending) {
        Ok(_) => Outcome::Pass,
        Err(e) => Outcome::Fail(format!("cancelling with it: {e:?}")),
    }
}

// ---------------------------------------------------------------------- F-028's cases

// Amendment 0010's criteria (`US-N1`), registered by `F-028` (`super::f028`). They live here for
// the world above.

/// A passkey's public material, well formed, from `n`.
fn passkey_of(n: u8) -> Verifier {
    let mut public_key = [n; 65];
    public_key[0] = 0x04;
    Verifier::of(
        Scheme::WebAuthnEs256,
        ledger_core::scheme::PasskeyKey {
            handle: [n; 32],
            public_key,
            rp_id: "api.bloque.app".into(),
            backup_eligible: true,
            sign_count: 0,
        }
        .encode(),
    )
}

const MIGRATION: &[u8] = b"migration from the prior system, batch 1";

fn import(
    w: &mut World<'_>,
    by: Caller,
    identity: ledger_core::id::IdentityId,
    verifiers: Vec<Verifier>,
) -> Result<Vec<CredentialId>, Refusal> {
    let ctx = w.ctx(by);
    w.c()
        .import_credentials(
            &ctx,
            ledger_core::ledger::custody::ImportCredentials {
                identity,
                credentials: verifiers
                    .into_iter()
                    .map(
                        |verifier| ledger_core::ledger::custody::ImportedCredential {
                            verifier,
                            scope: None,
                        },
                    )
                    .collect(),
            },
            MIGRATION,
        )
        .map(|a| a.value)
}

/// Given a credential's public material of an accepted kind, When the operator imports it, Then it
/// is a member of the set, marked imported, audited as the operator's and shown to the holder.
pub(super) fn ac_n1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Delegated, 0);
    let ops = w.staff(&[OperatorOpKind::ImportCredentials]);
    let ids = match import(
        &mut w,
        ops,
        alice.principal,
        vec![passkey_of(2), passkey_of(3)],
    ) {
        Ok(ids) => ids,
        Err(e) => return Outcome::Fail(format!("the import: {e:?}")),
    };
    for id in &ids {
        let Some(c) = w.s.state().credential(id) else {
            return Outcome::Fail("an imported credential is missing".into());
        };
        if c.identity() != alice.principal
            || c.marker() != CredentialMarker::Imported
            || c.status() != ledger_core::principal::CredentialStatus::Active
        {
            return Outcome::Fail("not an active, imported member of alice's set".into());
        }
    }
    if w.c().custody_notices(&alice.principal).is_empty() {
        return Outcome::Fail("the holder was not shown the import (INV-20)".into());
    }
    // The imported passkey is a member: it reads the account as its owner.
    let as_passkey = Caller::new(alice.principal, ids[0]);
    let now = w.now;
    match w.c().recovery_mechanisms(&as_passkey, &acct, now) {
        Ok(_) => Outcome::Pass,
        Err(e) => Outcome::Fail(format!("an imported member cannot act: {e:?}")),
    }
}

/// Given a credential of a kind not accepted, or one already in the set, Then the import is
/// refused with `ERR-CredentialInvalid` and nothing changes.
pub(super) fn ac_n1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, _) = w.funded(&secret(1), Custody::Delegated, 0);
    let ops = w.staff(&[OperatorOpKind::ImportCredentials]);
    if import(&mut w, ops, alice.principal, vec![passkey_of(2)]).is_err() {
        return Outcome::Fail("the first import".into());
    }
    let before = w.s.state().credentials_of(&alice.principal).count();
    for set in [
        vec![passkey_of(2)],
        vec![Verifier::of(Scheme::WebAuthnEs256, vec![1; 40])],
        vec![Verifier::of(Scheme::ApiKey, vec![1; 31])],
    ] {
        let out = expect_refusal(
            import(&mut w, ops, alice.principal, set),
            "ERR-CredentialInvalid",
        );
        if out != Outcome::Pass {
            return out;
        }
    }
    if w.s.state().credentials_of(&alice.principal).count() != before {
        return Outcome::Fail("a refused import changed the set".into());
    }
    Outcome::Pass
}

/// Given an identity with an account in owned custody, Then the import is refused with
/// `ERR-NotAuthorised`.
pub(super) fn ac_n1_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, _) = w.funded(&secret(1), Custody::Owned, 0);
    let ops = w.staff(&[OperatorOpKind::ImportCredentials]);
    expect_refusal(
        import(&mut w, ops, alice.principal, vec![passkey_of(2)]),
        "ERR-NotAuthorised",
    )
}

/// Given an imported credential, When the holder reclaims custody with it, Then it must prove it
/// works: another key's signature is refused, its own succeeds.
pub(super) fn ac_n1_4__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(&secret(1), Custody::Delegated, 0);
    let ops = w.staff(&[OperatorOpKind::ImportCredentials]);
    let sk = secret(9);
    let ids = match import(&mut w, ops, alice.principal, vec![verifier_of(&sk)]) {
        Ok(ids) => ids,
        Err(e) => return Outcome::Fail(format!("the import: {e:?}")),
    };
    let out = expect_refusal(
        w.reclaim(alice, acct, &secret(10), ids[0]),
        "ERR-CredentialInvalid",
    );
    if out != Outcome::Pass {
        return out;
    }
    match w.reclaim(alice, acct, &sk, ids[0]) {
        Ok(()) => Outcome::Pass,
        Err(e) => Outcome::Fail(format!("the proven imported credential: {e:?}")),
    }
}
