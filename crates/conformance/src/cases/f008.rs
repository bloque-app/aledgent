// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-008` — Internal transfers and multi-leg: its registered cases (`F-015` §6, §7).
//!
//! - The **kernel** cases bypass `ledger-core`: hand-built, validly signed transfers through
//!   [`KernelPort`], submitted as one linked chain the way the binding
//!   submits a multi-leg unit. CipherClerk itself refuses a chain with an unbalanced leg (`INV-1`)
//!   or an overdrawing one (`INV-3`), whole, whichever leg it is.
//! - **Service** cases drive `ledger-core`'s `transfer` through [`ServicePort`] as principals,
//!   and read what it did back from the book's own delta log.
//!
//! Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use cipher_clerk::{Direction, EventStatus, TransferFlags};
use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, IdentityId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{Ctx, Founded, GrantRequest, Leg, OpenAccount, Transfer};
use ledger_core::lifecycle::State as OpState;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::state::Command;
use ledger_kernel::posting::{OperationRef, PostingKind, PostingMeta};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::cases::f001::{create, kernel, raw};
use crate::harness::{Harness, KernelPort, ServicePort, acknowledgement};
use crate::shape::expect_refusal;

use Direction::{Credit, Debit};

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F008,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-008`'s registered cases.
pub const CASES: &[Case] = &[
    // Invariants (SPEC §9), at the layer §9 names: the kernel, bypassing ledger-core.
    Case::run(
        "INV-1",
        "inv_1_an_unbalanced_leg_refuses_the_whole_chain__kernel",
        Layer::Kernel,
        Feature::F008,
        Port::Kernel,
        &["REQ-KER-1", "REQ-KER-10", "AC-B3.1"],
        inv_1_an_unbalanced_leg_refuses_the_whole_chain__kernel,
    ),
    Case::run(
        "INV-3",
        "inv_3_an_overdrawing_leg_refuses_the_whole_chain__kernel",
        Layer::Kernel,
        Feature::F008,
        Port::Kernel,
        &["REQ-KER-4", "REQ-KER-10", "AC-B3.1", "AC-B3.2"],
        inv_3_an_overdrawing_leg_refuses_the_whole_chain__kernel,
    ),
    // Errors (SPEC §10).
    service_case!(
        "ERR-BatchTooLarge",
        err_batch_too_large__service,
        &["OQ-24"]
    ),
    // Acceptance criteria of the V0 stories.
    service_case!("AC-A2.2", ac_a2_2__service, &["INV-1", "REQ-OP-1"]),
    service_case!("AC-B1.1", ac_b1_1__service, &["INV-1", "REQ-OP-1"]),
    service_case!("AC-B1.2", ac_b1_2__service, &["INV-3", "REQ-LA-2"]),
    service_case!("AC-B1.3", ac_b1_3__service, &["REQ-SL-5"]),
    service_case!("AC-B1.4", ac_b1_4__service, &["ERR-ZeroAmount"]),
    service_case!("AC-B3.1", ac_b3_1__service, &["REQ-KER-10"]),
    service_case!("AC-B3.2", ac_b3_2__service, &["REQ-BA-4"]),
    service_case!("AC-B3.3", ac_b3_3__service, &["ERR-BatchTooLarge", "OQ-24"]),
];

// ---------------------------------------------------------------------------
// Kernel cases: raw chains, ledger-core bypassed.

/// Mark every transfer of `chain` but the last `LINKED`, as the binding does, and sign again.
fn linked(
    k: &dyn KernelPort,
    chain: &mut [(cipher_clerk::Transfer, Vec<cipher_clerk::state::Opening>)],
) {
    let n = chain.len();
    for (i, (t, _)) in chain.iter_mut().enumerate() {
        if i + 1 < n {
            t.flags = t.flags | TransferFlags::LINKED;
        }
        t.signatures = k.sign(t);
    }
}

/// Submit `chain` as one kernel batch; the kernel's status per event.
fn submit_chain(
    k: &mut dyn KernelPort,
    chain: &[(cipher_clerk::Transfer, Vec<cipher_clerk::state::Opening>)],
) -> Vec<EventStatus> {
    let transfers: Vec<_> = chain.iter().map(|(t, _)| t.clone()).collect();
    let openings: Vec<_> = chain.iter().flat_map(|(_, o)| o.clone()).collect();
    k.submit(&transfers, &openings, 1_000)
        .unwrap()
        .into_iter()
        .map(|r| r.status)
        .collect()
}

fn inv_1_an_unbalanced_leg_refuses_the_whole_chain__kernel(h: &mut dyn Harness) -> Outcome {
    let k = kernel(h);
    let issuer = create(k, 1, 1, false);
    let payer = create(k, 2, 1, true);
    let payee = create(k, 3, 1, true);
    let fund = raw(k, 0x80, &[(issuer, Debit, 1, 100), (payer, Credit, 1, 100)]);
    assert_eq!(submit_chain(k, &[fund]), vec![EventStatus::Created]);
    let before = k.root().unwrap();
    // Three legs, the unbalanced one at each position in turn: the kernel refuses that event
    // and fails the others with it. Nothing is applied.
    for bad in 0..3u8 {
        let mut chain: Vec<_> = (0..3u8)
            .map(|i| {
                let credit = if i == bad { 9 } else { 10 };
                raw(
                    k,
                    0x90 + bad * 4 + i,
                    &[(payer, Debit, 1, 10), (payee, Credit, 1, credit)],
                )
            })
            .collect();
        linked(k, &mut chain);
        let statuses = submit_chain(k, &chain);
        for (i, s) in statuses.iter().enumerate() {
            if i == usize::from(bad) {
                assert_eq!(*s, EventStatus::UnbalancedTransfer, "leg {i} of {bad}");
            } else {
                assert_ne!(*s, EventStatus::Created, "leg {i} of {bad} applied");
            }
        }
        assert_eq!(k.root().unwrap(), before, "a leg of chain {bad} was kept");
    }
    // The same chain, balanced: applied whole.
    let mut chain: Vec<_> = (0..3u8)
        .map(|i| {
            raw(
                k,
                0xB0 + i,
                &[(payer, Debit, 1, 10), (payee, Credit, 1, 10)],
            )
        })
        .collect();
    linked(k, &mut chain);
    assert_eq!(submit_chain(k, &chain), vec![EventStatus::Created; 3]);
    Outcome::Pass
}

fn inv_3_an_overdrawing_leg_refuses_the_whole_chain__kernel(h: &mut dyn Harness) -> Outcome {
    let k = kernel(h);
    let issuer = create(k, 1, 1, false);
    let payer = create(k, 2, 1, true);
    let a = create(k, 3, 1, true);
    let b = create(k, 4, 1, true);
    let fund = raw(k, 0x80, &[(issuer, Debit, 1, 100), (payer, Credit, 1, 100)]);
    assert_eq!(submit_chain(k, &[fund]), vec![EventStatus::Created]);
    let before = k.root().unwrap();
    // AC-B3.2: two legs from one flagged account, each covered alone, not together.
    let mut chain = vec![
        raw(k, 0x81, &[(payer, Debit, 1, 60), (a, Credit, 1, 60)]),
        raw(k, 0x82, &[(payer, Debit, 1, 60), (b, Credit, 1, 60)]),
    ];
    linked(k, &mut chain);
    let statuses = submit_chain(k, &chain);
    assert_ne!(statuses[0], EventStatus::Created);
    assert_eq!(statuses[1], EventStatus::BalanceMustNotBeNegative);
    assert_eq!(k.root().unwrap(), before);
    // The kernel applies the legs in order against the staged state: a leg may spend what an
    // earlier one credited, never what a later one would.
    let mut late = vec![
        raw(k, 0x83, &[(a, Debit, 1, 30), (b, Credit, 1, 30)]),
        raw(k, 0x84, &[(payer, Debit, 1, 30), (a, Credit, 1, 30)]),
    ];
    linked(k, &mut late);
    let statuses = submit_chain(k, &late);
    assert_eq!(statuses[0], EventStatus::BalanceMustNotBeNegative);
    assert_ne!(statuses[1], EventStatus::Created);
    assert_eq!(k.root().unwrap(), before);
    let mut early = vec![
        raw(k, 0x85, &[(payer, Debit, 1, 30), (a, Credit, 1, 30)]),
        raw(k, 0x86, &[(a, Debit, 1, 30), (b, Credit, 1, 30)]),
    ];
    linked(k, &mut early);
    assert_eq!(submit_chain(k, &early), vec![EventStatus::Created; 2]);
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

fn leg(from: AccountId, to: AccountId, amount: u64) -> Leg {
    Leg {
        from,
        to,
        asset: usd(),
        amount: m(amount),
    }
}

/// The deadline every transfer here carries: the in-process deployment declares no default for
/// transfers (`harness::service_lifecycle`).
const DEADLINE: Time = 10_000;

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
        let mut b = [0xF8; 16];
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

    fn person(&mut self) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .s
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![8]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
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
                    ops: NonEmptySet::one(OperationKind::Issue),
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

    /// A person with one Customer account holding `amount` USD.
    fn funded(&mut self, amount: u64) -> (Caller, AccountId) {
        let p = self.person();
        let a = self.account(&p);
        if amount > 0 {
            let ctx = self.ctx(self.operator());
            self.s.issue(&ctx, usd(), a, m(amount)).unwrap();
        }
        (p, a)
    }

    /// `owner` lets `to` move value out of `account`.
    fn delegate(&mut self, owner: Caller, to: IdentityId, account: AccountId) {
        let ctx = self.ctx(owner);
        self.s
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Delegated,
                    principal: to,
                    subject: Subject::Account(account),
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

    fn transfer(
        &mut self,
        caller: Caller,
        legs: Vec<Leg>,
    ) -> (
        Ctx,
        Result<ledger_core::ledger::Accepted<Vec<u64>>, Refusal>,
    ) {
        let ctx = self.ctx(caller);
        let r = self.s.transfer(&ctx, &Transfer::unit(legs).until(DEADLINE));
        (ctx, r)
    }

    fn available(&self, account: AccountId) -> i128 {
        self.s.balance_view(&account, &usd()).unwrap().available
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

    /// Everything a refused transfer must leave as it was: the kernel's root, the delta log, the
    /// operation store, the outbox and the attribution records.
    fn fingerprint(&self) -> ([u8; 32], usize, usize, usize) {
        let ops = self.s.state().operations();
        (
            self.s.book_root(),
            self.s.book_log().len(),
            ops.all().count(),
            ops.outbox().count(),
        )
    }

    /// `op` took one `Transfer` posting per leg, in one commit, and settled in it.
    fn settled_in_one_commit(&self, op: &OperationId, seqs: &[u64]) {
        let o = self.s.operation(op).expect("the operation exists");
        assert_eq!(o.state(), OpState::Settled);
        let path: Vec<_> = o.transitions().iter().map(|t| (t.from, t.to)).collect();
        assert_eq!(
            path,
            vec![
                (OpState::Received, OpState::Accepted),
                (OpState::Accepted, OpState::Settled)
            ]
        );
        assert_eq!(o.transitions()[1].posting, Some(PostingKind::Transfer));
        assert_eq!(o.transitions()[1].postings, seqs);
        let log = self.s.book_log();
        let commit = log
            .iter()
            .find(|r| r.delta.postings.iter().any(|p| p.seq == seqs[0]))
            .expect("the commit of the first posting");
        let in_commit: Vec<u64> = commit.delta.postings.iter().map(|p| p.seq).collect();
        assert_eq!(in_commit, seqs, "every leg in the one commit");
        for (i, seq) in seqs.iter().enumerate() {
            let t = self.posting(*seq);
            let meta = PostingMeta::new(PostingKind::Transfer, op.to_ref(), i as u32);
            let home = ledger_kernel::partition::Partition::of_key(&t.entries[0].account_id.0);
            assert_eq!(t.id, meta.transfer_id_in(home));
            assert_eq!(t.entries.len(), 2);
            assert_eq!(t.entries[0].direction, Debit);
            assert_eq!(t.entries[1].direction, Credit);
        }
    }
}

// ---------------------------------------------------------------------------
// Service cases.

fn err_batch_too_large__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let max = w.s.lifecycle_config().max_legs;
    let fp = w.fingerprint();
    let (ctx, r) = w.transfer(alice, vec![leg(a, b, 1); max + 1]);
    assert_eq!(w.fingerprint(), fp);
    assert!(w.s.operation(&ctx.op).is_none());
    expect_refusal(r.map(|a| a.value), "ERR-BatchTooLarge")
}

fn ac_a2_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a1) = w.funded(100);
    let a2 = w.account(&alice);
    let (ctx, r) = w.transfer(alice, vec![leg(a1, a2, 40)]);
    let r = r.unwrap();
    assert_eq!(r.value.len(), 1, "one posting");
    assert_eq!(
        r.status.unwrap().state,
        OpState::Settled,
        "final when accepted"
    );
    assert_eq!(r.record.command, Command::Transfer);
    w.settled_in_one_commit(&ctx.op, &r.value);
    assert_eq!((w.available(a1), w.available(a2)), (60, 40));
    Outcome::Pass
}

fn ac_b1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let (ctx, r) = w.transfer(alice, vec![leg(a, b, 30)]);
    let r = r.unwrap();
    assert_eq!(r.value.len(), 1);
    assert_eq!(r.record.postings, r.value);
    assert_eq!(r.status.unwrap().state, OpState::Settled);
    w.settled_in_one_commit(&ctx.op, &r.value);
    // Both accounts changed, exactly, in that one posting.
    assert_eq!((w.available(a), w.available(b)), (70, 30));
    assert_eq!(w.s.trial_balance(&usd()), Ok(0));
    Outcome::Pass
}

fn ac_b1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let fp = w.fingerprint();
    let (ctx, r) = w.transfer(alice, vec![leg(a, b, 101)]);
    assert_eq!(w.fingerprint(), fp, "something changed");
    assert!(w.s.operation(&ctx.op).is_none());
    assert!(w.s.state().record(&ctx.op).is_none());
    assert_eq!((w.available(a), w.available(b)), (100, 0));
    expect_refusal(r.map(|a| a.value), "ERR-InsufficientAvailable")
}

fn ac_b1_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(10_000);
    let (bob, b) = w.funded(10_000);
    let issuance = w.f.issuance;
    let issued = w.s.balance_view(&issuance, &usd()).unwrap().available;
    let log_before = w.s.book_log().len();
    let outbox_before = w.s.state().operations().outbox().count();
    let mut ops = Vec::new();
    for i in 0..100u64 {
        let (who, from, to) = if i % 2 == 0 {
            (alice, a, b)
        } else {
            (bob, b, a)
        };
        let (ctx, r) = w.transfer(who, vec![leg(from, to, 1 + i % 5)]);
        r.unwrap();
        ops.push(ctx.op);
    }
    // The outbox holds the transitions' notifications and nothing else; every posting since is a
    // Transfer, and none touches the issuance counterparty. That the simulated settlement
    // layer records nothing is `F-010`'s half of this criterion
    // (`ac_b1_3_transfers_leave_the_simulated_layer_untouched__service`).
    let outbox: Vec<_> = w.s.state().operations().outbox().cloned().collect();
    assert_eq!(outbox.len(), outbox_before + 2 * ops.len());
    for n in outbox.iter().filter(|n| ops.contains(&n.op)) {
        assert!(matches!(
            n.shown.state,
            OpState::Accepted | OpState::Settled
        ));
    }
    let issuance_sub = |w: &World, id| {
        w.s.state()
            .account(&id)
            .unwrap()
            .sub_account(w.s.state().registry().get(&usd()).unwrap())
            .id()
    };
    let issuance_id = issuance_sub(&w, issuance);
    for record in &w.s.book_log()[log_before..] {
        for t in record.delta.transfers.values() {
            assert_eq!(
                PostingMeta::kind_of(&t.user_data),
                Some(PostingKind::Transfer)
            );
            assert!(t.entries.iter().all(|e| e.account_id != issuance_id));
        }
    }
    assert_eq!(
        w.s.balance_view(&issuance, &usd()).unwrap().available,
        issued
    );
    assert_eq!(w.s.trial_balance(&usd()), Ok(0));
    Outcome::Pass
}

fn ac_b1_4__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let fp = w.fingerprint();
    let (_, r) = w.transfer(alice, vec![leg(a, b, 0)]);
    assert_eq!(w.fingerprint(), fp);
    // It does not silently succeed as part of a unit either.
    let (_, unit) = w.transfer(alice, vec![leg(a, b, 5), leg(a, b, 0)]);
    assert_eq!(unit.map(|a| a.value), Err(Refusal::ZeroAmount));
    assert_eq!(w.fingerprint(), fp);
    expect_refusal(r.map(|a| a.value), "ERR-ZeroAmount")
}

fn ac_b3_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let (_, c) = w.funded(0);
    let max = w.s.lifecycle_config().max_legs.min(4);
    // One overdrawing leg at each position: no leg applies.
    for bad in 0..max {
        let legs: Vec<Leg> = (0..max)
            .map(|i| {
                leg(
                    a,
                    if i % 2 == 0 { b } else { c },
                    if i == bad { 1_000 } else { 5 },
                )
            })
            .collect();
        let fp = w.fingerprint();
        let (ctx, r) = w.transfer(alice, legs);
        assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
        assert_eq!(
            w.fingerprint(),
            fp,
            "a leg of the unit failing at {bad} applied"
        );
        assert!(w.s.operation(&ctx.op).is_none());
    }
    assert_eq!(
        (w.available(a), w.available(b), w.available(c)),
        (100, 0, 0)
    );
    // Every leg covered: all apply, in one commit.
    let legs: Vec<Leg> = (0..max)
        .map(|i| leg(a, if i % 2 == 0 { b } else { c }, 5))
        .collect();
    let (ctx, r) = w.transfer(alice, legs);
    let r = r.unwrap();
    assert_eq!(r.value.len(), max);
    w.settled_in_one_commit(&ctx.op, &r.value);
    assert_eq!(w.available(a), 100 - 5 * max as i128);
    Outcome::Pass
}

fn ac_b3_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (bob, b) = w.funded(0);
    let (_, c) = w.funded(0);
    let fp = w.fingerprint();
    let (_, r) = w.transfer(alice, vec![leg(a, b, 60), leg(a, c, 60)]);
    assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
    assert_eq!(w.fingerprint(), fp);
    // Each alone is covered.
    let (_, r) = w.transfer(alice, vec![leg(a, b, 60)]);
    r.unwrap();
    assert_eq!(w.available(a), 40);
    // Legs are applied in order: value passed through Bob's account in the same unit counts.
    w.delegate(bob, alice.principal, b);
    let (_, r) = w.transfer(alice, vec![leg(a, b, 40), leg(b, c, 100)]);
    r.unwrap();
    assert_eq!(
        (w.available(a), w.available(b), w.available(c)),
        (0, 0, 100)
    );
    Outcome::Pass
}

fn ac_b3_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let max = w.s.lifecycle_config().max_legs;
    let fp = w.fingerprint();
    // Refused at submission: before the caller, the accounts or the asset are looked at, so a
    // unit that is wrong in every other way too gets this refusal.
    let stranger = Caller::new(
        IdentityId::mint(&OperationId::from_ref(OperationRef([0x77; 16])), 0),
        ledger_core::id::CredentialId::mint(&OperationId::from_ref(OperationRef([0x77; 16])), 1),
    );
    let nowhere = AccountId::mint(&OperationId::from_ref(OperationRef([0x78; 16])), 0);
    let garbage = Leg {
        from: nowhere,
        to: nowhere,
        asset: AssetId::parse("NOPE").unwrap(),
        amount: m(0),
    };
    let (_, wrong) = w.transfer(stranger, vec![garbage; max + 1]);
    assert_eq!(wrong.map(|a| a.value), Err(Refusal::BatchTooLarge));
    let (_, r) = w.transfer(alice, vec![leg(a, b, 1); max + 1]);
    assert_eq!(w.fingerprint(), fp);
    // The declared maximum itself is accepted.
    let (_, ok) = w.transfer(alice, vec![leg(a, b, 1); max]);
    assert_eq!(ok.unwrap().value.len(), max);
    expect_refusal(r.map(|a| a.value), "ERR-BatchTooLarge")
}
