// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of the query surface over the real kernel (`F-011`, T-011-01 to T-011-09). The
//! acceptance and invariant cases also run in the conformance suite
//! (`crates/conformance/src/cases/f011.rs`) on both stores; these cover what needs the library's
//! insides: the store called with a narrowed scope, brute-force scans of the journal, balance
//! checkpoints, an injected drift, reads counted at the leaf store, and the 10 000-query fuzz.

use std::collections::BTreeSet as StdSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::query::journal::{CHECKPOINT_EVERY, JournalRefusal, PostingWrite};
use crate::query::{
    AuthorisedScope, BalanceKind, Cursor, Direction, Incident, MAX_PAGE, Movement, MovementFilter,
    MovementStatus, MovementsRequest, QueryStore, Recorder, StatementRequest, Subject as Whose,
    TimeRange,
};
use ledger_kernel::book::{Step, Unit};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, MemStore, StoreError, SubRoots,
};
use ledger_kernel::vault::MemVault;

// ---------------------------------------------------------------- fixture

fn asset(id: &str) -> AssetId {
    AssetId::parse(id).unwrap()
}

fn usd() -> AssetId {
    asset("USD")
}

fn eur() -> AssetId {
    asset("EUR")
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

/// A tiny deterministic generator, so a failing workload can be replayed from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A book with `USD` and `EUR` (precision 2) and `OLD` (deprecated), default deadlines for
/// reservations and transfers, and the operator able to issue and redeem USD and EUR. Every
/// command advances the time by one, so time ranges have something to select.
struct Q<S: LeafStore> {
    ledger: Ledger<S, MemVault>,
    founded: Founded,
    next: u64,
    now: Time,
}

impl Q<MemStore> {
    fn new() -> Q<MemStore> {
        Q::on(MemStore::new())
    }
}

impl<S: LeafStore> Q<S> {
    fn on(store: S) -> Q<S> {
        let book = Book::bootstrap(store, MemVault::new(), KeyRing::from_root([11; 32])).unwrap();
        let decl = |id: &str, deprecated| AssetDecl {
            id: asset(id),
            location: Some(
                crate::asset::AssetLocation::parse(&alloc::format!("Test{id}")).unwrap(),
            ),
            precision: 2,
            kind: AssetKind::Fiat,
            deprecated,
        };
        let genesis = Genesis {
            assets: vec![decl("USD", false), decl("EUR", false), decl("OLD", true)],
            operator_verifier: Verifier::new(vec![0]),
            lifecycle: crate::lifecycle::LifecycleConfig::new([11; 16], 60, 3_600)
                .with_default_deadline(OperationKind::Reserve, 1_000_000)
                .with_default_deadline(OperationKind::Transfer, 1_000_000)
                .with_recovery(crate::custody::testing::config()),
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
        let mut q = Q {
            ledger,
            founded,
            next: 0,
            now: 1,
        };
        for a in [usd(), eur()] {
            let op = q.operator();
            let ctx = q.ctx(op);
            q.ledger
                .grant_authority(
                    &ctx,
                    GrantRequest {
                        kind: GrantKind::Issuance,
                        principal: op.principal,
                        subject: Subject::Asset(a),
                        ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                            .unwrap(),
                        per_operation: None,
                        per_window: Some(Window {
                            length: core::num::NonZeroU64::new(1_000_000).unwrap(),
                            limit: nz(u64::MAX / 2),
                        }),
                        assets: Some(NonEmptySet::one(a)),
                        counterparties: None,
                        parent: None,
                        expires: None,
                    },
                )
                .unwrap();
        }
        q
    }

    fn op(&mut self) -> OperationId {
        self.next += 1;
        let mut b = [0x11; 16];
        b[8..].copy_from_slice(&self.next.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.now += 1;
        Ctx {
            caller,
            op: self.op(),
            now: self.now,
        }
    }

    fn on_op(&mut self, caller: Caller, op: OperationId) -> Ctx {
        self.now += 1;
        Ctx {
            caller,
            op,
            now: self.now,
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

    fn account(&mut self, owner: &Caller, custody: Custody) -> AccountId {
        let ctx = self.ctx(*owner);
        let acknowledged = (custody == Custody::Owned).then(|| {
            self.ledger
                .recovery_terms(owner, &owner.principal)
                .unwrap()
                .digest()
        });
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

    fn issue(&mut self, to: AccountId, a: AssetId, amount: u64) -> u64 {
        let ctx = self.ctx(self.operator());
        self.ledger.issue(&ctx, a, to, m(amount)).unwrap().value
    }

    fn transfer(
        &mut self,
        by: Caller,
        from: AccountId,
        to: AccountId,
        a: AssetId,
        amount: u64,
    ) -> Result<Vec<u64>, Refusal> {
        let ctx = self.ctx(by);
        self.ledger
            .transfer(&ctx, &Transfer::single(from, to, a, m(amount)))
            .map(|x| x.value)
    }

    fn reserve(
        &mut self,
        by: Caller,
        payer: AccountId,
        a: AssetId,
        amount: u64,
    ) -> Result<OperationId, Refusal> {
        let ctx = self.ctx(by);
        self.ledger
            .reserve(&ctx, payer, a, m(amount))
            .map(|_| ctx.op)
    }

    fn capture(
        &mut self,
        by: Caller,
        op: OperationId,
        to: AccountId,
        amount: u64,
    ) -> Result<u64, Refusal> {
        let ctx = self.on_op(by, op);
        self.ledger.capture(&ctx, to, m(amount)).map(|a| a.value)
    }

    fn release(&mut self, by: Caller, op: OperationId) -> Result<u64, Refusal> {
        let ctx = self.on_op(by, op);
        self.ledger.release(&ctx).map(|a| a.value)
    }

    /// A grant from `owner` to `to` over `account`, for `ops`, bounded per operation.
    fn delegate(
        &mut self,
        owner: Caller,
        to: IdentityId,
        account: AccountId,
        ops: &[OperationKind],
    ) {
        let ctx = self.ctx(owner);
        self.ledger
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Delegated,
                    principal: to,
                    subject: Subject::Account(account),
                    ops: NonEmptySet::new(ops.iter().copied().collect()).unwrap(),
                    per_operation: Some(nz(1_000_000)),
                    per_window: None,
                    assets: None,
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
    }
}

/// People and their accounts, for random workloads.
struct World {
    people: Vec<(Caller, Vec<AccountId>)>,
    open: Vec<(Caller, OperationId)>,
}

impl World {
    fn new(q: &mut Q<MemStore>, people: usize, accounts: usize) -> World {
        let mut w = World {
            people: Vec::new(),
            open: Vec::new(),
        };
        for _ in 0..people {
            let p = q.person();
            let accts: Vec<AccountId> = (0..accounts)
                .map(|_| q.account(&p, Custody::Owned))
                .collect();
            w.people.push((p, accts));
        }
        w
    }

    fn all(&self) -> Vec<AccountId> {
        self.people.iter().flat_map(|(_, a)| a.clone()).collect()
    }
}

/// `n` random commands: issue, transfer, reserve, capture, release, redeem. Refusals are part of
/// the workload and change nothing.
fn workload(q: &mut Q<MemStore>, w: &mut World, rng: &mut Rng, n: usize) {
    let assets = [usd(), eur()];
    for _ in 0..n {
        let (who, accts) = w.people[rng.below(w.people.len() as u64) as usize].clone();
        let from = accts[rng.below(accts.len() as u64) as usize];
        let a = assets[rng.below(2) as usize];
        let amount = 1 + rng.below(40);
        match rng.below(7) {
            0 | 1 => {
                q.issue(from, a, amount * 3);
            }
            2 | 3 => {
                let all = w.all();
                let to = all[rng.below(all.len() as u64) as usize];
                let _ = q.transfer(who, from, to, a, amount);
            }
            4 => {
                if let Ok(op) = q.reserve(who, from, a, amount) {
                    w.open.push((who, op));
                }
            }
            5 => {
                if !w.open.is_empty() {
                    let (by, op) = w.open.remove(rng.below(w.open.len() as u64) as usize);
                    let all = w.all();
                    let to = all[rng.below(all.len() as u64) as usize];
                    let _ = q.capture(by, op, to, 1 + rng.below(amount));
                }
            }
            _ => {
                if !w.open.is_empty() {
                    let (by, op) = w.open.remove(rng.below(w.open.len() as u64) as usize);
                    let _ = q.release(by, op);
                }
            }
        }
    }
}

/// Every movement of `accounts` a brute-force scan of the whole journal finds below `view`.
fn brute(q: &Q<MemStore>, accounts: &[AccountId], f: &MovementFilter, view: u64) -> Vec<Movement> {
    let v = q.ledger.view(q.now).unwrap();
    let mut out = Vec::new();
    for e in q.ledger.state().journal().iter() {
        if e.seq >= view {
            continue;
        }
        if let Some(t) = f.time {
            if e.at < t.from || e.at >= t.to {
                continue;
            }
        }
        let auth = v.authorities_of(e);
        for mv in crate::query::movement::movements_of(e, |a| accounts.contains(a), &auth) {
            if f.account.is_some_and(|a| mv.account != a)
                || f.asset.is_some_and(|a| mv.asset != a)
                || f.direction.is_some_and(|d| mv.direction != d)
                || f.counterparty.is_some_and(|c| mv.counterparty != Some(c))
            {
                continue;
            }
            out.push(mv);
        }
    }
    out
}

/// Every page of a movement list, following the cursor.
fn pages(q: &Q<MemStore>, caller: &Caller, mut req: MovementsRequest) -> Vec<Movement> {
    let mut out = Vec::new();
    loop {
        let page = q
            .ledger
            .view(q.now)
            .unwrap()
            .movements(caller, &req)
            .unwrap();
        out.extend(page.movements);
        match page.next {
            Some(c) => req.cursor = Some(c),
            None => return out,
        }
    }
}

fn list(subject: Whose, filter: MovementFilter, limit: u32) -> MovementsRequest {
    MovementsRequest {
        subject,
        filter,
        limit: Some(limit),
        cursor: None,
    }
}

// ---------------------------------------------------------------- T-011-01: one view, no writes

/// A fingerprint of everything a query must not change: the kernel's root and posting sequence,
/// and the service store's journal, operations and reservations.
fn fingerprint(q: &Q<MemStore>) -> ([u8; 32], u64, usize, usize, usize) {
    let s = q.ledger.state();
    (
        q.ledger.book().root().unwrap(),
        q.ledger.book().store().next_posting_seq().unwrap(),
        s.journal().len(),
        s.operations().all().count(),
        s.reservations().iter().count(),
    )
}

/// `AC-H4.1`, `REQ-QR-3`: ten thousand random queries of every kind, by every kind of caller, over
/// a random book, and the composite root, the sequence and the service store are unchanged after
/// every one.
#[test]
fn ac_h4_1_ten_thousand_queries_change_nothing() {
    let mut q = Q::new();
    let mut w = World::new(&mut q, 3, 2);
    let mut rng = Rng(0x0DDBA11);
    workload(&mut q, &mut w, &mut rng, 60);
    let stranger = q.person();
    let operator = q.operator();
    let before = fingerprint(&q);
    let accounts = w.all();
    let n: usize = std::env::var("F011_QUERIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let view = q.ledger.view(q.now).unwrap();
    let mut results = 0usize;
    for i in 0..n {
        let caller = match rng.below(4) {
            0 => stranger,
            1 => operator,
            _ => w.people[rng.below(3) as usize].0,
        };
        let acct = accounts[rng.below(accounts.len() as u64) as usize];
        let a = if rng.below(2) == 0 { usd() } else { eur() };
        let ok = match i % 7 {
            0 => view.balances(&caller, Whose::Account(acct), None).is_ok(),
            1 => view
                .balances(&caller, Whose::Identity(caller.principal), Some(a))
                .is_ok(),
            2 => view
                .movements(
                    &caller,
                    &list(Whose::Account(acct), MovementFilter::default(), 5),
                )
                .is_ok(),
            3 => view
                .statement(
                    &caller,
                    &StatementRequest {
                        account: acct,
                        asset: a,
                        from: Some(rng.below(q.now)),
                        to: Some(q.now),
                        as_of: None,
                    },
                )
                .is_ok(),
            4 => view.can_spend(&caller, &acct, &a, m(rng.below(50))).is_ok(),
            5 => view.trial_balance(&caller, None).is_ok(),
            _ => view.assets(&caller).is_ok(),
        };
        results += usize::from(ok);
    }
    assert!(results > 0);
    assert_eq!(fingerprint(&q), before);
}

/// `REQ-QR-2`: a view is fixed at the committed posting sequence, and every movement it returns
/// is below it.
#[test]
fn req_qr_2_a_view_is_fixed_at_one_sequence() {
    let mut q = Q::new();
    let alice = q.person();
    let a = q.account(&alice, Custody::Owned);
    q.issue(a, usd(), 100);
    let head = q.ledger.book().store().next_posting_seq().unwrap();
    let view = q.ledger.view(q.now).unwrap();
    assert_eq!(view.sequence(), head);
    let page = view
        .movements(
            &alice,
            &list(Whose::Account(a), MovementFilter::default(), 10),
        )
        .unwrap();
    assert_eq!(page.view, head);
    assert!(page.movements.iter().all(|m| m.seq < head));
}

// ---------------------------------------------------------------- T-011-02: the scope, in the store

/// `INV-16` at the store (SPEC §9's layer): the store is called directly with a scope narrowed to
/// one account. Every read of any other account — one that exists and holds value — is
/// `ERR-UnknownAccount`, the same as an account that does not exist.
#[test]
fn inv_16_the_store_refuses_every_read_outside_the_scope() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let mine = q.account(&alice, Custody::Owned);
    let theirs = q.account(&bob, Custody::Owned);
    q.issue(theirs, usd(), 100);
    q.issue(mine, usd(), 5);
    q.transfer(bob, theirs, mine, usd(), 10).unwrap();
    let missing = AccountId::mint(&q.op(), 0);
    let s = q.ledger.state();
    let scope = AuthorisedScope::narrowed([mine]);
    for other in [theirs, missing] {
        let unknown = Err(Refusal::UnknownAccount);
        assert_eq!(s.account_in(&scope, &other).map(|_| ()), unknown);
        assert_eq!(s.reserved_in(&scope, &other, &usd()).map(|_| ()), unknown);
        assert_eq!(
            s.postings_in(&scope, &other, 0, u64::MAX, 10).map(|_| ()),
            unknown
        );
        assert_eq!(
            s.position_postings_in(&scope, &other, &usd(), 0, u64::MAX, 10)
                .map(|_| ()),
            unknown
        );
        assert_eq!(
            s.balance_before_in(&scope, &other, &usd(), u64::MAX)
                .map(|_| ()),
            unknown
        );
        assert_eq!(
            s.balance_now_in(&scope, &other, &usd()).map(|_| ()),
            unknown
        );
    }
    // In scope, the same reads answer.
    assert_eq!(
        s.balance_now_in(&scope, &mine, &usd()).unwrap().available,
        15
    );
    assert_eq!(
        s.postings_in(&scope, &mine, 0, u64::MAX, 10).unwrap().len(),
        2
    );
    // An empty scope reaches nothing at all.
    let none = AuthorisedScope::narrowed([]);
    assert_eq!(
        s.account_in(&none, &mine).map(|_| ()),
        Err(Refusal::UnknownAccount)
    );
}

/// `INV-16`, `REQ-PV-2` at the service: a principal with no grant gets `ERR-UnknownAccount` for
/// every query and account; a delegate reaches only the account it was granted.
#[test]
fn inv_16_no_grant_reaches_nothing_and_a_grant_only_its_account() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let a1 = q.account(&alice, Custody::Owned);
    let a2 = q.account(&alice, Custody::Owned);
    q.issue(a1, usd(), 50);
    q.issue(a2, usd(), 50);
    let view = q.ledger.view(q.now).unwrap();
    for acct in [a1, a2] {
        assert_eq!(
            view.balances(&bob, Whose::Account(acct), None),
            Err(Refusal::UnknownAccount)
        );
        assert_eq!(
            view.movements(
                &bob,
                &list(Whose::Account(acct), MovementFilter::default(), 5)
            ),
            Err(Refusal::UnknownAccount)
        );
        assert_eq!(
            view.statement(
                &bob,
                &StatementRequest {
                    account: acct,
                    asset: usd(),
                    from: Some(0),
                    to: Some(q.now),
                    as_of: None
                }
            ),
            Err(Refusal::UnknownAccount)
        );
        assert_eq!(
            view.can_spend(&bob, &acct, &usd(), m(1)),
            Err(Refusal::UnknownAccount)
        );
    }
    assert_eq!(
        view.balances(&bob, Whose::Identity(alice.principal), None),
        Err(Refusal::UnknownAccount)
    );
    q.delegate(alice, bob.principal, a1, &[OperationKind::Transfer]);
    let view = q.ledger.view(q.now).unwrap();
    let rows = view
        .balances(&bob, Whose::Identity(alice.principal), None)
        .unwrap()
        .rows;
    assert!(rows.iter().all(|r| r.account == a1), "{rows:?}");
    assert_eq!(
        view.balances(&bob, Whose::Account(a2), None),
        Err(Refusal::UnknownAccount)
    );
}

// ---------------------------------------------------------------- T-011-03: balances

/// `AC-H1.1`: an identity with three accounts in two assets gets one result with six rows.
#[test]
fn ac_h1_1_one_result_across_every_account_and_asset() {
    let mut q = Q::new();
    let alice = q.person();
    let accts: Vec<AccountId> = (0..3).map(|_| q.account(&alice, Custody::Owned)).collect();
    for (i, a) in accts.iter().enumerate() {
        q.issue(*a, usd(), 10 + i as u64);
        q.issue(*a, eur(), 20 + i as u64);
    }
    let r = q
        .ledger
        .view(q.now)
        .unwrap()
        .balances(&alice, Whose::Identity(alice.principal), None)
        .unwrap();
    assert_eq!(r.rows.len(), 6, "{r:?}");
    for (i, a) in accts.iter().enumerate() {
        let row = |x: AssetId| {
            *r.rows
                .iter()
                .find(|r| r.account == *a && r.asset == x)
                .unwrap()
        };
        assert_eq!(row(usd()).available, 10 + i as i128);
        assert_eq!(row(eur()).available, 20 + i as i128);
    }
}

/// `AC-H1.2`, `REQ-BA-3`, `REQ-BA-6`: every row labels available and total, `total` is their sum,
/// `reserved` is the open reservations', and the `available` shown is the number a spend is
/// checked against: `can_spend` passes at it and fails one unit above.
#[test]
fn ac_h1_2_available_and_total_from_one_view() {
    let mut q = Q::new();
    let alice = q.person();
    let a = q.account(&alice, Custody::Owned);
    q.issue(a, usd(), 100);
    let op = q.reserve(alice, a, usd(), 30).unwrap();
    let view = q.ledger.view(q.now).unwrap();
    let row = view
        .balances(&alice, Whose::Account(a), Some(usd()))
        .unwrap()
        .rows[0];
    assert_eq!(
        (row.available, row.reserved, row.encumbered, row.total),
        (70, 30, 0, 100)
    );
    assert_eq!(view.can_spend(&alice, &a, &usd(), m(70)), Ok(()));
    assert_eq!(
        view.can_spend(&alice, &a, &usd(), m(71)),
        Err(Refusal::InsufficientAvailable)
    );
    q.capture(alice, op, a, 10).unwrap();
    let row = q
        .ledger
        .view(q.now)
        .unwrap()
        .balances(&alice, Whose::Account(a), Some(usd()))
        .unwrap()
        .rows[0];
    assert_eq!((row.available, row.reserved, row.total), (100, 0, 100));
}

// ---------------------------------------------------------------- T-011-04: movements

/// `AC-H2.3`, `REQ-PO-1`: the status of a movement is a total function of its posting kind.
#[test]
fn ac_h2_3_status_comes_from_the_posting_kind() {
    for kind in PostingKind::ALL {
        let _ = MovementStatus::of(kind);
    }
    assert_eq!(
        MovementStatus::of(PostingKind::Reserve),
        MovementStatus::Held
    );
    assert_eq!(
        MovementStatus::of(PostingKind::Release),
        MovementStatus::Returned
    );
    assert_eq!(
        MovementStatus::of(PostingKind::Capture),
        MovementStatus::Completed
    );
    assert_eq!(
        MovementStatus::of(PostingKind::Reversal),
        MovementStatus::Reversed
    );
}

/// `AC-H2.3`: the lint. No function of the movement module takes text: a status can never be
/// parsed out of a string.
#[test]
fn ac_h2_3_no_function_of_the_movement_module_takes_a_string() {
    let src = include_str!("../../query/movement.rs");
    let mut signature = std::string::String::new();
    let mut in_sig = false;
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        if t.contains("fn ") {
            in_sig = true;
            signature.clear();
        }
        if in_sig {
            signature.push_str(t);
            if t.contains('{') || t.ends_with(';') {
                in_sig = false;
                for bad in ["str", "String", "char"] {
                    assert!(
                        !signature.contains(bad),
                        "a function takes text: {signature}"
                    );
                }
            }
        }
    }
}

/// `AC-H2.2`, `REQ-QR-1`: each filter alone and in pairs gives what a brute-force scan of the
/// journal gives, on a random book, for an account and for an identity.
#[test]
fn ac_h2_2_every_filter_alone_and_in_pairs_matches_a_scan() {
    let mut q = Q::new();
    let mut w = World::new(&mut q, 3, 2);
    let mut rng = Rng(0xF1173);
    workload(&mut q, &mut w, &mut rng, 120);
    let head = q.ledger.book().store().next_posting_seq().unwrap();
    let (alice, accts) = w.people[0].clone();
    let other = w.people[1].1[0];
    let mid = q.now / 2;
    let single: Vec<MovementFilter> = vec![
        MovementFilter::default(),
        MovementFilter {
            account: Some(accts[1]),
            ..Default::default()
        },
        MovementFilter {
            asset: Some(eur()),
            ..Default::default()
        },
        MovementFilter {
            direction: Some(Direction::Debit),
            ..Default::default()
        },
        MovementFilter {
            counterparty: Some(other),
            ..Default::default()
        },
        MovementFilter {
            time: Some(TimeRange {
                from: mid,
                to: q.now,
            }),
            ..Default::default()
        },
    ];
    let mut filters = single.clone();
    for (i, a) in single.iter().enumerate() {
        for b in &single[i + 1..] {
            filters.push(MovementFilter {
                account: a.account.or(b.account),
                asset: a.asset.or(b.asset),
                direction: a.direction.or(b.direction),
                counterparty: a.counterparty.or(b.counterparty),
                time: a.time.or(b.time),
            });
        }
    }
    let mut nonempty = 0;
    for f in &filters {
        for (subject, scope) in [
            (Whose::Account(accts[0]), vec![accts[0]]),
            (Whose::Identity(alice.principal), accts.clone()),
        ] {
            if f.account.is_some_and(|a| !scope.contains(&a)) {
                continue;
            }
            let expect = brute(&q, &scope, f, head);
            let got = pages(&q, &alice, list(subject, *f, 7));
            assert_eq!(got, expect, "{f:?} {subject:?}");
            nonempty += usize::from(!expect.is_empty());
        }
    }
    assert!(nonempty > filters.len(), "the workload selects something");
}

/// `AC-H5.1`, `REQ-QR-4`, `INV-20`: an entry made under operator authority on a delegated-custody
/// account is visible to its owner, marked as operator authority with the operator's principal.
#[test]
fn ac_h5_1_an_operator_entry_is_visible_and_marked() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let a = q.account(&alice, Custody::Delegated);
    let b = q.account(&bob, Custody::Owned);
    q.issue(a, usd(), 100);
    let op = q.operator();
    let ctx = q.ctx(op);
    q.ledger
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
    q.transfer(op, a, b, usd(), 25).unwrap();
    let page = q
        .ledger
        .view(q.now)
        .unwrap()
        .movements(
            &alice,
            &list(Whose::Account(a), MovementFilter::default(), 10),
        )
        .unwrap();
    let debit = page
        .movements
        .iter()
        .find(|m| m.kind == PostingKind::Transfer)
        .unwrap();
    assert_eq!(debit.direction, Direction::Debit);
    assert_eq!(debit.counterparty, Some(b));
    assert_eq!(debit.authorities.len(), 1);
    assert_eq!(debit.authorities[0].kind, AuthorityKind::Operator);
    assert_eq!(debit.authorities[0].principal, op.principal);
    // The issue that funded it is attributed to the issuance authority.
    let issued = page
        .movements
        .iter()
        .find(|m| m.kind == PostingKind::Issue)
        .unwrap();
    assert_eq!(issued.authorities[0].kind, AuthorityKind::Issuance);
    assert_eq!(issued.counterparty, Some(q.founded.issuance));
}

/// A reservation is two entries against the payer, available to reserved; a capture for less
/// takes the reserved amount out and returns the difference to available, in one posting.
#[test]
fn reservations_show_as_reserved_entries_of_the_payer() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let a = q.account(&alice, Custody::Owned);
    let b = q.account(&bob, Custody::Owned);
    q.issue(a, usd(), 100);
    let op = q.reserve(alice, a, usd(), 40).unwrap();
    q.capture(alice, op, b, 30).unwrap();
    let got = q
        .ledger
        .view(q.now)
        .unwrap()
        .movements(
            &alice,
            &list(Whose::Account(a), MovementFilter::default(), 10),
        )
        .unwrap()
        .movements;
    let shape: Vec<(PostingKind, BalanceKind, Direction, u64, Option<AccountId>)> = got
        .iter()
        .map(|m| {
            (
                m.kind,
                m.balance,
                m.direction,
                m.amount.get(),
                m.counterparty,
            )
        })
        .collect();
    use BalanceKind::{Available, Reserved};
    use Direction::{Credit, Debit};
    assert_eq!(
        shape,
        vec![
            (
                PostingKind::Issue,
                Available,
                Credit,
                100,
                Some(q.founded.issuance)
            ),
            (PostingKind::Reserve, Available, Debit, 40, None),
            (PostingKind::Reserve, Reserved, Credit, 40, None),
            (PostingKind::Capture, Reserved, Debit, 40, Some(b)),
            (PostingKind::Capture, Available, Credit, 10, None),
        ]
    );
    assert_eq!(got[1].status, MovementStatus::Held);
    assert_eq!(got[3].status, MovementStatus::Completed);
}

// ---------------------------------------------------------------- T-011-05: the cursor

/// `AC-H2.1`, `REQ-QR-5`: paging while writers append. The union of the pages is exactly the
/// list as of the first page's view, with no duplicate and no gap, and a cursor made for one
/// filter is refused with another.
#[test]
fn ac_h2_1_pages_are_stable_while_writers_append() {
    let mut q = Q::new();
    let mut w = World::new(&mut q, 2, 2);
    let mut rng = Rng(0xC0FFEE);
    workload(&mut q, &mut w, &mut rng, 80);
    let (alice, _) = w.people[0].clone();
    let head = q.ledger.book().store().next_posting_seq().unwrap();
    let subject = Whose::Identity(alice.principal);
    let f = MovementFilter::default();
    let expect = brute(&q, &w.people[0].1, &f, head);
    let mut req = list(subject, f, 3);
    let mut got: Vec<Movement> = Vec::new();
    loop {
        let page = q
            .ledger
            .view(q.now)
            .unwrap()
            .movements(&alice, &req)
            .unwrap();
        assert_eq!(page.view, head);
        got.extend(page.movements);
        let Some(c) = page.next else { break };
        // Writers append between pages.
        workload(&mut q, &mut w, &mut rng, 3);
        req.cursor = Some(c);
    }
    assert_eq!(got, expect);
    assert!(
        got.windows(2).all(|p| p[0].seq <= p[1].seq),
        "in sequence order"
    );
    // A cursor with another filter, a forged one, and one from a view not yet reached.
    let first = q
        .ledger
        .view(q.now)
        .unwrap()
        .movements(&alice, &list(subject, f, 1))
        .unwrap();
    let c = first.next.unwrap();
    let view = q.ledger.view(q.now).unwrap();
    let mut other = list(
        subject,
        MovementFilter {
            asset: Some(usd()),
            ..Default::default()
        },
        1,
    );
    other.cursor = Some(c.clone());
    assert_eq!(view.movements(&alice, &other), Err(Refusal::QueryUnbounded));
    let mut forged = c.as_bytes().to_vec();
    forged[12] ^= 0xFF;
    let mut bad = list(subject, f, 1);
    bad.cursor = Some(Cursor::from_bytes(forged));
    assert_eq!(view.movements(&alice, &bad), Err(Refusal::QueryUnbounded));
    bad.cursor = Some(Cursor::from_bytes(vec![1, 2, 3]));
    assert_eq!(view.movements(&alice, &bad), Err(Refusal::QueryUnbounded));
}

// ---------------------------------------------------------------- T-011-06: time to sequence

/// `REQ-PO-2`, `F-011` §6: posting times never decrease along the sequence, even when the time a
/// command is given goes back; and every time range maps to the sequence range a brute-force scan
/// finds.
#[test]
fn time_ranges_map_to_the_sequence_ranges_a_scan_finds() {
    let mut q = Q::new();
    let mut w = World::new(&mut q, 2, 2);
    let mut rng = Rng(0x7117E);
    workload(&mut q, &mut w, &mut rng, 40);
    // A command given an earlier time still commits at the last posting's time or later.
    let (alice, accts) = w.people[0].clone();
    q.now -= 20;
    q.issue(accts[0], usd(), 5);
    workload(&mut q, &mut w, &mut rng, 20);
    let j = q.ledger.state().journal();
    let times: Vec<(u64, Time)> = j.iter().map(|e| (e.seq, e.at)).collect();
    assert!(
        times
            .windows(2)
            .all(|p| p[0].1 <= p[1].1 && p[0].0 < p[1].0)
    );
    let head = q.ledger.book().store().next_posting_seq().unwrap();
    for _ in 0..200 {
        let from = rng.below(q.now + 2);
        let to = from + rng.below(q.now + 2 - from + 1);
        let lo = times
            .iter()
            .find(|(_, at)| *at >= from)
            .map_or(head, |t| t.0);
        let hi = times.iter().find(|(_, at)| *at >= to).map_or(head, |t| t.0);
        assert_eq!(j.first_at_or_after(from, head), lo);
        assert_eq!(j.first_at_or_after(to, head), hi);
        let in_range: StdSet<u64> = times
            .iter()
            .filter(|(_, at)| *at >= from && *at < to)
            .map(|t| t.0)
            .collect();
        let by_seq: StdSet<u64> = times
            .iter()
            .filter(|(s, _)| *s >= lo && *s < hi)
            .map(|t| t.0)
            .collect();
        assert_eq!(in_range, by_seq, "[{from}, {to})");
    }
    let _ = alice;
}

// ---------------------------------------------------------------- T-011-07: statements

/// `AC-H3.1`, `INV-6`: statements over random periods balance to the minor unit — opening plus
/// movements is closing — and the opening equals a brute-force sum of every entry before the
/// period, across balance checkpoints.
#[test]
fn ac_h3_1_statements_over_random_periods_balance() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let a = q.account(&alice, Custody::Owned);
    let b = q.account(&bob, Custody::Owned);
    // Enough postings on one account to cross several checkpoints.
    let mut rng = Rng(0x57A7E);
    q.issue(a, usd(), 10_000);
    let mut open = Vec::new();
    for _ in 0..(5 * CHECKPOINT_EVERY) {
        match rng.below(4) {
            0 => {
                q.transfer(alice, a, b, usd(), 1 + rng.below(20)).unwrap();
            }
            1 => {
                q.issue(a, usd(), 1 + rng.below(20));
            }
            2 => {
                if let Ok(op) = q.reserve(alice, a, usd(), 1 + rng.below(10)) {
                    open.push(op);
                }
            }
            _ => {
                if let Some(op) = open.pop() {
                    q.release(alice, op).unwrap();
                }
            }
        }
    }
    assert!(q.ledger.state().journal().checkpoint_count(&a, &usd()) >= 3);
    let view = q.ledger.view(q.now).unwrap();
    let rec = Recorder::new();
    let journal = q.ledger.state().journal();
    for _ in 0..100 {
        let from = rng.below(q.now + 1);
        let to = from + rng.below(q.now + 2 - from);
        let s = view
            .statement(
                &alice,
                &StatementRequest {
                    account: a,
                    asset: usd(),
                    from: Some(from),
                    to: Some(to),
                    as_of: None,
                },
            )
            .unwrap();
        let mut closing = s.opening;
        for mv in &s.movements {
            match mv.balance {
                BalanceKind::Available => closing.available += mv.delta(),
                BalanceKind::Reserved => closing.reserved += mv.delta(),
                BalanceKind::Encumbered => closing.encumbered += mv.delta(),
            }
        }
        assert_eq!(closing, s.closing);
        let mut brute = crate::query::Sums::default();
        for e in journal.iter().filter(|e| e.seq < s.from_seq) {
            for l in e
                .lines
                .iter()
                .filter(|l| l.account == a && l.asset == usd())
            {
                brute.add(l);
            }
        }
        assert_eq!(brute, s.opening, "[{from}, {to})");
    }
    // A statement from the beginning to now closes on the stored balances.
    let whole = view
        .statement(
            &alice,
            &StatementRequest {
                account: a,
                asset: usd(),
                from: Some(0),
                to: Some(q.now + 1),
                as_of: None,
            },
        )
        .unwrap();
    let row = view
        .balances(&alice, Whose::Account(a), Some(usd()))
        .unwrap()
        .rows[0];
    assert_eq!(whole.closing.available, row.available);
    assert_eq!(whole.closing.reserved, row.reserved);
    assert!(rec.incidents().is_empty());
}

/// `AC-H3.2`, `REQ-QR-2`: the same statement asked twice is identical, byte for byte: a closed
/// period after more postings, and an open one at the same `as_of`.
#[test]
fn ac_h3_2_the_same_statement_twice_is_identical() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let a = q.account(&alice, Custody::Owned);
    let b = q.account(&bob, Custody::Owned);
    q.issue(a, usd(), 500);
    for n in 1..6 {
        q.transfer(alice, a, b, usd(), n).unwrap();
    }
    let end = q.now;
    for n in 1..4 {
        q.transfer(alice, a, b, usd(), n).unwrap();
    }
    let closed = StatementRequest {
        account: a,
        asset: usd(),
        from: Some(0),
        to: Some(end),
        as_of: None,
    };
    let s1 = q
        .ledger
        .view(q.now)
        .unwrap()
        .statement(&alice, &closed)
        .unwrap();
    assert!(s1.closed);
    let open = StatementRequest {
        to: Some(q.now + 100),
        as_of: Some(q.ledger.book().store().next_posting_seq().unwrap()),
        ..closed
    };
    let o1 = q
        .ledger
        .view(q.now)
        .unwrap()
        .statement(&alice, &open)
        .unwrap();
    assert!(!o1.closed);
    // More postings.
    for n in 1..4 {
        q.transfer(alice, a, b, usd(), n).unwrap();
    }
    let s2 = q
        .ledger
        .view(q.now)
        .unwrap()
        .statement(&alice, &closed)
        .unwrap();
    assert_eq!(s1.to_bytes(), s2.to_bytes());
    let o2 = q
        .ledger
        .view(q.now)
        .unwrap()
        .statement(&alice, &open)
        .unwrap();
    assert_eq!(o1.to_bytes(), o2.to_bytes());
    // Without `as_of`, an open period moves with the book.
    let moving = StatementRequest {
        as_of: None,
        ..open
    };
    let m1 = q
        .ledger
        .view(q.now)
        .unwrap()
        .statement(&alice, &moving)
        .unwrap();
    assert_ne!(m1.to_bytes(), o2.to_bytes());
}

