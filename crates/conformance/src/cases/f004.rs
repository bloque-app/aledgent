// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-004` — Reservation model: its registered cases (`F-015` §6, §7).
//!
//! - The **kernel** case bypasses `ledger-core` entirely: hand-built, validly signed transfers
//!   through [`KernelPort`](crate::harness::KernelPort) reserve into flagged holding positions and
//!   then try to overdraw the payer, or empty a holding past what it holds. CipherClerk itself
//!   refuses (`INV-3` at the layer SPEC §9 names).
//! - **Service** cases drive `ledger-core`'s `reserve`, `capture` and `release` through
//!   [`ServicePort`] as principals.
//!
//! Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].
//!
//! The half of `AC-C2.1` that says a repeated capture returns the original outcome needs the
//! operation lifecycle, and is registered in `f007.rs`, pending on `F-007`.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use cipher_clerk::{Account, AccountFlags, Direction, EventStatus, Layer as KLayer};
use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, IdentityId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{Ctx, Founded, GrantRequest, OpenAccount};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::reservation::{BalanceView, Closing, Status};
use ledger_core::set::NonEmptySet;
use ledger_core::state::Command;
use ledger_kernel::posting::{OperationRef, PostingKind, PostingMeta};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::cases::f001::{create, kernel, raw, submit};
use crate::harness::{Harness, ServicePort, acknowledgement};
use crate::shape::expect_refusal;

