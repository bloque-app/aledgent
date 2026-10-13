// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-003` — Identities, accounts, assets, authority: its registered cases (`F-015` §6, §7).
//!
//! - **Service** cases drive `ledger-core`'s [`Ledger`](ledger_core::Ledger) through
//!   [`ServicePort`] as principals: the operator (from genesis), people, organisations and
//!   agents it opens.
//! - **Store** cases write straight to an empty service store through
//!   [`Harness::service_store`], bypassing every command, and show the store itself refusing
//!   (`INV-7`, `INV-17`, `INV-24`).
//!
//! Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].
//!
//! Still pending here: `INV-22` at the credential layer, SPEC §9's target (signed attenuation, a
//! later hardening; `REQ-AR-6` declares service for V0). `AC-A1.3` and `AC-A2.2` are `F-007`'s and
//! `F-008`'s, `AC-G3.2` is `F-004`'s, and `INV-16` at the store layer is `F-011`'s: each needs what
//! that feature builds (`NOTES.md`).

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use ledger_core::Refusal;
use ledger_core::account::{AccountKind, AccountStatus, Custody, LedgerAccount, Time};
use ledger_core::asset::{AssetId, AssetKind, MAX_PRECISION, Minor, NonZeroMinor, RegistryError};
use ledger_core::authority::{
    AuthorityKind, Bounds, Charge, Grant, GrantKind, GrantTerms, OperationKind, Subject, Window,
    WindowKey,
};
use ledger_core::eval::Request;
use ledger_core::id::{
    self, AccountId, GrantId, IdKind, IdentityId, Mint, OperationId, account_reference,
};
use ledger_core::identity::{Identity, IdentityKind, IdentityStatus};
use ledger_core::ledger::{Ctx, Founded, GrantRequest, OpenAccount};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::state::{State, StoreRefusal, Write};
use ledger_kernel::posting::OperationRef;

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::{Harness, ServicePort, acknowledgement};
use crate::shape::expect_refusal;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F003,
            Port::Service,
            $traces,
            $name,
        )
    };
}

macro_rules! store_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Store,
            Feature::F003,
            Port::ServiceStore,
            $traces,
            $name,
        )
    };
}

/// `F-003`'s registered cases.
pub const CASES: &[Case] = &[
    // Invariants (SPEC §9), at the layer each case exercises.
    store_case!(
        "INV-7",
        inv_7_excess_precision_is_refused_never_rounded__store,
        &["REQ-AS-4"]
    ),
    service_case!(
        "INV-9",
        inv_9_a_new_account_is_credited_in_an_asset_it_never_held__service,
        &["REQ-LA-1"]
    ),
    // REQ-AR-6 declares INV-16 at service for V0. The store case, §9's target, is F-011's.
    service_case!(
        "INV-16",
        inv_16_reach_is_unknown_account_without_live_authority__service,
        &["ERR-UnknownAccount", "REQ-PV-2", "REQ-AR-6"]
    ),
    store_case!(
        "INV-17",
        inv_17_identifiers_follow_the_grammar_and_carry_no_personal_data__store,
        &["REQ-PV-3"]
    ),
    service_case!(
        "INV-20",
        inv_20_the_causing_authority_is_recorded_with_every_entry__service,
        &["REQ-PN-1"]
    ),
    // SPEC §9's target layer. Its V0 column says service (amendment 0006 item 2): this case is
    // the target, reported but not required. Signed attenuation is
    // a later hardening (F-003 Open points).
    Case::pending(
        "INV-22",
        "inv_22_a_widened_or_forged_credential_fails_verification__credential",
        Layer::Credential,
        Feature::F003,
        &["REQ-AU-3"],
    ),
    service_case!(
        "INV-22",
        inv_22_no_api_widens_a_grant_and_effective_scope_is_the_meet__service,
        &["REQ-AU-3"]
    ),
    store_case!(
        "INV-24",
        inv_24_a_second_registry_or_a_precision_outside_it_cannot_be_formed__store,
        &["REQ-AS-1"]
    ),
    // Errors (SPEC §10).
    service_case!("ERR-NotAuthorised", err_not_authorised__service, &[]),
    service_case!("ERR-AuthorityRevoked", err_authority_revoked__service, &[]),
    service_case!(
        "ERR-CredentialInvalid",
        err_credential_invalid__service,
        &[]
    ),
    service_case!("ERR-UnboundedGrant", err_unbounded_grant__service, &[]),
    service_case!("ERR-BoundExceeded", err_bound_exceeded__service, &[]),
    service_case!("ERR-UnknownAccount", err_unknown_account__service, &[]),
    service_case!("ERR-AccountClosed", err_account_closed__service, &[]),
    service_case!("ERR-AccountNotEmpty", err_account_not_empty__service, &[]),
    service_case!(
        "ERR-IdentityRestricted",
        err_identity_restricted__service,
        &[]
    ),
    service_case!("ERR-UnknownAsset", err_unknown_asset__service, &[]),
    service_case!("ERR-AssetDeprecated", err_asset_deprecated__service, &[]),
    service_case!(
        "ERR-PrecisionExceeded",
        err_precision_exceeded__service,
        &[]
    ),
    service_case!("ERR-ZeroAmount", err_zero_amount__service, &[]),
    // Acceptance criteria of the V0 stories.
    service_case!("AC-A1.1", ac_a1_1__service, &["REQ-LA-1", "INV-9"]),
    service_case!("AC-A1.2", ac_a1_2__service, &["REQ-SN-1"]),
    service_case!("AC-A1.4", ac_a1_4__service, &["INV-9"]),
    service_case!("AC-A2.1", ac_a2_1__service, &["REQ-LA-1", "REQ-ID-6"]),
    service_case!("AC-A4.1", ac_a4_1__service, &["REQ-LA-6", "REQ-AU-2"]),
    service_case!("AC-A4.2", ac_a4_2__service, &["REQ-LA-6"]),
    service_case!("AC-A6.1", ac_a6_1__service, &["REQ-LA-4"]),
    service_case!("AC-A6.2", ac_a6_2__service, &["INV-2"]),
    service_case!("AC-F1.1", ac_f1_1__service, &["INV-4"]),
    service_case!("AC-F1.2", ac_f1_2__service, &["REQ-AS-1"]),
    service_case!("AC-F3.1", ac_f3_1__service, &["REQ-AS-4", "INV-7"]),
    service_case!("AC-F4.1", ac_f4_1__service, &["REQ-AS-6", "INV-5"]),
    service_case!("AC-F4.2", ac_f4_2__service, &["REQ-AU-10"]),
    service_case!("AC-F4.3", ac_f4_3__service, &["REQ-AS-6"]),
    service_case!("AC-G1.1", ac_g1_1__service, &["REQ-AU-2", "REQ-AG-1"]),
    service_case!(
        "AC-G1.2",
        ac_g1_2__service,
        &["REQ-PN-1", "REQ-AU-7", "REQ-AT-1"]
    ),
    service_case!("AC-G1.3", ac_g1_3__service, &["REQ-AG-2"]),
    service_case!("AC-G2.1", ac_g2_1__service, &["REQ-AG-1"]),
    service_case!("AC-G2.2", ac_g2_2__service, &["REQ-AU-4"]),
    service_case!("AC-G3.1", ac_g3_1__service, &["REQ-AU-3"]),
    service_case!("AC-I4.1", ac_i4_1__service, &["REQ-PV-3", "INV-17"]),
    service_case!("AC-I4.2", ac_i4_2__service, &["REQ-ID-1"]),
];