/// `INV-6`: an injected drift — a posting applied to the book behind the journal's back — raises
/// an incident, and the statement still reports the entry-derived figures.
#[test]
fn inv_6_an_injected_drift_raises_the_incident() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let a = q.account(&alice, Custody::Owned);
    let b = q.account(&bob, Custody::Owned);
    q.issue(a, usd(), 100);
    q.issue(b, usd(), 100);
    let req = StatementRequest {
        account: a,
        asset: usd(),
        from: Some(0),
        to: Some(q.now + 1_000),
        as_of: None,
    };
    let rec = Recorder::new();
    q.ledger
        .view_with(q.now, &rec)
        .unwrap()
        .statement(&alice, &req)
        .unwrap();
    assert!(rec.incidents().is_empty());
    assert!(q.ledger.reconcile().unwrap().is_empty());
    // A raw posting, straight to the book: the kernel's balance moves, the journal does not.
    let raw = q.op();
    let usd_rec = *q.ledger.state().registry().get(&usd()).unwrap();
    let spec = |id: &AccountId| q.ledger.state().account(id).unwrap().sub_account(&usd_rec);
    let draft = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Transfer, raw.to_ref(), 0),
        vec![
            EntryDraft::debit(spec(&b), nz(7).get()),
            EntryDraft::credit(spec(&a), nz(7).get()),
        ],
    )
    .unwrap();
    let unit = Unit {
        steps: vec![Step {
            creations: Vec::new(),
            chain: vec![draft],
        }],
    };
    q.ledger.book.apply(&unit, q.now).unwrap();
    let rec = Recorder::new();
    let s = q
        .ledger
        .view_with(q.now, &rec)
        .unwrap()
        .statement(&alice, &req)
        .unwrap();
    assert_eq!(s.closing.available, 100, "entries are the source of truth");
    let drift = Incident::Drift {
        account: a,
        asset: usd(),
        kind: BalanceKind::Available,
        entries: 100,
        stored: 107,
        view: q.ledger.book().store().next_posting_seq().unwrap(),
    };
    assert_eq!(rec.incidents(), vec![drift]);
    assert_eq!(
        q.ledger.reconcile().unwrap().len(),
        2,
        "both accounts drift"
    );
}