use Direction::{Credit, Debit};

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F004,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-004`'s registered cases.
pub const CASES: &[Case] = &[
    // Amendment 0013 (R2): open reservations, aggregated and itemised; T-004-09.
    Case::pending(
        "AC-C6.1",
        "ac_c6_1_open_reservations_aggregated_and_itemised_in_one_view__service",
        Layer::Service,
        Feature::F004,
        &["REQ-QR-7", "REQ-QR-2"],
    ),
    Case::pending(
        "AC-C6.2",
        "ac_c6_2_a_closed_reservation_is_not_open__service",
        Layer::Service,
        Feature::F004,
        &["REQ-QR-7"],
    ),
    Case::pending(
        "AC-C6.3",
        "ac_c6_3_another_principals_reservations_are_not_seen__service",
        Layer::Service,
        Feature::F004,
        &["REQ-QR-7", "REQ-QR-4", "INV-16"],
    ),
    // Invariants (SPEC §9), at the layer each case exercises.
    // F-001's kernel case covers a flagged account; this one bypasses ledger-core and overdraws
    // a payer with open reservations.
    Case::run(
        "INV-3",
        "inv_3_customer_never_negative_while_reservations_are_open__kernel",
        Layer::Kernel,
        Feature::F004,
        Port::Kernel,
        &["REQ-KER-4", "REQ-BA-4"],
        inv_3_customer_never_negative_while_reservations_are_open__kernel,
    ),
    // The kernel's check covers every balance kind the book uses: reservations, captures and
    // releases never put anything on the Pending or Encumbrance kinds (DEC-2, F-004 §2). Read from
    // the kernel's own leaves after service commands.
    Case::run(
        "INV-3",
        "inv_3_no_posting_touches_the_pending_or_encumbrance_kinds__kernel",
        Layer::Kernel,
        Feature::F004,
        Port::Service,
        &["REQ-KER-3", "REQ-BA-1"],
        inv_3_no_posting_touches_the_pending_or_encumbrance_kinds__kernel,
    ),
    service_case!(
        "INV-14",
        inv_14_capture_and_release_are_keyed_by_operation_never_by_amount__service,
        &["ERR-AmountMismatch", "REQ-OP-2"]
    ),
    // Errors (SPEC §10).
    service_case!(
        "ERR-InsufficientAvailable",
        err_insufficient_available__service,
        &[]
    ),
    service_case!("ERR-AmountMismatch", err_amount_mismatch__service, &[]),
    // Acceptance criteria of the V0 stories.
    service_case!("AC-B2.1", ac_b2_1__service, &["INV-3", "REQ-KER-4"]),
    service_case!("AC-B2.2", ac_b2_2__service, &["REQ-BA-4"]),
    service_case!("AC-C1.1", ac_c1_1__service, &["REQ-BA-4"]),
    service_case!("AC-C1.2", ac_c1_2__service, &["REQ-BA-3"]),
    service_case!("AC-C1.3", ac_c1_3__service, &["REQ-AU-1"]),
    // The first half of AC-C2.1: the difference returns in the same posting. The repeat half is
    // F-007's (f007.rs).
    service_case!("AC-C2.1", ac_c2_1__service, &["REQ-OP-2"]),
    service_case!("AC-C2.2", ac_c2_2__service, &["REQ-OP-2"]),
    service_case!("AC-C3.1", ac_c3_1__service, &["REQ-OP-1"]),
    // Moved from F-003's table: "an operation already reserved runs to a terminal state after
    // its authority is revoked" needs a reservation (F-003 NOTES.md).
    service_case!("AC-G3.2", ac_g3_2__service, &["REQ-OP-9", "REQ-AU-3"]),
];

// ---------------------------------------------------------------------------
// Kernel case: raw transfers, ledger-core bypassed.

fn inv_3_customer_never_negative_while_reservations_are_open__kernel(
    h: &mut dyn Harness,
) -> Outcome {
    let k = kernel(h);
    let issuer = create(k, 1, 1, false);
    let payer = create(k, 2, 1, true);
    // Two holding positions, flagged as DEC-2's are.
    let holding_a = create(k, 3, 1, true);
    let holding_b = create(k, 4, 1, true);
    let merchant = create(k, 5, 1, true);
    let fund = raw(k, 0x40, &[(issuer, Debit, 1, 100), (payer, Credit, 1, 100)]);
    assert_eq!(submit(k, &fund), EventStatus::Created);
    // Two reservations: settled transfers into the holdings. Available is now 10.
    let a = raw(
        k,
        0x41,
        &[(payer, Debit, 1, 60), (holding_a, Credit, 1, 60)],
    );
    assert_eq!(submit(k, &a), EventStatus::Created);
    let b = raw(
        k,
        0x42,
        &[(payer, Debit, 1, 30), (holding_b, Credit, 1, 30)],
    );
    assert_eq!(submit(k, &b), EventStatus::Created);
    let before = k.root().unwrap();

    // A spend of 11 against an available 10: the kernel refuses, whatever is reserved.
    let spend = raw(k, 0x43, &[(payer, Debit, 1, 11), (merchant, Credit, 1, 11)]);
    assert_eq!(submit(k, &spend), EventStatus::BalanceMustNotBeNegative);
    // A third reservation of 11: refused the same way.
    let c = raw(k, 0x44, &[(payer, Debit, 1, 11), (merchant, Credit, 1, 11)]);
    assert_eq!(submit(k, &c), EventStatus::BalanceMustNotBeNegative);
    // Taking more out of a holding than it holds: refused, so capture plus release can never
    // exceed the reservation.
    let over = raw(
        k,
        0x45,
        &[(holding_a, Debit, 1, 61), (merchant, Credit, 1, 61)],
    );
    assert_eq!(submit(k, &over), EventStatus::BalanceMustNotBeNegative);
    // One transfer that debits the payer 21 and credits it 10 back out of a holding would leave
    // it at 10 - 21 + 10 = -1: refused as a whole.
    let mixed = raw(
        k,
        0x46,
        &[
            (payer, Debit, 1, 21),
            (holding_b, Debit, 1, 10),
            (merchant, Credit, 1, 21),
            (payer, Credit, 1, 10),
        ],
    );
    assert_eq!(submit(k, &mixed), EventStatus::BalanceMustNotBeNegative);
    assert_eq!(k.root().unwrap(), before);

    // Exactly the available balance: accepted, and then nothing more.
    let exact = raw(k, 0x47, &[(payer, Debit, 1, 10), (merchant, Credit, 1, 10)]);
    assert_eq!(submit(k, &exact), EventStatus::Created);
    let one = raw(k, 0x48, &[(payer, Debit, 1, 1), (merchant, Credit, 1, 1)]);
    assert_eq!(submit(k, &one), EventStatus::BalanceMustNotBeNegative);
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// A world of principals over the service port.

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
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
        let mut w = World { s, f, n: 0, now };
        w.issuer();
        w
    }

    fn op(&mut self) -> OperationId {
        self.n += 1;
        let mut b = [0xF4; 16];
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

    /// A context for a transition of the reserved operation `op`.
    fn on(&self, caller: Caller, op: OperationId) -> Ctx {
        Ctx {
            caller,
            op,
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
            .open_identity(&ctx, kind, acts_for, Verifier::new(vec![4]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn person(&mut self) -> Caller {
        self.identity(IdentityKind::Person, None)
    }

    fn account(&mut self, owner: &Caller) -> AccountId {
        let ctx = self.ctx(*owner);
        let acknowledged = acknowledgement(&*self.s, owner, owner.principal, Custody::Owned);
        self.s
            .open_account(
                &ctx,
                OpenAccount {
                    owner: owner.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Owned,
                    treasury_limits: BTreeMap::new(),
                    acknowledged,
                },
            )
            .unwrap()
            .value
    }

    /// The operator, able to issue USD.
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
                    per_window: Some(Window {
                        length: NonZeroU64::new(1_000_000).unwrap(),
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

    /// A person with one Customer account holding `amount` USD.
    fn funded(&mut self, amount: u64) -> (Caller, AccountId) {
        let p = self.person();
        let a = self.account(&p);
        if amount > 0 {
            self.issue(a, amount);
        }
        (p, a)
    }

    /// Reserve `amount` of `payer` as `caller`: the operation reference, or the refusal.
    fn reserve(
        &mut self,
        caller: Caller,
        payer: AccountId,
        amount: u64,
    ) -> Result<OperationId, Refusal> {
        let ctx = self.ctx(caller);
        self.s
            .reserve(&ctx, payer, usd(), m(amount))
            .map(|_| ctx.op)
    }

    fn view(&self, account: AccountId) -> BalanceView {
        self.s.balance_view(&account, &usd()).unwrap()
    }

    fn available(&self, account: AccountId) -> i128 {
        self.view(account).available
    }

    /// The kernel transfer of posting number `seq`, from the book's delta log.
    fn posting(&self, seq: u64) -> cipher_clerk::Transfer {
        for record in self.s.book_log() {
            for p in &record.delta.postings {
                if p.seq == seq {
                    return record.delta.transfers[&p.transfer].clone();
                }
            }
        }
        panic!("no posting {seq}");
    }

    /// The service's book, as a fingerprint of everything a refused command must not change:
    /// the kernel's root, the index, and the number of operation records.
    fn fingerprint(&self) -> ([u8; 32], Vec<Status>, usize) {
        (
            self.s.book_root(),
            self.s.state().reservations().iter().copied().collect(),
            self.s.book_log().len(),
        )
    }

    fn holdings_agree(&self) {
        self.s.check_holdings().unwrap();
        assert_eq!(self.s.trial_balance(&usd()), Ok(0));
    }
}

// ---------------------------------------------------------------------------
// Service cases.

/// The layers of every stored kernel account and entry, after reserves, a capture for less, a
/// release and a full capture.
fn inv_3_no_posting_touches_the_pending_or_encumbrance_kinds__kernel(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(1_000);
    let (_, b) = w.funded(0);
    let one = w.reserve(alice, a, 300).unwrap();
    let two = w.reserve(alice, a, 200).unwrap();
    let three = w.reserve(alice, a, 100).unwrap();
    w.s.capture(&w.on(alice, one), b, m(120)).unwrap();
    w.s.release(&w.on(alice, two)).unwrap();
    w.s.capture(&w.on(alice, three), b, m(100)).unwrap();
    w.holdings_agree();

    let mut accounts = std::collections::BTreeSet::new();
    for record in w.s.book_log() {
        assert!(record.delta.pending.is_empty());
        for t in record.delta.transfers.values() {
            assert!(!t.flags.contains(cipher_clerk::TransferFlags::PENDING));
            for e in &t.entries {
                assert_eq!(e.layer, KLayer::Settled, "an entry off the settled kind");
            }
        }
        accounts.extend(record.delta.accounts.keys().copied());
    }
    assert!(
        accounts.len() >= 6,
        "the payer, two others and three holdings"
    );
    for id in accounts {
        let a: Account = w.s.kernel_account(&id).unwrap();
        let blank = Account::new(
            id,
            a.journal_id,
            a.auth_key,
            a.ledger,
            a.code,
            a.normal_balance,
        );
        for layer in [KLayer::Pending, KLayer::Encumbrance] {
            let i = layer.as_index();
            assert_eq!(
                a.balances[i], blank.balances[i],
                "{id:?} holds value on {layer:?}"
            );
        }
    }
    Outcome::Pass
}

fn inv_14_capture_and_release_are_keyed_by_operation_never_by_amount__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let big = w.reserve(alice, a, 50).unwrap();
    let small = w.reserve(alice, a, 30).unwrap();
    assert_eq!(w.view(a).reserved, 80);

    // Capturing the small reservation for 40 is a mismatch, although the big one holds 50: no
    // amount ever selects a reservation. Nothing changes.
    let before = w.fingerprint();
    assert_eq!(
        w.s.capture(&w.on(alice, small), b, m(40)).map(|a| a.value),
        Err(Refusal::AmountMismatch)
    );
    assert_eq!(w.fingerprint(), before);

    // The small one captured for 25: 5 returns to alice, and the big one is untouched.
    w.s.capture(&w.on(alice, small), b, m(25)).unwrap();
    assert_eq!((w.available(a), w.view(a).reserved), (25, 50));
    assert_eq!(w.available(b), 25);
    // A release takes no amount: exactly what the index holds for this operation returns.
    w.s.release(&w.on(alice, big)).unwrap();
    assert_eq!((w.available(a), w.view(a).reserved), (75, 0));

    // Ended reservations cannot be ended again, and an unknown reference names no reservation.
    // (An identical repeat of the command that ended one returns its original outcome instead,
    // F-007, REQ-OP-3: that is `AC-C2.1`'s second half, in f007.rs.)
    let before = w.fingerprint();
    assert_eq!(
        w.s.release(&w.on(alice, small)).map(|a| a.value),
        Err(Refusal::OperationTerminal)
    );
    for op in [big, small] {
        assert_eq!(
            w.s.capture(&w.on(alice, op), b, m(1)).map(|a| a.value),
            Err(Refusal::OperationTerminal)
        );
    }
    assert_eq!(w.fingerprint(), before);
    let stranger = w.op();
    assert_eq!(
        w.s.release(&w.on(alice, stranger)).map(|a| a.value),
        Err(Refusal::UnknownOperation)
    );
    // The index records how each ended, keyed by the operation.
    let st = w.s.state().reservations();
    assert!(matches!(
        st.status(&small),
        Some(Status::Closed(_, Closing::Captured { cleared, destination }))
            if cleared == nz(25) && destination == b
    ));
    assert!(matches!(
        st.status(&big),
        Some(Status::Closed(_, Closing::Released))
    ));
    w.holdings_agree();
    Outcome::Pass
}

fn err_insufficient_available__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    w.reserve(alice, a, 70).unwrap();
    let before = w.fingerprint();
    let ctx = w.ctx(alice);
    let r = w.s.reserve(&ctx, a, usd(), m(31));
    assert_eq!(w.fingerprint(), before);
    expect_refusal(r, "ERR-InsufficientAvailable")
}

fn err_amount_mismatch__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let op = w.reserve(alice, a, 60).unwrap();
    let before = w.fingerprint();
    let r = w.s.capture(&w.on(alice, op), b, m(61));
    assert_eq!(w.fingerprint(), before);
    expect_refusal(r, "ERR-AmountMismatch")
}

/// Many spends that together exceed the balance: exactly as many succeed as it covers, and the
/// balance never goes below zero.
fn ac_b2_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (bob, _) = w.funded(0);
    // Two principals spend the same account: alice, and bob under a delegated grant.
    let ctx = w.ctx(alice);
    w.s.grant_authority(
        &ctx,
        GrantRequest {
            kind: GrantKind::Delegated,
            principal: bob.principal,
            subject: Subject::Account(a),
            ops: NonEmptySet::one(OperationKind::Reserve),
            per_operation: Some(nz(1_000)),
            per_window: None,
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        },
    )
    .unwrap();
    let mut succeeded = 0;
    for i in 0..12 {
        let who = if i % 2 == 0 { alice } else { bob };
        match w.reserve(who, a, 9) {
            Ok(_) => succeeded += 1,
            Err(r) => assert_eq!(r, Refusal::InsufficientAvailable),
        }
        assert!(w.available(a) >= 0);
    }
    // 100 covers eleven reservations of 9.
    assert_eq!(succeeded, 11);
    let v = w.view(a);
    assert_eq!((v.available, v.reserved, v.total), (1, 99, 100));
    // A last spend of exactly what is left succeeds, and then nothing more.
    w.reserve(alice, a, 1).unwrap();
    assert_eq!(w.reserve(bob, a, 1), Err(Refusal::InsufficientAvailable));
    assert_eq!(w.available(a), 0);
    w.holdings_agree();
    Outcome::Pass
}

/// The reduction of available is the check's own transfer: one posting that debits the payer,
/// and a refused reserve changes nothing at all.
fn ac_b2_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let ctx = w.ctx(alice);
    let accepted = w.s.reserve(&ctx, a, usd(), m(40)).unwrap();
    assert_eq!(accepted.record.postings, vec![accepted.value]);
    let t = w.posting(accepted.value);
    assert_eq!(
        PostingMeta::kind_of(&t.user_data),
        Some(PostingKind::Reserve)
    );
    let payer =
        w.s.state()
            .account(&a)
            .unwrap()
            .sub_account(w.s.state().registry().get(&usd()).unwrap())
            .id();
    assert!(
        t.entries
            .iter()
            .any(|e| e.account_id == payer && e.direction == Debit)
    );
    assert_eq!(w.available(a), 60);

    // Refused: the book, the index and the log are unchanged, byte for byte, and no record.
    let before = w.fingerprint();
    let ctx = w.ctx(alice);
    assert_eq!(
        w.s.reserve(&ctx, a, usd(), m(61)).map(|a| a.value),
        Err(Refusal::InsufficientAvailable)
    );
    assert_eq!(w.fingerprint(), before);
    assert!(w.s.state().record(&ctx.op).is_none());
    // Its holding was created in the refused step, and dropped with it.
    assert!(w.s.state().reservations().status(&ctx.op).is_none());
    Outcome::Pass
}

/// Available falls by the amount as soon as `reserve` returns: nothing external is waited for.
fn ac_c1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let before = w.view(a);
    let op = w.reserve(alice, a, 35).unwrap();
    let after = w.view(a);
    assert_eq!(after.available, before.available - 35);
    assert_eq!(after.reserved, 35);
    assert_eq!(after.total, before.total);
    assert!(matches!(
        w.s.state().reservations().status(&op),
        Some(Status::Open(r)) if r.amount == nz(35) && r.payer.account == a
    ));
    let record = w.s.state().record(&op).unwrap();
    assert_eq!(record.command, Command::Reserve);
    w.holdings_agree();
    Outcome::Pass
}

/// After a reservation, no read shows the reserved value as spendable, and no spend can use it.
fn ac_c1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    w.reserve(alice, a, 80).unwrap();
    let now = w.now;
    // Every read: the full view, the plain available read, and the number a spend is checked
    // against are the same number (REQ-BA-3).
    let v = w.s.balances(&alice, &a, &usd(), now).unwrap();
    assert_eq!(
        (v.available, v.reserved, v.encumbered, v.total),
        (20, 80, 0, 100)
    );
    assert_eq!(w.s.available(&alice, &a, &usd(), now), Ok(20));
    assert_eq!(w.view(a), v);
    // A spend of 21 is refused; 20 is not.
    assert_eq!(w.reserve(alice, a, 21), Err(Refusal::InsufficientAvailable));
    w.reserve(alice, a, 20).unwrap();
    let v = w.s.balances(&alice, &a, &usd(), now).unwrap();
    assert_eq!((v.available, v.reserved, v.total), (0, 100, 100));
    // Nobody who cannot reach the account reads it at all (INV-16).
    let stranger = w.person();
    assert_eq!(
        w.s.balances(&stranger, &a, &usd(), now),
        Err(Refusal::UnknownAccount)
    );
    Outcome::Pass
}

/// Not enough is a refusal; so is every case where the decision cannot be made.
fn ac_c1_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(50);
    assert_eq!(w.reserve(alice, a, 51), Err(Refusal::InsufficientAvailable));
    // An account that never held the asset: refused by the kernel, and its position is not
    // left behind.
    let empty = w.account(&alice);
    assert_eq!(
        w.reserve(alice, empty, 1),
        Err(Refusal::InsufficientAvailable)
    );
    let st = w.s.state();
    let position = st
        .account(&empty)
        .unwrap()
        .sub_account(st.registry().get(&usd()).unwrap())
        .id();
    assert!(w.s.kernel_account(&position).is_none());
    // No authority over the account: refused, never reserved.
    let (bob, _) = w.funded(0);
    assert_eq!(w.reserve(bob, a, 1), Err(Refusal::UnknownAccount));
    // Authority that does not cover reserving: refused.
    let agent = w.identity(IdentityKind::Agent, Some(alice.principal));
    let ctx = w.ctx(alice);
    w.s.grant_authority(
        &ctx,
        GrantRequest {
            kind: GrantKind::Delegated,
            principal: agent.principal,
            subject: Subject::Account(a),
            ops: NonEmptySet::one(OperationKind::Capture),
            per_operation: Some(nz(10)),
            per_window: None,
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        },
    )
    .unwrap();
    assert_eq!(w.reserve(agent, a, 1), Err(Refusal::NotAuthorised));
    // A credential that cannot be resolved: refused.
    let forged = Caller::new(alice.principal, bob.credential);
    assert_eq!(w.reserve(forged, a, 1), Err(Refusal::CredentialInvalid));
    // A zero amount is not a reservation.
    assert_eq!(w.reserve(alice, a, 0), Err(Refusal::ZeroAmount));
    assert_eq!(w.view(a).available, 50);
    assert_eq!(w.view(a).reserved, 0);
    Outcome::Pass
}

/// A capture for less: the difference is back in available, in the capture's own posting.
fn ac_c2_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, merchant) = w.funded(0);
    let op = w.reserve(alice, a, 60).unwrap();
    let captured = w.s.capture(&w.on(alice, op), merchant, m(45)).unwrap();
    // One posting, three entries: the holding emptied, the merchant and the payer credited.
    assert_eq!(captured.record.postings, vec![captured.value]);
    assert_eq!(captured.record.command, Command::Capture);
    let t = w.posting(captured.value);
    assert_eq!(
        PostingMeta::kind_of(&t.user_data),
        Some(PostingKind::Capture)
    );
    assert_eq!(t.entries.len(), 3);
    assert_eq!(t.entries.iter().filter(|e| e.direction == Debit).count(), 1);
    assert!(t.flags.contains(cipher_clerk::TransferFlags::CLOSING_DEBIT));
    let v = w.view(a);
    assert_eq!((v.available, v.reserved, v.total), (55, 0, 55));
    assert_eq!(w.available(merchant), 45);
    // The holding is closed with nothing in it.
    let Some(Status::Closed(r, _)) = w.s.state().reservations().status(&op) else {
        panic!("the reservation is closed");
    };
    let holding = w.s.kernel_account(&r.holding.id()).unwrap();
    assert!(holding.flags.contains(AccountFlags::CLOSED));
    // The capture is attributed, beside the reservation's own record.
    assert_eq!(w.s.state().transitions(&op).len(), 1);
    // A capture at exactly the reserved amount returns nothing: two entries.
    let op = w.reserve(alice, a, 55).unwrap();
    let full = w.s.capture(&w.on(alice, op), merchant, m(55)).unwrap();
    assert_eq!(w.posting(full.value).entries.len(), 2);
    assert_eq!((w.available(a), w.available(merchant)), (0, 100));
    w.holdings_agree();
    Outcome::Pass
}

fn ac_c2_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, merchant) = w.funded(0);
    let op = w.reserve(alice, a, 40).unwrap();
    let before = (w.fingerprint(), w.view(a), w.view(merchant));
    assert_eq!(
        w.s.capture(&w.on(alice, op), merchant, m(41))
            .map(|a| a.value),
        Err(Refusal::AmountMismatch)
    );
    assert_eq!((w.fingerprint(), w.view(a), w.view(merchant)), before);
    assert!(w.s.state().transitions(&op).is_empty());
    // The reservation is still open, and can still be captured correctly.
    w.s.capture(&w.on(alice, op), merchant, m(40)).unwrap();
    w.holdings_agree();
    Outcome::Pass
}

fn ac_c3_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let op = w.reserve(alice, a, 70).unwrap();
    assert_eq!(w.available(a), 30);
    let released = w.s.release(&w.on(alice, op)).unwrap();
    assert_eq!(released.record.postings, vec![released.value]);
    assert_eq!(released.record.command, Command::Release);
    let t = w.posting(released.value);
    assert_eq!(
        PostingMeta::kind_of(&t.user_data),
        Some(PostingKind::Release)
    );
    assert_eq!(t.entries.len(), 2);
    let v = w.view(a);
    assert_eq!((v.available, v.reserved, v.total), (100, 0, 100));
    w.holdings_agree();
    Outcome::Pass
}

/// A revocation releases nothing: the reservation stays, the revoked principal can no longer end
/// it, and the owner runs it to a terminal state.
fn ac_g3_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, merchant) = w.funded(0);
    let agent = w.identity(IdentityKind::Agent, Some(alice.principal));
    let ctx = w.ctx(alice);
    let grant =
        w.s.grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: agent.principal,
                subject: Subject::Account(a),
                ops: NonEmptySet::new(
                    [
                        OperationKind::Reserve,
                        OperationKind::Capture,
                        OperationKind::Release,
                    ]
                    .into(),
                )
                .unwrap(),
                per_operation: Some(nz(100)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap()
        .value;
    let op = w.reserve(agent, a, 60).unwrap();
    let ctx = w.ctx(alice);
    w.s.revoke_authority(&ctx, grant).unwrap();
    // Nothing was released.
    let v = w.view(a);
    assert_eq!((v.available, v.reserved), (40, 60));
    assert!(matches!(
        w.s.state().reservations().status(&op),
        Some(Status::Open(_))
    ));
    // The agent's authority stopped at once (REQ-AU-3).
    assert_eq!(
        w.s.capture(&w.on(agent, op), merchant, m(60))
            .map(|a| a.value),
        Err(Refusal::AuthorityRevoked)
    );
    assert_eq!(
        w.s.release(&w.on(agent, op)).map(|a| a.value),
        Err(Refusal::AuthorityRevoked)
    );
    assert_eq!(w.reserve(agent, a, 1), Err(Refusal::AuthorityRevoked));
    // The operation runs to a terminal state under the owner's authority.
    let captured = w.s.capture(&w.on(alice, op), merchant, m(60)).unwrap();
    assert_eq!(captured.record.authorities[0].principal, alice.principal);
    assert!(matches!(
        w.s.state().reservations().status(&op),
        Some(Status::Closed(_, Closing::Captured { .. }))
    ));
    assert_eq!((w.available(a), w.available(merchant)), (40, 60));
    // The reservation's own record still names the agent that made it.
    assert_eq!(
        w.s.state().record(&op).unwrap().authorities[0].principal,
        agent.principal
    );
    w.holdings_agree();
    Outcome::Pass
}