// ---------------------------------------------------------------------------
// A world of principals over the service port.

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn eur() -> AssetId {
    AssetId::parse("EUR").unwrap()
}

fn btc() -> AssetId {
    AssetId::parse("BTC").unwrap()
}

fn old() -> AssetId {
    AssetId::parse("OLD").unwrap()
}

fn xyz() -> AssetId {
    AssetId::parse("XYZ").unwrap()
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn ops(kinds: &[OperationKind]) -> NonEmptySet<OperationKind> {
    NonEmptySet::new(kinds.iter().copied().collect()).unwrap()
}

/// A grant request with nothing but its kind, holder, subject and operations: add bounds.
fn request(
    kind: GrantKind,
    principal: IdentityId,
    subject: Subject,
    kinds: &[OperationKind],
) -> GrantRequest {
    GrantRequest {
        kind,
        principal,
        subject,
        ops: ops(kinds),
        per_operation: None,
        per_window: None,
        assets: None,
        counterparties: None,
        parent: None,
        expires: None,
    }
}

struct World<'a> {
    s: &'a mut dyn ServicePort,
    f: Founded,
    n: u64,
    now: Time,
}

impl<'a> World<'a> {
    fn new(h: &'a mut dyn Harness) -> World<'a> {
        let now = h.clock().now();
        let s = h.service().expect("the runner checked the service port");
        let f = s.founded();
        World { s, f, n: 0, now }
    }

    fn op(&mut self) -> OperationId {
        self.n += 1;
        let mut b = [0xF3; 16];
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

    fn identity(&mut self, kind: IdentityKind, acts_for: Option<IdentityId>) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .s
            .open_identity(&ctx, kind, acts_for, Verifier::new(vec![1]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn person(&mut self) -> Caller {
        self.identity(IdentityKind::Person, None)
    }

    fn agent(&mut self, of: &Caller) -> Caller {
        self.identity(IdentityKind::Agent, Some(of.principal))
    }

    /// A Customer account the owner opens for itself.
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

    /// An issuance grant from the operator to `to` over `asset`, per window and per asset.
    fn issuance(&mut self, to: IdentityId, asset: AssetId, kinds: &[OperationKind]) -> GrantId {
        let mut r = request(GrantKind::Issuance, to, Subject::Asset(asset), kinds);
        r.per_window = Some(Window {
            length: NonZeroU64::new(1_000_000).unwrap(),
            limit: nz(1_000_000_000),
        });
        r.assets = Some(NonEmptySet::one(asset));
        let ctx = self.ctx(self.operator());
        self.s.grant_authority(&ctx, r).unwrap().value
    }

    /// The operator, able to issue and redeem USD, EUR and BTC.
    fn issuer(&mut self) -> Caller {
        let op = self.operator();
        for a in [usd(), eur(), btc()] {
            self.issuance(
                op.principal,
                a,
                &[OperationKind::Issue, OperationKind::Redeem],
            );
        }
        op
    }

    /// Operator authority to redeem from every delegated-custody account.
    fn operator_redeems(&mut self) {
        let op = self.operator();
        let mut r = request(
            GrantKind::Operator,
            op.principal,
            Subject::DelegatedAccounts,
            &[OperationKind::Redeem],
        );
        r.per_operation = Some(nz(1_000_000_000));
        let ctx = self.ctx(op);
        self.s.grant_authority(&ctx, r).unwrap();
    }

    /// A delegated grant from `owner` to `to` over `account`, bounded per operation.
    fn delegate(
        &mut self,
        owner: &Caller,
        to: IdentityId,
        account: AccountId,
        kinds: &[OperationKind],
        per_op: u64,
    ) -> GrantId {
        let mut r = request(GrantKind::Delegated, to, Subject::Account(account), kinds);
        r.per_operation = Some(nz(per_op));
        let ctx = self.ctx(*owner);
        self.s.grant_authority(&ctx, r).unwrap().value
    }

    fn issue(&mut self, issuer: Caller, asset: AssetId, to: AccountId, amount: u64) -> u64 {
        let ctx = self.ctx(issuer);
        self.s.issue(&ctx, asset, to, m(amount)).unwrap().value
    }

    fn available(&self, caller: &Caller, account: AccountId, asset: AssetId) -> i128 {
        self.s
            .available(caller, &account, &asset, self.now)
            .unwrap()
    }

    fn transfer(&self, account: AccountId, amount: u64) -> Request {
        Request {
            kind: OperationKind::Transfer,
            account,
            asset: Some(usd()),
            amount: Some(nz(amount)),
            counterparty: Some(self.f.issuance),
        }
    }

    fn authorise(&mut self, caller: Caller, r: &Request) -> Result<AuthorityKind, Refusal> {
        let ctx = self.ctx(caller);
        self.s.authorise(&ctx, r).map(|d| d.used.kind)
    }

    fn sub_account(&self, account: AccountId, asset: AssetId) -> cipher_clerk::AccountId {
        let st = self.s.state();
        st.account(&account)
            .unwrap()
            .sub_account(st.registry().get(&asset).unwrap())
            .id()
    }
}

fn store(h: &mut dyn Harness) -> &mut State {
    h.service_store()
        .expect("the runner checked the service-store port")
}

fn raw_op(bytes: [u8; 16]) -> OperationId {
    OperationId::from_ref(OperationRef(bytes))
}

// ---------------------------------------------------------------------------
// Store cases: raw writes to the service store, every command bypassed.

fn inv_7_excess_precision_is_refused_never_rounded__store(h: &mut dyn Harness) -> Outcome {
    let s = store(h);
    s.commit(&[Write::RegisterAsset {
        id: usd(),
        precision: 2,
        kind: AssetKind::Fiat,
    }])
    .unwrap();
    let before = s.registry().digest();
    // A precision minor units in u64 cannot carry is refused by the store.
    assert_eq!(
        s.commit(&[Write::RegisterAsset {
            id: AssetId::parse("XX").unwrap(),
            precision: MAX_PRECISION + 1,
            kind: AssetKind::Other,
        }]),
        Err(StoreRefusal::Registry(RegistryError::PrecisionOutOfRange(
            MAX_PRECISION + 1
        )))
    );
    assert_eq!(s.registry().digest(), before);
    // The precision the store holds is the one amounts are exact against: excess is refused.
    let asset = *s.registry().get(&usd()).unwrap();
    assert_eq!(
        Minor::from_decimal("1.005", &asset),
        Err(Refusal::PrecisionExceeded)
    );
    assert_eq!(Minor::from_decimal("1.00", &asset), Ok(m(100)));
    // A stored counter is exact: past u64::MAX is refused, never wrapped.
    let who = Identity::new(Mint::new(raw_op([1; 16]), 0), IdentityKind::Person, None).unwrap();
    let acct = LedgerAccount::new(
        Mint::new(raw_op([1; 16]), 1),
        who.id(),
        AccountKind::Customer,
        Custody::Owned,
        0,
        BTreeMap::new(),
    )
    .unwrap();
    let limit = nz(u64::MAX);
    let grant = Grant::new(
        Mint::new(raw_op([1; 16]), 2),
        who.id(),
        GrantTerms {
            kind: GrantKind::Delegated,
            principal: who.id(),
            subject: Subject::Account(acct.id()),
            ops: ops(&[OperationKind::Transfer]),
            bounds: Bounds::new(
                None,
                Some(Window {
                    length: NonZeroU64::new(10).unwrap(),
                    limit,
                }),
                None,
                None,
            )
            .unwrap(),
            parent: None,
            expires: None,
        },
        0,
    );
    let key = WindowKey {
        grant: grant.id(),
        window: 0,
        asset: usd(),
    };
    s.commit(&[
        Write::PutIdentity(who),
        Write::PutAccount(acct),
        Write::PutGrant(grant),
        Write::Charge(Charge {
            key,
            amount: m(u64::MAX),
            limit,
        }),
    ])
    .unwrap();
    assert_eq!(
        s.commit(&[Write::Charge(Charge {
            key,
            amount: m(1),
            limit,
        })]),
        Err(StoreRefusal::BoundExceeded)
    );
    assert_eq!(s.used(&key), m(u64::MAX));
    Outcome::Pass
}

fn inv_17_identifiers_follow_the_grammar_and_carry_no_personal_data__store(
    h: &mut dyn Harness,
) -> Outcome {
    let s = store(h);
    // An operation whose bytes happen to spell personal data: the identifiers it mints are hashes.
    let op = raw_op(*b"alice@example.co");
    let who = Identity::new(Mint::new(op, 0), IdentityKind::Person, None).unwrap();
    let acct = LedgerAccount::new(
        Mint::new(op, 1),
        who.id(),
        AccountKind::Customer,
        Custody::Owned,
        0,
        BTreeMap::new(),
    )
    .unwrap();
    let grant = Grant::new(
        Mint::new(op, 2),
        who.id(),
        GrantTerms {
            kind: GrantKind::Delegated,
            principal: who.id(),
            subject: Subject::Account(acct.id()),
            ops: ops(&[OperationKind::Transfer]),
            bounds: Bounds::new(Some(nz(1)), None, None, None).unwrap(),
            parent: None,
            expires: None,
        },
        0,
    );
    // Raw writes of records whose identifiers are not their minting are refused, and change
    // nothing.
    let chosen = IdentityId::parse("did:aledgent:id:00000000000000000000000ACE").unwrap();
    for w in [
        Write::PutIdentity(who.forged(chosen)),
        Write::PutAccount(acct.clone().forged(AccountId::mint(&raw_op([9; 16]), 0))),
    ] {
        assert_eq!(s.commit(&[w]), Err(StoreRefusal::NotMinted));
    }
    assert!(s.identity(&chosen).is_none());
    s.commit(&[
        Write::PutIdentity(who),
        Write::PutAccount(acct.clone()),
        Write::PutGrant(grant.clone()),
    ])
    .unwrap();
    assert_eq!(
        s.commit(&[Write::PutGrant(
            grant.clone().forged(GrantId::mint(&raw_op([9; 16]), 0))
        )]),
        Err(StoreRefusal::NotMinted)
    );
    // Every stored identifier is its minting and has the grammar, with nothing of the input in it.
    let stored = [
        (who.id().to_did(), IdKind::Identity),
        (acct.id().to_did(), IdKind::Account),
        (grant.id().to_did(), IdKind::Authority),
    ];
    for (did, kind) in stored {
        assert_eq!(id::validate(&did), Ok(kind), "{did}");
        assert!(!did.to_lowercase().contains("alice"), "{did}");
    }
    assert_eq!(
        s.identity(&who.id()).unwrap().id(),
        IdentityId::mint(&op, 0)
    );
    assert_eq!(s.account(&acct.id()).unwrap().id(), AccountId::mint(&op, 1));
    Outcome::Pass
}

fn inv_24_a_second_registry_or_a_precision_outside_it_cannot_be_formed__store(
    h: &mut dyn Harness,
) -> Outcome {
    let s = store(h);
    s.commit(&[Write::RegisterAsset {
        id: usd(),
        precision: 2,
        kind: AssetKind::Fiat,
    }])
    .unwrap();
    let tag = s.registry().get(&usd()).unwrap().tag();
    // A second spelling, or a second precision, of the same asset cannot be registered.
    for (spelling, precision) in [("usd", 6), ("Usd", 2), ("USD", 2)] {
        assert_eq!(
            s.commit(&[Write::RegisterAsset {
                id: AssetId::parse(spelling).unwrap(),
                precision,
                kind: AssetKind::Stable,
            }]),
            Err(StoreRefusal::Registry(RegistryError::AlreadyRegistered(
                usd()
            )))
        );
    }
    // A batch that would add a second registry's worth of assets is all or nothing.
    assert_eq!(
        s.commit(&[
            Write::RegisterAsset {
                id: eur(),
                precision: 2,
                kind: AssetKind::Fiat,
            },
            Write::RegisterAsset {
                id: usd(),
                precision: 3,
                kind: AssetKind::Fiat,
            },
        ]),
        Err(StoreRefusal::Registry(RegistryError::AlreadyRegistered(
            usd()
        )))
    );
    assert!(s.registry().get(&eur()).is_err());
    // Deprecation changes the status and nothing else.
    s.commit(&[Write::DeprecateAsset(usd())]).unwrap();
    let asset = s.registry().get(&usd()).unwrap();
    assert_eq!((asset.precision(), asset.tag()), (2, tag));
    assert_eq!(s.registry().len(), 1);
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// Invariant cases at the service layer.

fn inv_9_a_new_account_is_credited_in_an_asset_it_never_held__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    for (asset, amount) in [(usd(), 100), (btc(), 1)] {
        let sub = w.sub_account(acct, asset);
        assert!(w.s.kernel_account(&sub).is_none());
        w.issue(issuer, asset, acct, amount);
        assert!(w.s.kernel_account(&sub).is_some());
        assert_eq!(w.available(&alice, acct, asset), i128::from(amount));
    }
    Outcome::Pass
}

fn inv_16_reach_is_unknown_account_without_live_authority__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let carol = w.person();
    let a1 = w.account(&alice, Custody::Owned);
    let a2 = w.account(&alice, Custody::Owned);
    let missing = AccountId::mint(&raw_op([0xAB; 16]), 0);
    let g = w.delegate(&alice, bob.principal, a1, &[OperationKind::Transfer], 10);
    let now = w.now;
    // No authority: the same answer as a missing account.
    for (who, acct) in [(carol, a1), (carol, missing), (bob, a2), (bob, missing)] {
        assert_eq!(
            w.s.reach_account(&who, &acct, now),
            Err(Refusal::UnknownAccount)
        );
        assert_eq!(
            w.s.available(&who, &acct, &usd(), now),
            Err(Refusal::UnknownAccount)
        );
    }
    assert_eq!(
        w.s.reach_identity(&carol, &alice.principal, now),
        Err(Refusal::UnknownAccount)
    );
    // A grant reaches its subject only; the owner reaches everything it owns.
    assert_eq!(
        w.s.reach_account(&bob, &a1, now),
        Ok(AuthorityKind::Delegated)
    );
    assert_eq!(
        w.s.reach_identity(&bob, &alice.principal, now),
        Ok(BTreeSet::from([a1]))
    );
    assert_eq!(
        w.s.reach_identity(&alice, &alice.principal, now),
        Ok(BTreeSet::from([a1, a2]))
    );
    // A revoked grant reaches nothing, at once.
    let ctx = w.ctx(alice);
    w.s.revoke_authority(&ctx, g).unwrap();
    assert_eq!(
        w.s.reach_account(&bob, &a1, now),
        Err(Refusal::UnknownAccount)
    );
    Outcome::Pass
}

fn inv_20_the_causing_authority_is_recorded_with_every_entry__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let op = w.operator();
    let issuer = w.issuer();
    w.operator_redeems();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Delegated);
    let ctx = w.ctx(issuer);
    let issued = w.s.issue(&ctx, usd(), acct, m(100)).unwrap();
    let ctx = w.ctx(issuer);
    let redeemed = w.s.redeem(&ctx, usd(), acct, m(100)).unwrap();
    let ctx = w.ctx(alice);
    let closed = w.s.close_account(&ctx, acct).unwrap();
    for (record, principal) in [
        (&issued.record, op.principal),
        (&redeemed.record, op.principal),
        (&closed.record, alice.principal),
    ] {
        assert_eq!(w.s.state().record(&record.op), Some(record));
        assert!(!record.authorities.is_empty());
        assert!(record.authorities.iter().all(|a| a.principal == principal));
    }
    // Every entry against the account is traceable to the authority that caused it.
    assert_eq!(issued.record.postings, vec![issued.value]);
    assert_eq!(issued.record.authorities[0].kind, AuthorityKind::Issuance);
    assert!(issued.record.accounts.contains(&acct));
    assert_eq!(redeemed.record.postings, vec![redeemed.value]);
    assert_eq!(
        redeemed
            .record
            .authorities
            .iter()
            .map(|a| a.kind)
            .collect::<Vec<_>>(),
        vec![AuthorityKind::Operator, AuthorityKind::Issuance]
    );
    assert_eq!(closed.record.authorities[0].kind, AuthorityKind::Owner);
    Outcome::Pass
}

fn inv_22_no_api_widens_a_grant_and_effective_scope_is_the_meet__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let root = w.delegate(&alice, bob.principal, acct, &[OperationKind::Transfer], 100);
    // Bob narrows "to himself" with a child that declares more than the root.
    let mut wider = request(
        GrantKind::Delegated,
        bob.principal,
        Subject::Account(acct),
        &[
            OperationKind::Transfer,
            OperationKind::Redeem,
            OperationKind::CloseAccount,
        ],
    );
    wider.per_operation = Some(nz(1_000_000));
    wider.parent = Some(root);
    let ctx = w.ctx(bob);
    w.s.grant_authority(&ctx, wider).unwrap();
    // The effective authority is the meet of the chain: no kind and no amount beyond the root.
    let close = Request {
        kind: OperationKind::CloseAccount,
        account: acct,
        asset: None,
        amount: None,
        counterparty: None,
    };
    assert_eq!(w.authorise(bob, &close), Err(Refusal::NotAuthorised));
    let over = w.transfer(acct, 101);
    assert_eq!(w.authorise(bob, &over), Err(Refusal::BoundExceeded));
    let within = w.transfer(acct, 100);
    assert_eq!(w.authorise(bob, &within), Ok(AuthorityKind::Delegated));
    // A child cannot change kind or subject.
    let alice_other = w.account(&alice, Custody::Owned);
    let mut elsewhere = request(
        GrantKind::Delegated,
        bob.principal,
        Subject::Account(alice_other),
        &[OperationKind::Transfer],
    );
    elsewhere.per_operation = Some(nz(1));
    elsewhere.parent = Some(root);
    let ctx = w.ctx(bob);
    assert_eq!(
        w.s.grant_authority(&ctx, elsewhere).map(|a| a.value),
        Err(Refusal::UnknownAccount)
    );
    let mut as_operator = request(
        GrantKind::Operator,
        bob.principal,
        Subject::Account(acct),
        &[OperationKind::Transfer],
    );
    as_operator.per_operation = Some(nz(1));
    as_operator.parent = Some(root);
    let ctx = w.ctx(bob);
    assert_eq!(
        w.s.grant_authority(&ctx, as_operator).map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// Error cases.

fn err_not_authorised__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    // Alice holds no issuance authority.
    let ctx = w.ctx(alice);
    expect_refusal(w.s.issue(&ctx, usd(), acct, m(1)), "ERR-NotAuthorised")
}

fn err_authority_revoked__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let g = w.delegate(&alice, bob.principal, acct, &[OperationKind::Transfer], 10);
    let ctx = w.ctx(alice);
    w.s.revoke_authority(&ctx, g).unwrap();
    let ctx = w.ctx(bob);
    expect_refusal(
        w.s.authorise(&ctx, &w.transfer(acct, 1)),
        "ERR-AuthorityRevoked",
    )
}