/// The journal's own rules: in sequence, never back in time, only what the store holds; a refused
/// posting changes nothing.
#[test]
fn the_journal_refuses_out_of_order_backwards_and_dangling_postings() {
    let mut q = Q::new();
    let alice = q.person();
    let a = q.account(&alice, Custody::Owned);
    let seq = q.issue(a, usd(), 100);
    let at = q.ledger.state().journal().last().unwrap().1;
    let usd_rec = *q.ledger.state().registry().get(&usd()).unwrap();
    let issuance = q
        .ledger
        .state()
        .account(&q.founded.issuance)
        .unwrap()
        .sub_account(&usd_rec);
    let op = q.op();
    let draft = |to: ledger_kernel::account::KernelAccountSpec| {
        PostingDraft::balanced(
            PostingMeta::new(PostingKind::Issue, op.to_ref(), 0),
            vec![
                EntryDraft::debit(issuance, nz(1).get()),
                EntryDraft::credit(to, nz(1).get()),
            ],
        )
        .unwrap()
    };
    let good = q.ledger.state().account(&a).unwrap().sub_account(&usd_rec);
    let w = |seq, at, d: PostingDraft| {
        Write::Posting(PostingWrite {
            seq,
            at,
            operation: op,
            draft: d,
        })
    };
    let before = q.ledger.state().journal().clone();
    let s = &mut q.ledger.state;
    assert_eq!(
        s.commit(&[w(seq, at, draft(good))]),
        Err(StoreRefusal::Journal(JournalRefusal::OutOfOrder))
    );
    assert_eq!(
        s.commit(&[w(seq + 1, at - 1, draft(good))]),
        Err(StoreRefusal::Journal(JournalRefusal::TimeWentBack))
    );
    let mut stranger = good;
    stranger.subject = [0xEE; 16];
    assert_eq!(
        s.commit(&[w(seq + 1, at, draft(stranger))]),
        Err(StoreRefusal::Journal(JournalRefusal::Dangling))
    );
    let wrong_op = Write::Posting(PostingWrite {
        seq: seq + 1,
        at,
        operation: OperationId::from_ref(OperationRef([0xAB; 16])),
        draft: draft(good),
    });
    assert_eq!(
        s.commit(&[wrong_op]),
        Err(StoreRefusal::Journal(JournalRefusal::Mismatch))
    );
    assert_eq!(*s.journal(), before);
    // Accepted, then undone with its batch.
    s.check(&[w(seq + 5, at, draft(good))]).unwrap();
    assert_eq!(*s.journal(), before);
}

