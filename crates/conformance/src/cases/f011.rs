// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-011` — Queries and statements: its registered cases (`F-015` §6, §7).
//!
//! Every case drives `ledger-core`'s commands and queries through [`ServicePort`] as principals.
//! The two **store** cases reach below the queries: `INV-16`'s calls the service store's read
//! methods directly with a scope narrowed to one account, and `INV-5`'s sums the commitments of
//! the service book's leaf store with no opening at all.
//!
//! Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::authority::{AuthorityKind, GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{Ctx, Founded, GrantRequest, OpenAccount, Transfer};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::query::{
    AuthorisedScope, BalanceKind, Direction, Movement, MovementFilter, MovementStatus,
    MovementsRequest, QueryStore, StatementRequest, Subject as Whose, TimeRange,
};
use ledger_core::set::NonEmptySet;
use ledger_kernel::posting::{OperationRef, PostingKind};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::{Harness, ServicePort, acknowledgement};
use crate::shape::expect_refusal;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F011,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-011`'s registered cases.
pub const CASES: &[Case] = &[
    // Invariants (SPEC §9), at the layer each case exercises.
    // The check on commitments needs no opening, so it runs in the store (F-011 §8): after every
    // unit of a service workload, the leaf store alone sums to zero per currency and layer.
    Case::run(
        "INV-5",
        "inv_5_the_trial_balance_is_exactly_zero_after_every_unit__store",
        Layer::Store,
        Feature::F011,
        Port::Service,
        &["NFR-9"],
        inv_5_the_trial_balance_is_exactly_zero_after_every_unit__store,
    ),
    service_case!(
        "INV-6",
        inv_6_reported_balances_equal_balances_rederived_from_entries__service,
        &["NFR-4"]
    ),
    service_case!(
        "INV-8",
        inv_8_reads_come_only_from_the_book_and_no_writable_copy_is_exported__service,
        &[]
    ),
    // SPEC §9's target layer. Its V0 column says service (amendment 0006 item 2): this case is
    // the target, reported but not required. The store is called
    // directly, with a scope narrowed to one account (F-011 §2).
    Case::run(
        "INV-16",
        "inv_16_an_unauthorised_read_is_unknown_account__store",
        Layer::Store,
        Feature::F011,
        Port::Service,
        &["ERR-UnknownAccount"],
        inv_16_an_unauthorised_read_is_unknown_account__store,
    ),
    service_case!(
        "INV-16",
        inv_16_an_unauthorised_read_is_unknown_account__service,
        &["ERR-UnknownAccount", "REQ-AR-6"]
    ),
    service_case!(
        "INV-20",
        inv_20_every_entry_is_visible_to_the_accounts_authority_holders__service,
        &["REQ-AR-6"]
    ),
    // Errors (SPEC §10).
    service_case!("ERR-QueryUnbounded", err_query_unbounded__service, &[]),
    // Acceptance criteria of the V0 stories.
    service_case!("AC-H1.1", ac_h1_1__service, &["REQ-BA-5"]),
    service_case!("AC-H1.2", ac_h1_2__service, &["REQ-BA-2", "REQ-QR-2"]),
    service_case!("AC-H2.1", ac_h2_1__service, &["REQ-PO-2", "REQ-QR-5"]),
    service_case!("AC-H2.2", ac_h2_2__service, &["REQ-QR-1"]),
    service_case!("AC-H2.3", ac_h2_3__service, &["REQ-PO-1"]),
    service_case!("AC-H3.1", ac_h3_1__service, &["INV-6"]),
    service_case!("AC-H3.2", ac_h3_2__service, &["REQ-QR-2"]),
    service_case!("AC-H4.1", ac_h4_1__service, &["REQ-QR-3"]),
    service_case!("AC-H5.1", ac_h5_1__service, &["REQ-QR-4", "INV-20"]),
    service_case!("AC-I1.2", ac_i1_2__service, &["REQ-PV-1", "INV-16"]),
    service_case!("AC-M4.1", ac_m4_1__service, &["INV-8"]),
];

// ---------------------------------------------------------------------------
// A world of principals over the service port.

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn eur() -> AssetId {
    AssetId::parse("EUR").unwrap()
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

/// The deadline every transfer of these cases carries: the in-process deployment declares no
/// default for transfers.
const DEADLINE: Time = 1_000_000;

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
        let mut w = World { s, f, n: 0, now };
        for a in [usd(), eur()] {
            w.issuer(a);
        }
        w
    }

    fn op(&mut self) -> OperationId {
        self.n += 1;
        let mut b = [0xF1; 16];
        b[8..].copy_from_slice(&self.n.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    /// A context one time unit after the last, so time ranges have something to select.
    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.now += 1;
        Ctx {
            caller,
            op: self.op(),
            now: self.now,
        }
    }

    fn on(&mut self, caller: Caller, op: OperationId) -> Ctx {
        self.now += 1;
        Ctx {
            caller,
            op,
            now: self.now,
        }
    }

    fn operator(&self) -> Caller {
        Caller::new(self.f.operator, self.f.operator_credential)
    }

    fn issuer(&mut self, a: AssetId) {
        let op = self.operator();
        let ctx = self.ctx(op);
        self.s
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
                        length: NonZeroU64::new(1_000_000).unwrap(),
                        limit: nz(1_000_000_000),
                    }),
                    assets: Some(NonEmptySet::one(a)),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
    }

    fn person(&mut self) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .s
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![4]))
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

    fn issue(&mut self, to: AccountId, a: AssetId, amount: u64) {
        let ctx = self.ctx(self.operator());
        self.s.issue(&ctx, a, to, m(amount)).unwrap();
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
        self.s
            .transfer(
                &ctx,
                &Transfer::single(from, to, a, m(amount)).until(DEADLINE),
            )
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
        self.s.reserve(&ctx, payer, a, m(amount)).map(|_| ctx.op)
    }

    fn capture(&mut self, by: Caller, op: OperationId, to: AccountId, amount: u64) {
        let ctx = self.on(by, op);
        self.s.capture(&ctx, to, m(amount)).unwrap();
    }

    fn release(&mut self, by: Caller, op: OperationId) {
        let ctx = self.on(by, op);
        self.s.release(&ctx).unwrap();
    }

    /// A delegated grant from `owner` to `to` over `account`, for `ops`.
    fn delegate(&mut self, owner: Caller, to: Caller, account: AccountId, ops: &[OperationKind]) {
        let ctx = self.ctx(owner);
        self.s
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Delegated,
                    principal: to.principal,
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

    /// Operator authority to transfer out of every delegated-custody account.
    fn operator_transfers(&mut self) {
        let op = self.operator();
        let ctx = self.ctx(op);
        self.s
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Operator,
                    principal: op.principal,
                    subject: Subject::DelegatedAccounts,
                    ops: NonEmptySet::one(OperationKind::Transfer),
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

    /// Every page of a list, following the cursor.
    fn all(
        &self,
        caller: &Caller,
        subject: Whose,
        filter: MovementFilter,
        limit: u32,
    ) -> Vec<Movement> {
        let mut req = MovementsRequest {
            subject,
            filter,
            limit: Some(limit),
            cursor: None,
        };
        let mut out = Vec::new();
        loop {
            let page = self.s.query_movements(caller, &req, self.now).unwrap();
            out.extend(page.movements);
            match page.next {
                Some(c) => req.cursor = Some(c),
                None => return out,
            }
        }
    }

    fn statement(
        &self,
        caller: &Caller,
        account: AccountId,
        a: AssetId,
        from: Time,
        to: Time,
    ) -> Result<ledger_core::query::Statement, Refusal> {
        self.s.query_statement(
            caller,
            &StatementRequest {
                account,
                asset: a,
                from: Some(from),
                to: Some(to),
                as_of: None,
            },
            self.now,
        )
    }
}

/// People with accounts, and a deterministic mixed workload over them.
struct Crowd {
    people: Vec<(Caller, Vec<AccountId>)>,
    open: Vec<(Caller, OperationId)>,
    seed: u64,
}

impl Crowd {
    fn new(w: &mut World, people: usize, accounts: usize) -> Crowd {
        let mut c = Crowd {
            people: Vec::new(),
            open: Vec::new(),
            seed: 0x5EED_F011,
        };
        for _ in 0..people {
            let p = w.person();
            let accts = (0..accounts)
                .map(|_| w.account(&p, Custody::Owned))
                .collect();
            c.people.push((p, accts));
        }
        c
    }

    fn below(&mut self, n: u64) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed % n
    }

    fn accounts(&self) -> Vec<AccountId> {
        self.people.iter().flat_map(|(_, a)| a.clone()).collect()
    }

    /// `n` commands: issue, transfer, reserve, capture, release. Refusals change nothing.
    fn run(&mut self, w: &mut World, n: usize, after_each: &mut dyn FnMut(&World)) {
        for _ in 0..n {
            let who_at = self.below(self.people.len() as u64) as usize;
            let (who, accts) = self.people[who_at].clone();
            let from = accts[self.below(accts.len() as u64) as usize];
            let a = if self.below(2) == 0 { usd() } else { eur() };
            let amount = 1 + self.below(30);
            let all = self.accounts();
            match self.below(6) {
                0 | 1 => w.issue(from, a, amount * 3),
                2 | 3 => {
                    let to = all[self.below(all.len() as u64) as usize];
                    let _ = w.transfer(who, from, to, a, amount);
                }
                4 => {
                    if let Ok(op) = w.reserve(who, from, a, amount) {
                        self.open.push((who, op));
                    }
                }
                _ => {
                    if !self.open.is_empty() {
                        let at = self.below(self.open.len() as u64) as usize;
                        let (by, op) = self.open.remove(at);
                        if self.below(2) == 0 {
                            let to = all[self.below(all.len() as u64) as usize];
                            w.capture(by, op, to, 1);
                        } else {
                            w.release(by, op);
                        }
                    }
                }
            }
            after_each(w);
        }
    }
}

fn check(cond: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if cond { Ok(()) } else { Err(why()) }
}

fn outcome(r: Result<(), String>) -> Outcome {
    match r {
        Ok(()) => Outcome::Pass,
        Err(why) => Outcome::Fail(why),
    }
}

// ---------------------------------------------------------------------------
// Invariants.

fn inv_5_the_trial_balance_is_exactly_zero_after_every_unit__store(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let mut crowd = Crowd::new(&mut w, 3, 2);
    let mut failures: Vec<String> = Vec::new();
    crowd.run(&mut w, 60, &mut |w| {
        let t = w.s.store_trial_balance();
        if !t.is_zero() || t.accounts == 0 {
            failures.push(format!("store: {t:?}"));
        }
        let q = w.s.query_trial_balance(&w.operator(), None, w.now).unwrap();
        if !q.is_zero() {
            failures.push(format!("query: {q:?}"));
        }
    });
    outcome(check(failures.is_empty(), || failures.join("; ")))
}

fn inv_6_reported_balances_equal_balances_rederived_from_entries__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let mut crowd = Crowd::new(&mut w, 3, 2);
    crowd.run(&mut w, 80, &mut |_| {});
    let r = (|| {
        check(w.s.reconcile().is_empty(), || {
            format!("drift: {:?}", w.s.reconcile())
        })?;
        for (p, accts) in &crowd.people {
            for a in accts {
                for asset in [usd(), eur()] {
                    let row =
                        w.s.query_balances(p, Whose::Account(*a), Some(asset), w.now)
                            .unwrap()
                            .rows[0];
                    // Re-derived from entries alone: every entry, from the first posting on.
                    let s = w.statement(p, *a, asset, 0, w.now + 1).unwrap();
                    check(
                        s.opening == Default::default()
                            && s.closing.available == row.available
                            && s.closing.reserved == row.reserved,
                        || format!("{a:?} {asset:?}: {row:?} vs {:?}", s.closing),
                    )?;
                }
            }
        }
        Ok(())
    })();
    outcome(r)
}

fn inv_8_reads_come_only_from_the_book_and_no_writable_copy_is_exported__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let a = w.account(&alice, Custody::Owned);
    w.issue(a, usd(), 100);
    let _ = w.reserve(alice, a, usd(), 30).unwrap();
    let root = w.s.book_root();
    let r = (|| {
        // The balance read is the book's: the kernel's settled net and the index.
        let mut got =
            w.s.query_balances(&alice, Whose::Account(a), Some(usd()), w.now)
                .unwrap();
        let book = w.s.balance_view(&a, &usd()).unwrap();
        check(
            got.rows[0].available == book.available && got.rows[0].reserved == book.reserved,
            || format!("{got:?} vs the book's {book:?}"),
        )?;
        // What a caller holds is a copy: changing it changes nothing in the book.
        got.rows[0].available = 1_000_000;
        got.rows[0].total = 1_000_000;
        let again =
            w.s.query_balances(&alice, Whose::Account(a), Some(usd()), w.now)
                .unwrap();
        check(again.rows[0].available == 70, || format!("{again:?}"))?;
        check(w.s.book_root() == root, || "a read changed the book".into())?;
        Ok(())
    })();
    outcome(r)
}

fn inv_16_an_unauthorised_read_is_unknown_account__store(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let mine = w.account(&alice, Custody::Owned);
    let theirs = w.account(&bob, Custody::Owned);
    w.issue(theirs, usd(), 100);
    w.transfer(bob, theirs, mine, usd(), 10).unwrap();
    let _ = w.reserve(bob, theirs, usd(), 5).unwrap();
    // The store, called directly, with a scope narrowed to one account.
    let state = w.s.state();
    let scope = AuthorisedScope::narrowed([mine]);
    let missing = AccountId::mint(&OperationId::from_ref(OperationRef([0xAA; 16])), 0);
    let r = (|| {
        for other in [theirs, missing] {
            let refused = |r: Result<(), Refusal>, what: &str| {
                check(r == Err(Refusal::UnknownAccount), || {
                    format!("{what} of {other:?}: {r:?}")
                })
            };
            refused(state.account_in(&scope, &other).map(|_| ()), "account")?;
            refused(
                state.reserved_in(&scope, &other, &usd()).map(|_| ()),
                "reserved",
            )?;
            refused(
                state
                    .postings_in(&scope, &other, 0, u64::MAX, 10)
                    .map(|_| ()),
                "postings",
            )?;
            refused(
                state
                    .position_postings_in(&scope, &other, &usd(), 0, u64::MAX, 10)
                    .map(|_| ()),
                "position postings",
            )?;
            refused(
                state
                    .balance_before_in(&scope, &other, &usd(), u64::MAX)
                    .map(|_| ()),
                "balance before",
            )?;
            refused(
                state.balance_now_in(&scope, &other, &usd()).map(|_| ()),
                "balance now",
            )?;
        }
        // The account in scope answers.
        check(
            state
                .balance_now_in(&scope, &mine, &usd())
                .map(|s| s.available)
                == Ok(10),
            || "the account in scope does not answer".into(),
        )
    })();
    outcome(r)
}

/// Every query a stranger can ask of `a`, and of its owner's identity: each must be unknown.
fn stranger_reaches_nothing(
    w: &World,
    stranger: &Caller,
    owner: &Caller,
    a: AccountId,
    op: OperationId,
) -> Result<(), String> {
    let unknown = |r: Result<(), Refusal>, what: &str, want: Refusal| {
        check(r == Err(want), || format!("{what}: {r:?}"))
    };
    let s = &w.s;
    unknown(
        s.query_balances(stranger, Whose::Account(a), None, w.now)
            .map(|_| ()),
        "balances of the account",
        Refusal::UnknownAccount,
    )?;
    unknown(
        s.query_balances(stranger, Whose::Identity(owner.principal), None, w.now)
            .map(|_| ()),
        "balances of the identity",
        Refusal::UnknownAccount,
    )?;
    unknown(
        s.query_movements(
            stranger,
            &MovementsRequest {
                subject: Whose::Account(a),
                filter: MovementFilter::default(),
                limit: Some(10),
                cursor: None,
            },
            w.now,
        )
        .map(|_| ()),
        "movements",
        Refusal::UnknownAccount,
    )?;
    unknown(
        w.statement(stranger, a, usd(), 0, w.now + 1).map(|_| ()),
        "statement",
        Refusal::UnknownAccount,
    )?;
    unknown(
        s.can_spend(stranger, &a, &usd(), m(1), w.now),
        "can_spend",
        Refusal::UnknownAccount,
    )?;
    unknown(
        s.query_operation(stranger, &op, w.now).map(|_| ()),
        "operation",
        Refusal::UnknownOperation,
    )
}

fn inv_16_an_unauthorised_read_is_unknown_account__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let a = w.account(&alice, Custody::Owned);
    let a2 = w.account(&alice, Custody::Owned);
    w.issue(a, usd(), 100);
    w.issue(a2, usd(), 100);
    let op = w.reserve(alice, a, usd(), 10).unwrap();
    let r = (|| {
        stranger_reaches_nothing(&w, &bob, &alice, a, op)?;
        stranger_reaches_nothing(&w, &bob, &alice, a2, op)?;
        Ok(())
    })();
    if let Err(why) = r {
        return Outcome::Fail(why);
    }
    // A grant over one account reaches that account, never the rest of the identity.
    w.delegate(alice, bob, a, &[OperationKind::Transfer]);
    let rows =
        w.s.query_balances(&bob, Whose::Identity(alice.principal), None, w.now)
            .unwrap()
            .rows;
    let r = check(
        !rows.is_empty() && rows.iter().all(|r| r.account == a),
        || format!("{rows:?}"),
    )
    .and_then(|_| {
        check(
            w.s.query_balances(&bob, Whose::Account(a2), None, w.now)
                == Err(Refusal::UnknownAccount),
            || "the grant reached another account".into(),
        )
    });
    outcome(r)
}

fn inv_20_every_entry_is_visible_to_the_accounts_authority_holders__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    w.operator_transfers();
    let alice = w.person();
    let bob = w.person();
    let carol = w.person();
    let a = w.account(&alice, Custody::Delegated);
    let b = w.account(&bob, Custody::Owned);
    w.issue(a, usd(), 100);
    w.transfer(alice, a, b, usd(), 10).unwrap();
    let op = w.operator();
    w.transfer(op, a, b, usd(), 20).unwrap();
    w.delegate(alice, carol, a, &[OperationKind::Transfer]);
    w.transfer(carol, a, b, usd(), 5).unwrap();
    let r = (|| {
        let journal: Vec<u64> =
            w.s.state()
                .journal()
                .iter()
                .filter(|e| e.touches(&a))
                .map(|e| e.seq)
                .collect();
        // The owner and the delegate see every entry against the account, and who made it.
        for holder in [alice, carol] {
            let got = w.all(&holder, Whose::Account(a), MovementFilter::default(), 2);
            let seqs: Vec<u64> = got.iter().map(|m| m.seq).collect();
            check(seqs == journal, || {
                format!("{holder:?} sees {seqs:?}, the book holds {journal:?}")
            })?;
            let kinds: Vec<AuthorityKind> = got
                .iter()
                .filter(|m| m.kind == PostingKind::Transfer)
                .map(|m| m.authorities[0].kind)
                .collect();
            check(
                kinds
                    == vec![
                        AuthorityKind::Owner,
                        AuthorityKind::Operator,
                        AuthorityKind::Delegated,
                    ],
                || format!("{kinds:?}"),
            )?;
            check(got.iter().all(|m| !m.authorities.is_empty()), || {
                "an entry with no authority".into()
            })?;
        }
        Ok(())
    })();
    outcome(r)
}

// ---------------------------------------------------------------------------
// Errors.

fn err_query_unbounded__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let a = w.account(&alice, Custody::Owned);
    w.issue(a, usd(), 10);
    let unpaged = MovementsRequest {
        subject: Whose::Account(a),
        filter: MovementFilter::default(),
        limit: None,
        cursor: None,
    };
    let open_ended = StatementRequest {
        account: a,
        asset: usd(),
        from: Some(0),
        to: None,
        as_of: None,
    };
    match w.s.query_statement(&alice, &open_ended, w.now) {
        Err(Refusal::QueryUnbounded) => {}
        other => return Outcome::Fail(format!("an open-ended statement: {other:?}")),
    }
    expect_refusal(
        w.s.query_movements(&alice, &unpaged, w.now),
        "ERR-QueryUnbounded",
    )
}

// ---------------------------------------------------------------------------
// Acceptance criteria.

fn ac_h1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let accts: Vec<AccountId> = (0..3).map(|_| w.account(&alice, Custody::Owned)).collect();
    for (i, a) in accts.iter().enumerate() {
        w.issue(*a, usd(), 10 + i as u64);
        w.issue(*a, eur(), 20 + i as u64);
    }
    let r =
        w.s.query_balances(&alice, Whose::Identity(alice.principal), None, w.now)
            .unwrap();
    outcome((|| {
        check(r.rows.len() == 6, || {
            format!("{} rows: {r:?}", r.rows.len())
        })?;
        for (i, a) in accts.iter().enumerate() {
            for (asset, base) in [(usd(), 10), (eur(), 20)] {
                let row = r.rows.iter().find(|x| x.account == *a && x.asset == asset);
                check(row.is_some_and(|x| x.available == base + i as i128), || {
                    format!("{a:?} {asset:?}: {row:?}")
                })?;
            }
        }
        Ok(())
    })())
}

fn ac_h1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let a = w.account(&alice, Custody::Owned);
    let b = w.account(&alice, Custody::Owned);
    w.issue(a, usd(), 100);
    let mut open = Vec::new();
    let mut r = Ok(());
    // Writers between reads: every read is one view, whatever landed before it.
    for i in 0..12u64 {
        if i % 3 == 2 {
            if let Some(op) = open.pop() {
                w.capture(alice, op, b, 1);
            }
        } else if let Ok(op) = w.reserve(alice, a, usd(), 1 + i) {
            open.push(op);
        }
        let rows =
            w.s.query_balances(&alice, Whose::Identity(alice.principal), Some(usd()), w.now)
                .unwrap()
                .rows;
        r = r.and_then(|_| {
            let total: i128 = rows.iter().map(|x| x.total).sum();
            check(total == 100, || {
                format!("the identity's total moved: {rows:?}")
            })?;
            for x in &rows {
                check(x.available + x.reserved + x.encumbered == x.total, || {
                    format!("{x:?}")
                })?;
                let avail = u64::try_from(x.available).unwrap();
                check(
                    w.s.can_spend(&alice, &x.account, &usd(), m(avail + 1), w.now)
                        == Err(Refusal::InsufficientAvailable),
                    || format!("the available shown is not the one checked: {x:?}"),
                )?;
                if avail > 0 {
                    check(
                        w.s.can_spend(&alice, &x.account, &usd(), m(avail), w.now) == Ok(()),
                        || format!("{x:?}"),
                    )?;
                }
            }
            Ok(())
        });
    }
    outcome(r)
}

fn ac_h2_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let mut crowd = Crowd::new(&mut w, 2, 2);
    crowd.run(&mut w, 50, &mut |_| {});
    let (alice, _) = crowd.people[0].clone();
    let subject = Whose::Identity(alice.principal);
    let view = w.s.view_sequence();
    // The whole list at this view, in one page.
    let whole =
        w.s.query_movements(
            &alice,
            &MovementsRequest {
                subject,
                filter: MovementFilter::default(),
                limit: Some(1_000),
                cursor: None,
            },
            w.now,
        )
        .unwrap();
    let mut req = MovementsRequest {
        subject,
        filter: MovementFilter::default(),
        limit: Some(3),
        cursor: None,
    };
    let mut paged: Vec<Movement> = Vec::new();
    let mut r = Ok(());
    loop {
        let page = w.s.query_movements(&alice, &req, w.now).unwrap();
        r = r.and_then(|_| {
            check(page.view == view, || {
                format!("page at {} not {view}", page.view)
            })
        });
        paged.extend(page.movements);
        let Some(c) = page.next else { break };
        // Writers append between pages.
        crowd.run(&mut w, 2, &mut |_| {});
        req.cursor = Some(c);
    }
    outcome(r.and_then(|_| {
        check(whole.next.is_none(), || "the whole list did not fit".into())?;
        check(paged == whole.movements, || {
            format!(
                "{} paged, {} at the view",
                paged.len(),
                whole.movements.len()
            )
        })?;
        check(paged.windows(2).all(|p| p[0].seq <= p[1].seq), || {
            "not in sequence order".into()
        })
    }))
}

fn ac_h2_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let mut crowd = Crowd::new(&mut w, 3, 2);
    crowd.run(&mut w, 80, &mut |_| {});
    let (alice, accts) = crowd.people[0].clone();
    let other = crowd.people[1].1[0];
    let subject = Whose::Identity(alice.principal);
    let everything = w.all(&alice, subject, MovementFilter::default(), 50);
    let mid = w.now / 2;
    let filters = [
        MovementFilter {
            account: Some(accts[1]),
            ..Default::default()
        },
        MovementFilter {
            asset: Some(eur()),
            ..Default::default()
        },
        MovementFilter {
            direction: Some(Direction::Credit),
            ..Default::default()
        },
        MovementFilter {
            counterparty: Some(other),
            ..Default::default()
        },
        MovementFilter {
            time: Some(TimeRange {
                from: mid,
                to: w.now,
            }),
            ..Default::default()
        },
    ];
    let mut r = Ok(());
    let mut nonempty = 0;
    for f in &filters {
        let want: Vec<Movement> = everything
            .iter()
            .filter(|m| {
                f.account.is_none_or(|a| m.account == a)
                    && f.asset.is_none_or(|a| m.asset == a)
                    && f.direction.is_none_or(|d| m.direction == d)
                    && f.counterparty.is_none_or(|c| m.counterparty == Some(c))
                    && f.time.is_none_or(|t| m.at >= t.from && m.at < t.to)
            })
            .cloned()
            .collect();
        let got = w.all(&alice, subject, *f, 4);
        nonempty += usize::from(!want.is_empty());
        r = r.and_then(|_| {
            check(got == want, || {
                format!("{f:?}: {} vs {}", got.len(), want.len())
            })
        });
    }
    outcome(r.and_then(|_| {
        check(nonempty == filters.len(), || {
            "a filter selected nothing".into()
        })
    }))
}

fn ac_h2_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let a = w.account(&alice, Custody::Owned);
    let b = w.account(&bob, Custody::Owned);
    w.issue(a, usd(), 100);
    w.transfer(alice, a, b, usd(), 5).unwrap();
    let r1 = w.reserve(alice, a, usd(), 10).unwrap();
    w.capture(alice, r1, b, 4);
    let r2 = w.reserve(alice, a, usd(), 10).unwrap();
    w.release(alice, r2);
    let got = w.all(&alice, Whose::Account(a), MovementFilter::default(), 10);
    let mut kinds: Vec<PostingKind> = got.iter().map(|m| m.kind).collect();
    kinds.dedup();
    outcome((|| {
        check(
            kinds
                == vec![
                    PostingKind::Issue,
                    PostingKind::Transfer,
                    PostingKind::Reserve,
                    PostingKind::Capture,
                    PostingKind::Reserve,
                    PostingKind::Release,
                ],
            || format!("{kinds:?}"),
        )?;
        for m in &got {
            check(m.status == MovementStatus::of(m.kind), || format!("{m:?}"))?;
        }
        check(
            got.iter()
                .filter(|m| m.kind == PostingKind::Reserve)
                .all(|m| m.status == MovementStatus::Held),
            || "a reserve is not held".into(),
        )
    })())
}

fn ac_h3_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let mut crowd = Crowd::new(&mut w, 2, 1);
    crowd.run(&mut w, 100, &mut |_| {});
    let (alice, accts) = crowd.people[0].clone();
    let a = accts[0];
    let mut r = Ok(());
    let mut rng = 0x3A1u64;
    for _ in 0..40 {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let from = (rng >> 33) % (w.now + 1);
        let to = from + (rng >> 17) % (w.now + 2 - from);
        for asset in [usd(), eur()] {
            let s = w.statement(&alice, a, asset, from, to).unwrap();
            let mut closing = s.opening;
            for m in &s.movements {
                match m.balance {
                    BalanceKind::Available => closing.available += m.delta(),
                    BalanceKind::Reserved => closing.reserved += m.delta(),
                    BalanceKind::Encumbered => closing.encumbered += m.delta(),
                }
            }
            r = r.and_then(|_| {
                check(closing == s.closing, || {
                    format!(
                        "[{from}, {to}) {asset:?}: {:?} + movements != {:?}",
                        s.opening, s.closing
                    )
                })
            });
        }
    }
    // The whole history closes on the balances shown.
    for asset in [usd(), eur()] {
        let s = w.statement(&alice, a, asset, 0, w.now + 1).unwrap();
        let row =
            w.s.query_balances(&alice, Whose::Account(a), Some(asset), w.now)
                .unwrap()
                .rows[0];
        r = r.and_then(|_| {
            check(
                s.closing.available == row.available && s.closing.reserved == row.reserved,
                || format!("{asset:?}: {:?} vs {row:?}", s.closing),
            )
        });
    }
    outcome(r)
}

fn ac_h3_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let a = w.account(&alice, Custody::Owned);
    let b = w.account(&bob, Custody::Owned);
    w.issue(a, usd(), 100);
    for n in 1..5 {
        w.transfer(alice, a, b, usd(), n).unwrap();
    }
    let end = w.now;
    w.transfer(alice, a, b, usd(), 7).unwrap();
    let first = w.statement(&alice, a, usd(), 0, end).unwrap();
    // More postings, then the same request.
    for n in 1..4 {
        w.transfer(alice, a, b, usd(), n).unwrap();
    }
    let second = w.statement(&alice, a, usd(), 0, end).unwrap();
    outcome((|| {
        check(first.closed, || "the period has ended in the book".into())?;
        check(first.to_bytes() == second.to_bytes(), || {
            format!("{first:?}\n!=\n{second:?}")
        })
    })())
}

fn ac_h4_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let mut crowd = Crowd::new(&mut w, 2, 2);
    crowd.run(&mut w, 30, &mut |_| {});
    let stranger = w.person();
    let fingerprint = |w: &World| {
        let s = w.s.state();
        (
            w.s.book_root(),
            w.s.view_sequence(),
            s.journal().len(),
            s.operations().all().count(),
            s.reservations().iter().count(),
            s.operations().outbox().count(),
        )
    };
    let before = fingerprint(&w);
    let accts = crowd.accounts();
    let callers: Vec<Caller> = crowd
        .people
        .iter()
        .map(|p| p.0)
        .chain([stranger, w.operator()])
        .collect();
    let mut r = Ok(());
    for i in 0..400usize {
        let c = callers[i % callers.len()];
        let a = accts[i % accts.len()];
        let asset = if i % 2 == 0 { usd() } else { eur() };
        let _ = match i % 7 {
            0 => {
                w.s.query_balances(&c, Whose::Account(a), None, w.now)
                    .map(|_| ())
            }
            1 => {
                w.s.query_balances(&c, Whose::Identity(c.principal), Some(asset), w.now)
                    .map(|_| ())
            }
            2 => {
                w.s.query_movements(
                    &c,
                    &MovementsRequest {
                        subject: Whose::Account(a),
                        filter: MovementFilter::default(),
                        limit: Some(5),
                        cursor: None,
                    },
                    w.now,
                )
                .map(|_| ())
            }
            3 => w.statement(&c, a, asset, 0, w.now).map(|_| ()),
            4 => w.s.can_spend(&c, &a, &asset, m(i as u64), w.now),
            5 => w.s.query_trial_balance(&c, None, w.now).map(|_| ()),
            _ => w.s.query_assets(&c, w.now).map(|_| ()),
        };
        r = r.and_then(|_| check(fingerprint(&w) == before, || format!("query {i} wrote")));
    }
    outcome(r)
}

fn ac_h5_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.operator_transfers();
    let alice = w.person();
    let bob = w.person();
    let a = w.account(&alice, Custody::Delegated);
    let b = w.account(&bob, Custody::Owned);
    w.issue(a, usd(), 100);
    let op = w.operator();
    w.transfer(op, a, b, usd(), 25).unwrap();
    let got = w.all(&alice, Whose::Account(a), MovementFilter::default(), 10);
    let entry = got.iter().find(|m| m.kind == PostingKind::Transfer);
    outcome(check(
        entry.is_some_and(|mv| {
            mv.direction == Direction::Debit
                && mv.amount == m(25)
                && mv.counterparty == Some(b)
                && mv.authorities.len() == 1
                && mv.authorities[0].kind == AuthorityKind::Operator
                && mv.authorities[0].principal == op.principal
        }),
        || format!("{got:?}"),
    ))
}

fn ac_i1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let mallory = w.person();
    let a = w.account(&alice, Custody::Delegated);
    let theirs = w.account(&mallory, Custody::Owned);
    w.issue(a, usd(), 100);
    w.issue(theirs, usd(), 100);
    w.transfer(alice, a, theirs, usd(), 10).unwrap();
    let op = w.reserve(alice, a, usd(), 10).unwrap();
    // Mallory is Alice's counterparty and holds value in the same book: still nothing of Alice's.
    let r = stranger_reaches_nothing(&w, &mallory, &alice, a, op).and_then(|_| {
        // The operator holds no grant over this account either.
        stranger_reaches_nothing(&w, &w.operator(), &alice, a, op)
    });
    outcome(r)
}

fn ac_m4_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let bob = w.person();
    let a = w.account(&alice, Custody::Owned);
    let b = w.account(&bob, Custody::Owned);
    w.issue(a, usd(), 100);
    // The integrating party caches what it read.
    let cached =
        w.s.query_balances(&alice, Whose::Account(a), Some(usd()), w.now)
            .unwrap()
            .rows[0]
            .available;
    // Then the book moves on without it.
    let _ = w.reserve(alice, a, usd(), 60).unwrap();
    let cached = u64::try_from(cached).unwrap();
    outcome((|| {
        // A decision is taken on aledgent's number, never on the cache.
        check(
            w.s.can_spend(&alice, &a, &usd(), m(cached), w.now)
                == Err(Refusal::InsufficientAvailable),
            || "can_spend followed the cache".into(),
        )?;
        let spend = w.transfer(alice, a, b, usd(), cached);
        check(spend == Err(Refusal::InsufficientAvailable), || {
            format!("a transfer of the cached amount: {spend:?}")
        })?;
        let now =
            w.s.query_balances(&alice, Whose::Account(a), Some(usd()), w.now)
                .unwrap()
                .rows[0];
        let book = w.s.balance_view(&a, &usd()).unwrap();
        check(
            now.available == 40 && now.available == book.available,
            || format!("{now:?} vs {book:?}"),
        )
    })())
}