fn err_credential_invalid__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let ctx = w.ctx(Caller::new(alice.principal, bob.credential));
    let acknowledged = acknowledgement(&*w.s, &alice, alice.principal, Custody::Owned);
    expect_refusal(
        w.s.open_account(
            &ctx,
            OpenAccount {
                owner: alice.principal,
                kind: AccountKind::Customer,
                custody: Custody::Owned,
                treasury_limits: BTreeMap::new(),
                acknowledged,
            },
        ),
        "ERR-CredentialInvalid",
    )
}

fn err_unbounded_grant__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(alice);
    expect_refusal(
        w.s.grant_authority(
            &ctx,
            request(
                GrantKind::Delegated,
                bob.principal,
                Subject::Account(acct),
                &[OperationKind::Transfer],
            ),
        ),
        "ERR-UnboundedGrant",
    )
}

fn err_bound_exceeded__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let acct = w.account(&alice, Custody::Owned);
    w.delegate(&alice, bob.principal, acct, &[OperationKind::Transfer], 100);
    let ctx = w.ctx(bob);
    expect_refusal(
        w.s.authorise(&ctx, &w.transfer(acct, 101)),
        "ERR-BoundExceeded",
    )
}

fn err_unknown_account__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let carol = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(carol);
    expect_refusal(
        w.s.authorise(&ctx, &w.transfer(acct, 1)),
        "ERR-UnknownAccount",
    )
}