// ---------------------------------------------------------------- T-011-08: the rest

/// `INV-5`: both trial-balance checks — on commitments, in the store, and on openings — are zero
/// after a random workload of issues, transfers, reservations, captures and releases.
#[test]
fn inv_5_both_trial_balance_checks_are_zero() {
    let mut q = Q::new();
    let mut w = World::new(&mut q, 3, 2);
    let mut rng = Rng(0x7B7B);
    for round in 0..4 {
        workload(&mut q, &mut w, &mut rng, 30);
        let op = q.operator();
        let t = q
            .ledger
            .view(q.now)
            .unwrap()
            .trial_balance(&op, None)
            .unwrap();
        assert!(t.is_zero(), "round {round}: {t:?}");
        assert!(t.commitments.accounts > 0);
        // The store-only form, over every account the delta log ever wrote.
        let store = q.ledger.book().store();
        let all = crate::query::trial::log_accounts(store).unwrap();
        assert!(
            crate::query::trial::commitment_trial(store, &all)
                .unwrap()
                .is_zero()
        );
    }
    // Not the holder's.
    let alice = w.people[0].0;
    assert_eq!(
        q.ledger.view(q.now).unwrap().trial_balance(&alice, None),
        Err(Refusal::NotAuthorised)
    );
}

/// `can_spend` names every refusal, in `F-008`'s order, and never changes anything.
#[test]
fn can_spend_returns_a_named_reason_for_every_refusal() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let a = q.account(&alice, Custody::Owned);
    q.issue(a, usd(), 50);
    let view = q.ledger.view(q.now).unwrap();
    assert_eq!(view.can_spend(&alice, &a, &usd(), m(50)), Ok(()));
    assert_eq!(
        view.can_spend(&alice, &a, &usd(), m(0)),
        Err(Refusal::ZeroAmount)
    );
    assert_eq!(
        view.can_spend(&alice, &a, &asset("XYZ"), m(1)),
        Err(Refusal::UnknownAsset)
    );
    assert_eq!(
        view.can_spend(&alice, &a, &asset("OLD"), m(1)),
        Err(Refusal::AssetDeprecated)
    );
    assert_eq!(
        view.can_spend(&bob, &a, &usd(), m(1)),
        Err(Refusal::UnknownAccount)
    );
    assert_eq!(
        view.can_spend(&alice, &a, &usd(), m(51)),
        Err(Refusal::InsufficientAvailable)
    );
    let forged = Caller::new(alice.principal, bob.credential);
    assert_eq!(
        view.can_spend(&forged, &a, &usd(), m(1)),
        Err(Refusal::CredentialInvalid)
    );
    // A delegate without transfer authority, and one past its bound.
    q.delegate(alice, bob.principal, a, &[OperationKind::Reserve]);
    assert_eq!(
        q.ledger
            .view(q.now)
            .unwrap()
            .can_spend(&bob, &a, &usd(), m(1)),
        Err(Refusal::NotAuthorised)
    );
    let carol = q.person();
    let ctx = q.ctx(alice);
    q.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: carol.principal,
                subject: Subject::Account(a),
                ops: NonEmptySet::one(OperationKind::Transfer),
                per_operation: Some(nz(5)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    let view = q.ledger.view(q.now).unwrap();
    assert_eq!(view.can_spend(&carol, &a, &usd(), m(5)), Ok(()));
    assert_eq!(
        view.can_spend(&carol, &a, &usd(), m(6)),
        Err(Refusal::BoundExceeded)
    );
}