fn err_account_closed__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(alice);
    w.s.close_account(&ctx, acct).unwrap();
    let ctx = w.ctx(issuer);
    expect_refusal(w.s.issue(&ctx, usd(), acct, m(1)), "ERR-AccountClosed")
}

fn err_account_not_empty__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    w.issue(issuer, btc(), acct, 1);
    let ctx = w.ctx(alice);
    expect_refusal(w.s.close_account(&ctx, acct), "ERR-AccountNotEmpty")
}

fn err_identity_restricted__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(w.operator());
    w.s.set_identity_status(&ctx, alice.principal, IdentityStatus::Restricted)
        .unwrap();
    let ctx = w.ctx(issuer);
    expect_refusal(w.s.issue(&ctx, usd(), acct, m(1)), "ERR-IdentityRestricted")
}

fn err_unknown_asset__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(issuer);
    expect_refusal(w.s.issue(&ctx, xyz(), acct, m(1)), "ERR-UnknownAsset")
}

fn err_asset_deprecated__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(issuer);
    expect_refusal(w.s.issue(&ctx, old(), acct, m(1)), "ERR-AssetDeprecated")
}

fn err_precision_exceeded__service(h: &mut dyn Harness) -> Outcome {
    let w = World::new(h);
    expect_refusal(w.s.parse_amount("USD", "1.001"), "ERR-PrecisionExceeded")
}

fn err_zero_amount__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(issuer);
    expect_refusal(w.s.issue(&ctx, usd(), acct, Minor::ZERO), "ERR-ZeroAmount")
}

// ---------------------------------------------------------------------------
// Acceptance criteria.

/// A1.1: by the time open_account returns, the account exists, is addressable and can be
/// credited.
fn ac_a1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let stored = w.s.state().account(&acct).unwrap();
    assert_eq!(stored.status(), AccountStatus::Active);
    assert_eq!(stored.owner(), alice.principal);
    assert_eq!(AccountId::parse(&acct.to_did()), Ok(acct));
    w.issue(issuer, usd(), acct, 1);
    assert_eq!(w.available(&alice, acct, usd()), 1);
    Outcome::Pass
}

/// A1.2: no response carries a settlement-layer cost, fee estimate or funding step.
fn ac_a1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let ctx = w.ctx(w.operator());
    let opened =
        w.s.open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![7]))
            .unwrap();
    let alice = Caller::new(opened.value.identity, opened.value.credential);
    let ctx = w.ctx(alice);
    let acknowledged = acknowledgement(&*w.s, &alice, alice.principal, Custody::Owned);
    let account =
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
        .unwrap();
    let ctx = w.ctx(issuer);
    let issued = w.s.issue(&ctx, usd(), account.value, m(5)).unwrap();
    // Identifier bodies are upper case, so a lower-case field name cannot be matched by chance.
    for response in [
        format!("{opened:?}"),
        format!("{account:?}"),
        format!("{issued:?}"),
    ] {
        for word in ["fee", "cost", "gas", "fund", "price", "tip"] {
            assert!(!response.contains(word), "{word} in {response}");
        }
    }
    Outcome::Pass
}

/// A1.4: an account that exists is credited when value arrives, in any asset, with no step in
/// between.
fn ac_a1_4__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Delegated);
    for (asset, amount) in [(usd(), 10), (eur(), 20), (btc(), 30)] {
        w.issue(issuer, asset, acct, amount);
        assert_eq!(w.available(&alice, acct, asset), i128::from(amount));
    }
    Outcome::Pass
}

/// A2.1: two accounts of one identity are distinct and separately addressable.
fn ac_a2_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let a1 = w.account(&alice, Custody::Owned);
    let a2 = w.account(&alice, Custody::Owned);
    assert_ne!(a1, a2);
    assert_eq!(
        w.s.reach_identity(&alice, &alice.principal, w.now),
        Ok(BTreeSet::from([a1, a2]))
    );
    w.issue(issuer, usd(), a1, 7);
    assert_eq!(w.available(&alice, a1, usd()), 7);
    assert_eq!(w.available(&alice, a2, usd()), 0);
    Outcome::Pass
}