/// `operation(reference)`: state and class, deadline, evidence, outcome and postings, for the
/// initiator and for a holder of authority over an account it touched; nobody else can tell it
/// from a reference that does not exist.
#[test]
fn operation_shows_its_state_and_hides_from_strangers() {
    let mut q = Q::new();
    let alice = q.person();
    let bob = q.person();
    let carol = q.person();
    let a = q.account(&alice, Custody::Owned);
    let b = q.account(&bob, Custody::Owned);
    q.issue(a, usd(), 100);
    let op = q.reserve(alice, a, usd(), 30).unwrap();
    let never = q.op();
    let view = q.ledger.view(q.now).unwrap();
    let v = view.operation(&alice, &op).unwrap();
    assert_eq!(v.status, Some(Shown::of(crate::lifecycle::State::Reserved)));
    assert!(v.deadline.is_some());
    assert_eq!(v.outcome, None);
    assert_eq!(v.postings.len(), 1);
    assert_eq!(view.operation(&carol, &op), Err(Refusal::UnknownOperation));
    assert_eq!(
        view.operation(&alice, &never),
        Err(Refusal::UnknownOperation)
    );
    q.capture(alice, op, b, 30).unwrap();
    let view = q.ledger.view(q.now).unwrap();
    let v = view.operation(&alice, &op).unwrap();
    assert_eq!(v.outcome, Some(crate::lifecycle::State::Settled));
    assert_eq!(v.postings.len(), 2);
    assert!(v.evidence.len() >= 3);
    // The destination's owner reaches an account the capture touched.
    assert!(view.operation(&bob, &op).is_ok());
    // The registry, to any authenticated principal.
    let assets = view.assets(&carol).unwrap();
    assert_eq!(assets.len(), 3);
    assert_eq!(
        view.assets(&Caller::new(carol.principal, alice.credential)),
        Err(Refusal::CredentialInvalid)
    );
}