/// A4.1: in a pooled arrangement each drawing identity holds its own explicit, bounded grant.
fn ac_a4_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let org = w.identity(IdentityKind::Organisation, None);
    let pool = w.account(&org, Custody::Owned);
    let m1 = w.person();
    let m2 = w.person();
    let g1 = w.delegate(&org, m1.principal, pool, &[OperationKind::Transfer], 100);
    let mut r = request(
        GrantKind::Delegated,
        m2.principal,
        Subject::Account(pool),
        &[OperationKind::Transfer],
    );
    r.per_window = Some(Window {
        length: NonZeroU64::new(1_000).unwrap(),
        limit: nz(500),
    });
    let ctx = w.ctx(org);
    let g2 = w.s.grant_authority(&ctx, r).unwrap().value;
    assert_ne!(g1, g2);
    for (member, grant, amount) in [(m1, g1, 100), (m2, g2, 500)] {
        let ctx = w.ctx(member);
        let d = w.s.authorise(&ctx, &w.transfer(pool, amount)).unwrap();
        assert_eq!(d.used.kind, AuthorityKind::Delegated);
        assert_eq!(d.used.grant, Some(grant));
        assert_eq!(d.used.principal, member.principal);
        let over = w.transfer(pool, amount + 1);
        assert_eq!(w.authorise(member, &over), Err(Refusal::BoundExceeded));
        let stored = w.s.state().grant(&grant).unwrap();
        assert_eq!(stored.principal(), member.principal);
        assert_eq!(stored.subject(), Subject::Account(pool));
    }
    Outcome::Pass
}

/// A4.2: a drawing no authority covers is refused with `ERR-NotAuthorised`, never let through.
/// A principal with no authority over the pool at all does not reach it (`INV-16`), and is
/// refused with `ERR-UnknownAccount` (`NOTES.md`).
fn ac_a4_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let org = w.identity(IdentityKind::Organisation, None);
    let pool = w.account(&org, Custody::Owned);
    let reserver = w.person();
    let lapsed = w.person();
    let stranger = w.person();
    w.delegate(
        &org,
        reserver.principal,
        pool,
        &[OperationKind::Reserve],
        100,
    );
    let mut r = request(
        GrantKind::Delegated,
        lapsed.principal,
        Subject::Account(pool),
        &[OperationKind::Transfer],
    );
    r.per_operation = Some(nz(100));
    r.expires = Some(w.now + 1);
    let ctx = w.ctx(org);
    w.s.grant_authority(&ctx, r).unwrap();
    let drawing = w.transfer(pool, 1);
    assert_eq!(w.authorise(reserver, &drawing), Err(Refusal::NotAuthorised));
    let mut late = w.ctx(lapsed);
    late.now = w.now + 1;
    assert_eq!(
        w.s.authorise(&late, &drawing).map(|d| d.used.kind),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        w.authorise(stranger, &drawing),
        Err(Refusal::UnknownAccount)
    );
    Outcome::Pass
}

/// A6.1: closing with a non-zero balance fails with `ERR-AccountNotEmpty`.
fn ac_a6_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    w.issue(issuer, usd(), acct, 1);
    let ctx = w.ctx(alice);
    assert_eq!(
        w.s.close_account(&ctx, acct).map(|a| a.value),
        Err(Refusal::AccountNotEmpty)
    );
    assert_eq!(
        w.s.state().account(&acct).unwrap().status(),
        AccountStatus::Active
    );
    Outcome::Pass
}

/// A6.2: a balance moves out by a posting to a named destination; then the account closes, and
/// is kept.
fn ac_a6_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    w.operator_redeems();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Delegated);
    w.issue(issuer, usd(), acct, 40);
    let ctx = w.ctx(issuer);
    let moved = w.s.redeem(&ctx, usd(), acct, m(40)).unwrap();
    assert_eq!(moved.record.postings.len(), 1);
    assert_eq!(moved.record.accounts, vec![acct, w.f.issuance]);
    let ctx = w.ctx(alice);
    w.s.close_account(&ctx, acct).unwrap();
    let kept = w.s.state().account(&acct).unwrap();
    assert_eq!(kept.status(), AccountStatus::Closed);
    let ctx = w.ctx(issuer);
    assert_eq!(
        w.s.issue(&ctx, usd(), acct, m(1)).map(|a| a.value),
        Err(Refusal::AccountClosed)
    );
    Outcome::Pass
}

/// F1.1: balances are kept separate per asset.
fn ac_f1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    w.operator_redeems();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Delegated);
    w.issue(issuer, usd(), acct, 100);
    w.issue(issuer, btc(), acct, 5);
    let (s_usd, s_btc) = (w.sub_account(acct, usd()), w.sub_account(acct, btc()));
    assert_ne!(s_usd, s_btc);
    let (k_usd, k_btc) = (
        w.s.kernel_account(&s_usd).unwrap(),
        w.s.kernel_account(&s_btc).unwrap(),
    );
    assert_ne!(k_usd.ledger, k_btc.ledger);
    let ctx = w.ctx(issuer);
    w.s.redeem(&ctx, usd(), acct, m(40)).unwrap();
    assert_eq!(w.available(&alice, acct, usd()), 60);
    assert_eq!(w.available(&alice, acct, btc()), 5);
    Outcome::Pass
}

/// F1.2: any operation naming an unregistered asset fails with `ERR-UnknownAsset`.
fn ac_f1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(issuer);
    assert_eq!(
        w.s.issue(&ctx, xyz(), acct, m(1)).map(|a| a.value),
        Err(Refusal::UnknownAsset)
    );
    // Alice reaches her account, so the asset is what is refused.
    let ctx = w.ctx(alice);
    assert_eq!(
        w.s.redeem(&ctx, xyz(), acct, m(1)).map(|a| a.value),
        Err(Refusal::UnknownAsset)
    );
    assert_eq!(w.s.parse_amount("XYZ", "1"), Err(Refusal::UnknownAsset));
    assert_eq!(w.s.parse_amount("U-SD", "1"), Err(Refusal::UnknownAsset));
    let mut r = w.transfer(acct, 1);
    r.asset = Some(xyz());
    assert_eq!(w.authorise(alice, &r), Err(Refusal::UnknownAsset));
    let mut g = request(
        GrantKind::Issuance,
        alice.principal,
        Subject::Asset(xyz()),
        &[OperationKind::Issue],
    );
    g.per_window = Some(Window {
        length: NonZeroU64::new(10).unwrap(),
        limit: nz(1),
    });
    g.assets = Some(NonEmptySet::one(xyz()));
    let ctx = w.ctx(w.operator());
    assert_eq!(
        w.s.grant_authority(&ctx, g).map(|a| a.value),
        Err(Refusal::UnknownAsset)
    );
    Outcome::Pass
}

/// F3.1: excess precision is refused, not rounded; amounts are exact minor units.
fn ac_f3_1__service(h: &mut dyn Harness) -> Outcome {
    let w = World::new(h);
    for (asset, decimal, want) in [
        ("USD", "10.00", Ok(m(1000))),
        ("usd", "10.5", Ok(m(1050))),
        ("USD", "10.001", Err(Refusal::PrecisionExceeded)),
        ("USD", "10.000", Err(Refusal::PrecisionExceeded)),
        ("BTC", "0.12345678", Ok(m(12_345_678))),
        ("BTC", "0.123456789", Err(Refusal::PrecisionExceeded)),
    ] {
        assert_eq!(w.s.parse_amount(asset, decimal), want, "{asset} {decimal}");
    }
    Outcome::Pass
}

/// F4.1: with issuance authority, one posting credits the destination against the issuance
/// counterparty, and the trial balance stays zero.
fn ac_f4_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(issuer);
    let issued = w.s.issue(&ctx, usd(), acct, m(250)).unwrap();
    assert_eq!(issued.record.postings, vec![issued.value]);
    assert_eq!(issued.record.accounts, vec![w.f.issuance, acct]);
    assert_eq!(w.available(&alice, acct, usd()), 250);
    assert_eq!(w.available(&issuer, w.f.issuance, usd()), -250);
    assert_eq!(w.s.trial_balance(&usd()), Ok(0));
    Outcome::Pass
}

/// F4.2: without issuance authority for the asset, issuance fails with `ERR-NotAuthorised`.
fn ac_f4_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let op = w.operator();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    // The operator itself holds no issuance authority until it is granted.
    let ctx = w.ctx(op);
    assert_eq!(
        w.s.issue(&ctx, usd(), acct, m(1)).map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // Authority over USD is not authority over EUR.
    w.issuance(op.principal, usd(), &[OperationKind::Issue]);
    let ctx = w.ctx(op);
    assert_eq!(
        w.s.issue(&ctx, eur(), acct, m(1)).map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(w.s.trial_balance(&eur()), Ok(0));
    Outcome::Pass
}

/// F4.3: a redemption takes value out of available and posts it against the issuance
/// counterparty.
fn ac_f4_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let issuer = w.issuer();
    w.operator_redeems();
    let alice = w.person();
    let acct = w.account(&alice, Custody::Delegated);
    w.issue(issuer, usd(), acct, 90);
    let ctx = w.ctx(issuer);
    let r = w.s.redeem(&ctx, usd(), acct, m(30)).unwrap();
    assert_eq!(r.record.accounts, vec![acct, w.f.issuance]);
    assert_eq!(w.available(&alice, acct, usd()), 60);
    assert_eq!(w.available(&issuer, w.f.issuance, usd()), -60);
    assert_eq!(w.s.trial_balance(&usd()), Ok(0));
    // It cannot take more than is available.
    let ctx = w.ctx(issuer);
    assert_eq!(
        w.s.redeem(&ctx, usd(), acct, m(61)).map(|a| a.value),
        Err(Refusal::InsufficientAvailable)
    );
    Outcome::Pass
}

/// G1.1: a grant names its operations and at least one bound; one with none cannot be expressed.
fn ac_g1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let agent = w.agent(&alice);
    let acct = w.account(&alice, Custody::Owned);
    let g = w.delegate(
        &alice,
        agent.principal,
        acct,
        &[OperationKind::Transfer, OperationKind::Reserve],
        75,
    );
    let stored = w.s.state().grant(&g).unwrap();
    assert_eq!(
        stored.ops(),
        &ops(&[OperationKind::Transfer, OperationKind::Reserve])
    );
    assert_eq!(stored.bounds().per_operation(), Some(nz(75)));
    let ctx = w.ctx(alice);
    assert_eq!(
        w.s.grant_authority(
            &ctx,
            request(
                GrantKind::Delegated,
                agent.principal,
                Subject::Account(acct),
                &[OperationKind::Transfer],
            ),
        )
        .map(|a| a.value),
        Err(Refusal::UnboundedGrant)
    );
    assert_eq!(
        Bounds::new(None, None, None, None),
        Err(Refusal::UnboundedGrant)
    );
    Outcome::Pass
}

/// Alice, an agent of hers, an account of hers with 100 USD, and the agent able to redeem up to
/// 50 from it: a delegated grant from Alice and an issuance grant from the operator.
fn agent_world(w: &mut World<'_>) -> (Caller, Caller, AccountId) {
    let issuer = w.issuer();
    let alice = w.person();
    let agent = w.agent(&alice);
    let acct = w.account(&alice, Custody::Owned);
    w.issue(issuer, usd(), acct, 100);
    w.delegate(&alice, agent.principal, acct, &[OperationKind::Redeem], 50);
    w.issuance(agent.principal, usd(), &[OperationKind::Redeem]);
    (alice, agent, acct)
}

/// G1.2: an agent acting within its bounds succeeds, recorded as the agent's, on Alice's behalf,
/// and separate from Alice's own actions.
fn ac_g1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, agent, acct) = agent_world(&mut w);
    let ctx = w.ctx(agent);
    let done = w.s.redeem(&ctx, usd(), acct, m(30)).unwrap();
    let on_account = done.record.authorities[0];
    assert_eq!(on_account.kind, AuthorityKind::Delegated);
    assert_eq!(on_account.principal, agent.principal);
    assert_eq!(on_account.credential, agent.credential);
    assert_eq!(on_account.on_behalf_of, Some(alice.principal));
    assert_eq!(w.s.state().record(&done.record.op), Some(&done.record));
    assert_eq!(w.available(&alice, acct, usd()), 70);
    // Alice's own action is recorded as hers, under owner authority.
    let a2 = w.account(&alice, Custody::Owned);
    let ctx = w.ctx(alice);
    let own = w.s.close_account(&ctx, a2).unwrap();
    assert_eq!(own.record.authorities[0].kind, AuthorityKind::Owner);
    assert_eq!(own.record.authorities[0].principal, alice.principal);
    assert_eq!(own.record.authorities[0].on_behalf_of, None);
    Outcome::Pass
}

/// G1.3: past its bound the agent fails with `ERR-BoundExceeded`, and nothing changes.
fn ac_g1_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, agent, acct) = agent_world(&mut w);
    let ctx = w.ctx(agent);
    let op = ctx.op;
    assert_eq!(
        w.s.redeem(&ctx, usd(), acct, m(51)).map(|a| a.value),
        Err(Refusal::BoundExceeded)
    );
    assert!(w.s.state().record(&op).is_none());
    assert_eq!(w.available(&alice, acct, usd()), 100);
    assert_eq!(w.s.trial_balance(&usd()), Ok(0));
    Outcome::Pass
}