// ---------------------------------------------------------------- T-011-09: bounds

/// `MemStore`, counting every read (`ERR-QueryUnbounded` before any read).
struct Counting {
    inner: MemStore,
    reads: Arc<AtomicUsize>,
}

impl Counting {
    fn read(&self) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }
}

impl LeafStore for Counting {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        self.read();
        self.inner.subroots()
    }
    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        self.read();
        self.inner.load(keys)
    }
    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        self.inner.commit(seq, delta)
    }
    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.read();
        self.inner.last_committed()
    }
    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        self.read();
        self.inner.next_posting_seq()
    }
    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        self.read();
        self.inner.log_record(seq)
    }
}

/// `ERR-QueryUnbounded`, `REQ-CS-5`: a list without a page size, or with one past the maximum,
/// and a statement without both ends of its period, are refused before any read — not the leaf
/// store, not even the credential.
#[test]
fn err_query_unbounded_is_refused_before_any_read() {
    let reads = Arc::new(AtomicUsize::new(0));
    let mut q = Q::on(Counting {
        inner: MemStore::new(),
        reads: reads.clone(),
    });
    let alice = q.person();
    let a = q.account(&alice, Custody::Owned);
    q.issue(a, usd(), 10);
    // A caller that does not resolve: the bounds are checked first.
    let nobody = Caller::new(IdentityId::mint(&q.op(), 0), alice.credential);
    let view = q.ledger.view(q.now).unwrap();
    let before = reads.load(Ordering::SeqCst);
    for limit in [None, Some(0), Some(MAX_PAGE + 1)] {
        let req = MovementsRequest {
            subject: Whose::Account(a),
            filter: MovementFilter::default(),
            limit,
            cursor: None,
        };
        assert_eq!(view.movements(&nobody, &req), Err(Refusal::QueryUnbounded));
        assert_eq!(view.movements(&alice, &req), Err(Refusal::QueryUnbounded));
    }
    for (from, to) in [
        (None, Some(5)),
        (Some(1), None),
        (None, None),
        (Some(9), Some(3)),
    ] {
        let req = StatementRequest {
            account: a,
            asset: usd(),
            from,
            to,
            as_of: None,
        };
        assert_eq!(view.statement(&nobody, &req), Err(Refusal::QueryUnbounded));
    }
    let beyond = StatementRequest {
        account: a,
        asset: usd(),
        from: Some(0),
        to: Some(1),
        as_of: Some(view.sequence() + 1),
    };
    assert_eq!(
        view.statement(&alice, &beyond),
        Err(Refusal::QueryUnbounded)
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "no read on the refused path"
    );
    // A bounded request reads.
    view.movements(
        &alice,
        &MovementsRequest {
            subject: Whose::Account(a),
            filter: MovementFilter::default(),
            limit: Some(MAX_PAGE),
            cursor: None,
        },
    )
    .unwrap();
    view.balances(&alice, Whose::Account(a), None).unwrap();
    assert!(reads.load(Ordering::SeqCst) > before);
}

/// `REQ-PV-5`: every query is attributed to its principal before it reads.
#[test]
fn req_pv_5_every_query_is_attributed() {
    let mut q = Q::new();
    let alice = q.person();
    let a = q.account(&alice, Custody::Owned);
    q.issue(a, usd(), 10);
    let rec = Recorder::new();
    let view = q.ledger.view_with(q.now, &rec).unwrap();
    view.balances(&alice, Whose::Account(a), None).unwrap();
    view.assets(&alice).unwrap();
    let _ = view.balances(
        &Caller::new(alice.principal, q.founded.operator_credential),
        Whose::Account(a),
        None,
    );
    let got = rec.accesses();
    assert_eq!(
        got.len(),
        2,
        "an unattributable access is refused, not traced"
    );
    assert!(got.iter().all(|x| x.principal == alice.principal));
    assert_eq!(got[0].query, crate::query::QueryKind::Balances);
}

/// One grant to `to` over `acct`, made by `by`: `kind`, transfers only, `per_op` per operation and
/// `per_op * 10` per 1 000-unit window, over `assets` (all, for `None`).
fn transfer_grant(
    q: &mut Q<MemStore>,
    by: Caller,
    to: &Caller,
    kind: GrantKind,
    acct: AccountId,
    per_op: u64,
    assets: Option<AssetId>,
) -> crate::id::GrantId {
    let ops = [OperationKind::Transfer];
    grant_for(q, by, to, kind, acct, &ops, per_op, assets)
}

/// [`transfer_grant`] for the operation kinds `ops`.
#[allow(clippy::too_many_arguments)]
fn grant_for(
    q: &mut Q<MemStore>,
    by: Caller,
    to: &Caller,
    kind: GrantKind,
    acct: AccountId,
    ops: &[OperationKind],
    per_op: u64,
    assets: Option<AssetId>,
) -> crate::id::GrantId {
    let ctx = q.ctx(by);
    q.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind,
                principal: to.principal,
                subject: Subject::Account(acct),
                ops: NonEmptySet::new(ops.iter().copied().collect()).unwrap(),
                per_operation: Some(nz(per_op)),
                per_window: Some(Window {
                    length: core::num::NonZeroU64::new(1_000).unwrap(),
                    limit: nz(per_op * 10),
                }),
                assets: assets.map(NonEmptySet::one),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap()
        .value
}

/// What `who` may do with a transfer of `n` of `a` out of `acct`: the grant chosen, or the
/// refusal. The window charges are checked to be the chosen grant's.
fn chosen(
    q: &mut Q<MemStore>,
    who: Caller,
    acct: AccountId,
    a: AssetId,
    n: u64,
) -> Result<crate::id::GrantId, Refusal> {
    let ctx = q.ctx(who);
    let d = q.ledger.authorise(
        &ctx,
        &crate::eval::Request {
            kind: OperationKind::Transfer,
            account: acct,
            asset: Some(a),
            amount: Some(nz(n)),
            counterparty: None,
        },
    )?;
    let grant = d.used.grant.unwrap();
    assert!(!d.charges.is_empty());
    assert!(
        d.charges
            .iter()
            .all(|c| c.key.grant == grant && c.key.asset == a)
    );
    Ok(grant)
}

/// Bob holds two per-asset grants over Alice's account (the service key caps rollout,
/// 2026-10-28): USD, 1 000 per operation, and EUR, 100. At code generation `generation`.
fn per_asset_grants(
    generation: u64,
) -> (
    Q<MemStore>,
    Caller,
    AccountId,
    crate::id::GrantId,
    crate::id::GrantId,
) {
    let mut q = Q::new();
    q.ledger.set_generation(generation, false);
    let alice = q.person();
    let bob = q.person();
    let acct = q.account(&alice, Custody::Owned);
    q.issue(acct, usd(), 10_000);
    q.issue(acct, eur(), 10_000);
    let g_usd = transfer_grant(
        &mut q,
        alice,
        &bob,
        GrantKind::Delegated,
        acct,
        1_000,
        Some(usd()),
    );
    let g_eur = transfer_grant(
        &mut q,
        alice,
        &bob,
        GrantKind::Delegated,
        acct,
        100,
        Some(eur()),
    );
    (q, bob, acct, g_usd, g_eur)
}

/// `T-003-11` (`AC-G1.2`, `AC-G1.3`, `REQ-AU-1`, `REQ-AG-2`), from code generation
/// [`crate::eval::ASSET_CANDIDATES`] on: each asset's operations are decided under the grant that
/// lists that asset, within that grant's own bounds, and charge that grant's window.
#[test]
fn per_asset_grants_over_one_account_each_decide_their_own_asset() {
    let (mut q, bob, acct, g_usd, g_eur) = per_asset_grants(crate::eval::ASSET_CANDIDATES);
    assert_eq!(
        chosen(&mut q, bob, acct, eur(), 100),
        Ok(g_eur),
        "EUR under the EUR grant"
    );
    assert_eq!(
        chosen(&mut q, bob, acct, usd(), 500),
        Ok(g_usd),
        "USD under the USD grant"
    );
    assert_eq!(
        chosen(&mut q, bob, acct, usd(), 1_000),
        Ok(g_usd),
        "USD at its own bound"
    );
    // Each over its own grant's bound is refused.
    assert_eq!(
        chosen(&mut q, bob, acct, eur(), 101),
        Err(Refusal::BoundExceeded)
    );
    assert_eq!(
        chosen(&mut q, bob, acct, usd(), 1_001),
        Err(Refusal::BoundExceeded)
    );
    // The read that answers the same question, as the caps rollout verifies with it.
    let view = q.ledger.view(q.now).unwrap();
    assert_eq!(view.can_spend(&bob, &acct, &usd(), m(500)), Ok(()));
    assert_eq!(
        view.can_spend(&bob, &acct, &usd(), m(1_001)),
        Err(Refusal::BoundExceeded)
    );
    // And the command: each movement records its own asset's grant.
    let carol = q.person();
    let to = q.account(&carol, Custody::Owned);
    for (a, n, g) in [(usd(), 500, g_usd), (eur(), 100, g_eur)] {
        let ctx = q.ctx(bob);
        let done = q
            .ledger
            .transfer(&ctx, &Transfer::single(acct, to, a, m(n)))
            .unwrap();
        assert_eq!(done.record.authorities[0].grant, Some(g));
    }
}

/// `T-003-11`: a multi-leg transfer out of one account, one USD leg and one EUR leg, at
/// [`crate::eval::ASSET_CANDIDATES`]: each leg is authorised under its own asset's grant, and the
/// legs' bounds are checked per grant and asset (`bounds_together`).
#[test]
fn a_multi_leg_transfer_takes_each_legs_own_asset_grant() {
    let (mut q, bob, acct, g_usd, g_eur) = per_asset_grants(crate::eval::ASSET_CANDIDATES);
    let carol = q.person();
    let to = q.account(&carol, Custody::Owned);
    let leg = |a, n| crate::ledger::transfers::Leg {
        from: acct,
        to,
        asset: a,
        amount: m(n),
    };
    let ctx = q.ctx(bob);
    let done = q
        .ledger
        .transfer(&ctx, &Transfer::unit(vec![leg(usd(), 600), leg(eur(), 60)]))
        .unwrap();
    let grants: StdSet<_> = done.record.authorities.iter().map(|a| a.grant).collect();
    assert_eq!(grants, StdSet::from([Some(g_usd), Some(g_eur)]));
    // Two EUR legs drawing 60 each on the EUR grant exceed its 100 together.
    let ctx = q.ctx(bob);
    assert_eq!(
        q.ledger
            .transfer(&ctx, &Transfer::unit(vec![leg(eur(), 60), leg(eur(), 60)]))
            .map(|_| ()),
        Err(Refusal::BoundExceeded)
    );
}