/// G2.1: an agent has its own identity and its own credential, and never acts under Alice's.
fn ac_g2_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let agent = w.agent(&alice);
    assert_ne!(agent.principal, alice.principal);
    assert_ne!(agent.credential, alice.credential);
    let stored = w.s.state().identity(&agent.principal).unwrap();
    assert_eq!(stored.kind(), IdentityKind::Agent);
    assert_eq!(stored.acts_for(), Some(alice.principal));
    assert_eq!(
        w.s.state()
            .credential(&agent.credential)
            .unwrap()
            .identity(),
        agent.principal
    );
    let acct = w.account(&alice, Custody::Owned);
    let r = w.transfer(acct, 1);
    for caller in [
        Caller::new(agent.principal, alice.credential),
        Caller::new(alice.principal, agent.credential),
    ] {
        assert_eq!(w.authorise(caller, &r), Err(Refusal::CredentialInvalid));
    }
    // An agent cannot be opened without the identity it acts for.
    let ctx = w.ctx(w.operator());
    assert_eq!(
        w.s.open_identity(&ctx, IdentityKind::Agent, None, Verifier::new(vec![3]))
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    Outcome::Pass
}

/// G2.2: an agent's credential cannot widen its own scope.
fn ac_g2_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let agent = w.agent(&alice);
    let acct = w.account(&alice, Custody::Owned);
    let other = w.account(&alice, Custody::Owned);
    let g = w.delegate(
        &alice,
        agent.principal,
        acct,
        &[OperationKind::Transfer],
        20,
    );
    // It may narrow; declaring more changes nothing it can do.
    let mut wider = request(
        GrantKind::Delegated,
        agent.principal,
        Subject::Account(acct),
        &[OperationKind::Transfer, OperationKind::Redeem],
    );
    wider.per_operation = Some(nz(10_000));
    wider.parent = Some(g);
    let ctx = w.ctx(agent);
    w.s.grant_authority(&ctx, wider).unwrap();
    let over = w.transfer(acct, 21);
    assert_eq!(w.authorise(agent, &over), Err(Refusal::BoundExceeded));
    let mut redeem = w.transfer(acct, 1);
    redeem.kind = OperationKind::Redeem;
    assert_eq!(w.authorise(agent, &redeem), Err(Refusal::NotAuthorised));
    // It cannot reach Alice's other account, nor grant itself authority over it.
    let elsewhere = w.transfer(other, 1);
    assert_eq!(w.authorise(agent, &elsewhere), Err(Refusal::UnknownAccount));
    let mut self_grant = request(
        GrantKind::Delegated,
        agent.principal,
        Subject::Account(other),
        &[OperationKind::Transfer],
    );
    self_grant.per_operation = Some(nz(1));
    let ctx = w.ctx(agent);
    assert_eq!(
        w.s.grant_authority(&ctx, self_grant).map(|a| a.value),
        Err(Refusal::UnknownAccount)
    );
    Outcome::Pass
}

/// G3.1: a revocation stops every credential derived from the authority, at once.
fn ac_g3_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let agent = w.agent(&alice);
    let helper = w.agent(&alice);
    let acct = w.account(&alice, Custody::Owned);
    let root = w.delegate(
        &alice,
        agent.principal,
        acct,
        &[OperationKind::Transfer],
        100,
    );
    let mut child = request(
        GrantKind::Delegated,
        helper.principal,
        Subject::Account(acct),
        &[OperationKind::Transfer],
    );
    child.per_operation = Some(nz(10));
    child.parent = Some(root);
    let ctx = w.ctx(agent);
    w.s.grant_authority(&ctx, child).unwrap();
    let r = w.transfer(acct, 5);
    for who in [agent, helper] {
        assert_eq!(w.authorise(who, &r), Ok(AuthorityKind::Delegated));
    }
    let ctx = w.ctx(alice);
    w.s.revoke_authority(&ctx, root).unwrap();
    // The same instant: no expiry to wait for.
    for who in [agent, helper] {
        assert_eq!(w.authorise(who, &r), Err(Refusal::AuthorityRevoked));
        assert_eq!(
            w.s.reach_account(&who, &acct, w.now),
            Err(Refusal::UnknownAccount)
        );
    }
    Outcome::Pass
}

/// I4.1: no identifier contains personal data. Identifiers derive from the operation and an
/// ordinal only; nothing a caller supplies enters them.
fn ac_i4_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let ctx = w.ctx(w.operator());
    let personal = b"alice@example.com +57 300 000 0000".to_vec();
    let opened =
        w.s.open_identity(&ctx, IdentityKind::Person, None, Verifier::new(personal))
            .unwrap()
            .value;
    assert_eq!(opened.identity, IdentityId::mint(&ctx.op, 0));
    let alice = Caller::new(opened.identity, opened.credential);
    let acct = w.account(&alice, Custody::Owned);
    for (did, kind) in [
        (opened.identity.to_did(), IdKind::Identity),
        (opened.credential.to_did(), IdKind::Credential),
        (acct.to_did(), IdKind::Account),
    ] {
        assert_eq!(id::validate(&did), Ok(kind));
        for fragment in ["alice", "example"] {
            assert!(!did.to_lowercase().contains(fragment), "{did}");
        }
    }
    Outcome::Pass
}

/// I4.2: erasing personal data needs no identifier rewritten. The book holds none to erase:
/// closing the identity leaves every identifier, and the external account reference, as they
/// were.
fn ac_i4_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let acct = w.account(&alice, Custody::Owned);
    let reference = account_reference(&acct);
    let ctx = w.ctx(w.operator());
    w.s.set_identity_status(&ctx, alice.principal, IdentityStatus::Closed)
        .unwrap();
    let identity = w.s.state().identity(&alice.principal).unwrap();
    assert_eq!(identity.id(), alice.principal);
    assert_eq!(identity.status(), IdentityStatus::Closed);
    // Nothing on the record could carry personal data: its identity, how it was minted, its
    // kind, status and principal.
    let fields = format!("{identity:?}");
    assert!(fields.starts_with("Identity { id: "), "{fields}");
    assert_eq!(w.s.state().account(&acct).unwrap().id(), acct);
    assert_eq!(account_reference(&acct), reference);
    Outcome::Pass
}