/// `T-003-11`: in-flight transitions across the switch. A reservation accepted under generation 0
/// is captured, or released, under the generation after it by the same grant, with no window
/// charged again; and at [`crate::eval::ASSET_CANDIDATES`] a USD reservation and its capture are
/// decided under the USD grant, which a narrower EUR grant would have shadowed before.
#[test]
fn reservations_keep_their_asset_grant_across_the_switch() {
    let ops = [
        OperationKind::Reserve,
        OperationKind::Capture,
        OperationKind::Release,
    ];
    let book = |generation| {
        let mut q = Q::new();
        q.ledger.set_generation(generation, false);
        let alice = q.person();
        let bob = q.person();
        let acct = q.account(&alice, Custody::Owned);
        let carol = q.person();
        let to = q.account(&carol, Custody::Owned);
        q.issue(acct, usd(), 10_000);
        q.issue(acct, eur(), 10_000);
        let k = GrantKind::Delegated;
        let g_usd = grant_for(&mut q, alice, &bob, k, acct, &ops, 1_000, Some(usd()));
        let g_eur = grant_for(&mut q, alice, &bob, k, acct, &ops, 100, Some(eur()));
        (q, bob, acct, to, g_usd, g_eur)
    };
    let used = |q: &Q<MemStore>, grant, asset| {
        q.ledger.state().used(&crate::authority::WindowKey {
            grant,
            window: 0,
            asset,
        })
    };

    // Accepted under generation 0, under the EUR grant (the narrowest, and it admits EUR).
    let (mut q, bob, acct, to, _g_usd, g_eur) = book(0);
    let captured = q.reserve(bob, acct, eur(), 50).unwrap();
    let released = q.reserve(bob, acct, eur(), 30).unwrap();
    assert_eq!(used(&q, g_eur, eur()), Minor::new(80));
    // The input switched by a scheduled upgrade, then the generation after it.
    q.ledger.set_generation(0, true);
    let ctx = q.on_op(bob, captured);
    let done = q.ledger.capture(&ctx, to, m(50)).unwrap();
    assert_eq!(done.record.authorities[0].grant, Some(g_eur));
    q.ledger
        .set_generation(crate::eval::ASSET_CANDIDATES, false);
    let ctx = q.on_op(bob, released);
    let done = q.ledger.release(&ctx).unwrap();
    assert_eq!(done.record.authorities[0].grant, Some(g_eur));
    // Neither charged the window again. (A release restores nothing in V0: `REQ-AG-3` serves
    // `US-G4`, beyond it, F-003 plan.)
    assert_eq!(used(&q, g_eur, eur()), Minor::new(80));

    // From the new generation on, a USD reservation and its capture take the USD grant.
    let (mut q, bob, acct, to, g_usd, _g_eur) = book(crate::eval::ASSET_CANDIDATES);
    let op = q.reserve(bob, acct, usd(), 500).unwrap();
    assert_eq!(used(&q, g_usd, usd()), Minor::new(500));
    let ctx = q.on_op(bob, op);
    let done = q.ledger.capture(&ctx, to, m(500)).unwrap();
    assert_eq!(done.record.authorities[0].grant, Some(g_usd));
    assert_eq!(used(&q, g_usd, usd()), Minor::new(500));
    // Before it, the EUR grant decided the USD reservation, and refused it.
    let (mut q, bob, acct, _to, _g_usd, _g_eur) = book(0);
    assert_eq!(
        q.reserve(bob, acct, usd(), 500),
        Err(Refusal::BoundExceeded)
    );
}

/// `T-003-11`: an asset no grant lists is refused, under the first candidate as before, with
/// `ERR-BoundExceeded` (`REQ-AG-2`: the asset is a bound), at either generation.
#[test]
fn an_asset_no_grant_lists_is_refused_at_either_generation() {
    for generation in [0, crate::eval::ASSET_CANDIDATES] {
        let mut q = Q::new();
        q.ledger.set_generation(generation, false);
        let alice = q.person();
        let bob = q.person();
        let acct = q.account(&alice, Custody::Owned);
        q.issue(acct, eur(), 10_000);
        // Two grants over the account, neither listing EUR.
        let narrow = transfer_grant(
            &mut q,
            alice,
            &bob,
            GrantKind::Delegated,
            acct,
            100,
            Some(usd()),
        );
        let wide = transfer_grant(
            &mut q,
            alice,
            &bob,
            GrantKind::Delegated,
            acct,
            1_000,
            Some(usd()),
        );
        assert!(narrow != wide);
        assert_eq!(
            chosen(&mut q, bob, acct, eur(), 1),
            Err(Refusal::BoundExceeded),
            "generation {generation}"
        );
    }
}

/// `T-003-11`: the preference is across the candidate lists. A delegated grant that does not list
/// the asset no longer shadows an operator grant that does (delegated custody, `REQ-AU-5`).
#[test]
fn an_operator_grant_listing_the_asset_is_chosen_over_a_delegated_one_that_does_not() {
    for generation in [0, crate::eval::ASSET_CANDIDATES] {
        let mut q = Q::new();
        q.ledger.set_generation(generation, false);
        let alice = q.person();
        let bob = q.person();
        let acct = q.account(&alice, Custody::Delegated);
        q.issue(acct, usd(), 10_000);
        let op = q.operator();
        let delegated = transfer_grant(
            &mut q,
            alice,
            &bob,
            GrantKind::Delegated,
            acct,
            100,
            Some(eur()),
        );
        let operator = transfer_grant(
            &mut q,
            op,
            &bob,
            GrantKind::Operator,
            acct,
            1_000,
            Some(usd()),
        );
        assert!(delegated != operator);
        let got = chosen(&mut q, bob, acct, usd(), 500);
        if generation >= crate::eval::ASSET_CANDIDATES {
            assert_eq!(got, Ok(operator));
        } else {
            // Before it, the delegated grant came first whatever its assets.
            assert_eq!(got, Err(Refusal::BoundExceeded));
        }
    }
}

/// The generation rule (amendment 0033): an input of a generation before
/// [`crate::eval::ASSET_CANDIDATES`] is decided as that code decided it. The narrowest grant (EUR,
/// the smaller per-operation bound) is the one chosen for every asset, and its asset bound
/// refuses a USD transfer; EUR goes on under it.
#[test]
fn before_asset_candidates_the_narrowest_grant_decides_every_asset() {
    let (mut q, bob, acct, _g_usd, g_eur) = per_asset_grants(crate::eval::ASSET_CANDIDATES - 1);
    assert_eq!(chosen(&mut q, bob, acct, eur(), 100), Ok(g_eur));
    assert_eq!(
        chosen(&mut q, bob, acct, usd(), 500),
        Err(Refusal::BoundExceeded)
    );
    assert_eq!(
        chosen(&mut q, bob, acct, usd(), 1),
        Err(Refusal::BoundExceeded)
    );
    let view = q.ledger.view(q.now).unwrap();
    assert_eq!(
        view.can_spend(&bob, &acct, &usd(), m(500)),
        Err(Refusal::BoundExceeded)
    );
    let carol = q.person();
    let to = q.account(&carol, Custody::Owned);
    assert_eq!(
        q.transfer(bob, acct, to, usd(), 500),
        Err(Refusal::BoundExceeded)
    );
    // An input switched to the next generation by a scheduled upgrade is decided by it.
    q.ledger
        .set_generation(crate::eval::ASSET_CANDIDATES - 1, true);
    assert!(chosen(&mut q, bob, acct, usd(), 500).is_ok());
}

/// Prints the seeded grant ids of a book (`GENESIS_OP` = the 32 hex digits of its genesis op).
#[test]
#[ignore = "a tool, not a test: prints seeded grant ids"]
fn print_seeded_grant_ids() {
    let hex = std::env::var("GENESIS_OP").unwrap();
    let mut b = [0u8; 16];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    let g = OperationId::from_ref(OperationRef(b));
    for (section, name, n) in [
        (crate::chainspec::Section::Grants, "grants", 60u64),
        (crate::chainspec::Section::IssuanceGrants, "issuance", 10),
    ] {
        for ordinal in 0..n {
            std::println!(
                "{name}\t{ordinal}\t{}",
                crate::chainspec::grant_of(&g, section, ordinal).to_did()
            );
        }
    }
}
