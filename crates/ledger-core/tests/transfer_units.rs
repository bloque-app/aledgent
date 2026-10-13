// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The randomised test of transfer units (`F-008` T-008-05, `AC-B3.1`, `AC-B3.2`, `INV-1`,
//! `INV-3`, `REQ-KER-10`).
//!
//! The core is a single writer: "concurrent" means any order of arrival and any mix of payers.
//! Each case builds a small book — five Customer accounts of five people, in delegated custody,
//! funded at random, and an operator grant to move value between them — and submits 25 random
//! units of one to four legs through [`Ledger::transfer`], exactly as a caller would. Legs pick
//! any payer and payee, so units chain value through accounts, debit one account several times,
//! and overdraw at a random leg.
//!
//! A plaintext model applies each unit's legs in order and says whether the kernel will take it:
//! the whole unit, or nothing if any leg would leave its payer below zero. After every unit the
//! test checks:
//!
//! - the command applied exactly when the model says, and a refused unit is
//!   `ERR-InsufficientAvailable` with no partial application: every balance, the book's root and
//!   the posting sequence are as before, and no operation was recorded (`AC-B3.1`);
//! - an applied unit is one `Transfer` posting per leg, settled, with consecutive sequence
//!   numbers (`REQ-KER-10`);
//! - every posting the kernel accepted balances per asset: its debit and credit commit to the same
//!   value (`INV-1`, read from the kernel's own delta log);
//! - no Customer balance is ever negative, and every balance equals the model's (`INV-3`); the
//!   trial balance is zero.
//!
//! `F008_UNITS` sets the number of units on `MemStore` (default 10 000), split in four parts that
//! run in parallel; `F008_UNITS_FILE` those on `FileStore` (redb's in-memory backend, the same
//! code without the fsync; default 1 000).

// Test names end in `__<store>_part_<n>`, after the criterion they verify.
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};

use cipher_clerk::Direction;
use conformance::harness::delta_log;
use ledger_core::Ledger;
use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody};
use ledger_core::asset::{AssetId, AssetKind, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{
    AssetDecl, Ctx, Founded, Genesis, GrantRequest, Leg, OpenAccount, Transfer,
};
use ledger_core::lifecycle::{LifecycleConfig, State};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::{OperationRef, PostingKind, PostingMeta};
use ledger_kernel::store::{FileStore, LeafStore, MemStore};
use ledger_kernel::vault::{FileVault, MemVault, VaultBackend};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

const CUSTOMERS: usize = 5;
const UNITS_PER_BOOK: usize = 25;
const MAX_LEGS: usize = 4;

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

/// One case: the opening balances, and the units as (payer, payee, amount) per leg.
#[derive(Clone, Debug)]
struct Case {
    funds: Vec<u64>,
    units: Vec<Vec<(usize, usize, u64)>>,
}

fn a_case() -> impl Strategy<Value = Case> {
    let leg = (0..CUSTOMERS, 1..CUSTOMERS, 1..=150u64)
        .prop_map(|(from, d, amount)| (from, (from + d) % CUSTOMERS, amount));
    (
        vec(0..=300u64, CUSTOMERS),
        vec(vec(leg, 1..=MAX_LEGS), UNITS_PER_BOOK),
    )
        .prop_map(|(funds, units)| Case { funds, units })
}

#[derive(Default)]
struct Tally {
    applied: AtomicUsize,
    refused: AtomicUsize,
    multi_applied: AtomicUsize,
    multi_refused: AtomicUsize,
}

struct World<S: LeafStore, V: VaultBackend> {
    ledger: Ledger<S, V>,
    founded: Founded,
    accounts: Vec<AccountId>,
    n: u64,
}

impl<S: LeafStore, V: VaultBackend> World<S, V> {
    fn new(book: Book<S, V>, funds: &[u64]) -> Self {
        let genesis = Genesis {
            assets: vec![AssetDecl {
                id: usd(),
                location: Some(ledger_core::asset::AssetLocation::parse("Here(101)").unwrap()),
                precision: 2,
                kind: AssetKind::Fiat,
                deprecated: false,
            }],
            operator_verifier: Verifier::new(vec![0]),
            lifecycle: LifecycleConfig::new([0xF8; 16], 60, 3_600)
                .with_default_deadline(OperationKind::Transfer, 1_000)
                .with_max_legs(MAX_LEGS),
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
        let mut w = World {
            ledger,
            founded,
            accounts: Vec::new(),
            n: 0,
        };
        let op = w.operator();
        // Issuance of USD, and transfers between delegated accounts, for the operator.
        for request in [
            GrantRequest {
                kind: GrantKind::Issuance,
                principal: op.principal,
                subject: Subject::Asset(usd()),
                ops: NonEmptySet::one(OperationKind::Issue),
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
            GrantRequest {
                kind: GrantKind::Operator,
                principal: op.principal,
                subject: Subject::DelegatedAccounts,
                ops: NonEmptySet::one(OperationKind::Transfer),
                per_operation: Some(nz(u64::MAX / 2)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        ] {
            let ctx = w.ctx(op);
            w.ledger.grant_authority(&ctx, request).unwrap();
        }
        for &f in funds {
            let ctx = w.ctx(op);
            let person = w
                .ledger
                .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
                .unwrap()
                .value;
            let ctx = w.ctx(Caller::new(person.identity, person.credential));
            let account = w
                .ledger
                .open_account(
                    &ctx,
                    OpenAccount {
                        owner: person.identity,
                        kind: AccountKind::Customer,
                        custody: Custody::Delegated,
                        treasury_limits: BTreeMap::new(),
                        acknowledged: None,
                    },
                )
                .unwrap()
                .value;
            if f > 0 {
                let ctx = w.ctx(op);
                w.ledger.issue(&ctx, usd(), account, Minor::new(f)).unwrap();
            }
            w.accounts.push(account);
        }
        w
    }

    fn operator(&self) -> Caller {
        Caller::new(self.founded.operator, self.founded.operator_credential)
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.n += 1;
        let mut b = [0xF8; 16];
        b[8..].copy_from_slice(&self.n.to_be_bytes());
        Ctx {
            caller,
            op: OperationId::from_ref(OperationRef(b)),
            now: 10,
        }
    }

    fn available(&self, i: usize) -> i128 {
        self.ledger
            .balance_view(&self.accounts[i], &usd())
            .unwrap()
            .available
    }
}

/// Whether the kernel takes `legs` on `model`: every payer covered at its leg, in order.
fn model_applies(model: &mut [i128], legs: &[(usize, usize, u64)]) -> bool {
    let mut next = model.to_vec();
    for &(from, to, amount) in legs {
        next[from] -= i128::from(amount);
        if next[from] < 0 {
            return false;
        }
        next[to] += i128::from(amount);
    }
    model.copy_from_slice(&next);
    true
}

fn check_case<S: LeafStore, V: VaultBackend>(book: Book<S, V>, case: &Case, tally: &Tally) {
    let mut w = World::new(book, &case.funds);
    let mut model: Vec<i128> = case.funds.iter().map(|f| i128::from(*f)).collect();
    let op = w.operator();
    let mut log_seen = delta_log(w.ledger.book().store()).unwrap().len();
    for legs in &case.units {
        let transfer = Transfer::unit(
            legs.iter()
                .map(|&(from, to, amount)| Leg {
                    from: w.accounts[from],
                    to: w.accounts[to],
                    asset: usd(),
                    amount: Minor::new(amount),
                })
                .collect(),
        );
        let before = (
            w.ledger.book().root().unwrap(),
            w.ledger.book().store().next_posting_seq().unwrap(),
        );
        let ctx = w.ctx(op);
        let expected = model_applies(&mut model, legs);
        let got = w.ledger.transfer(&ctx, &transfer);
        let multi = legs.len() > 1;
        match got {
            Ok(accepted) => {
                assert!(
                    expected,
                    "the kernel took a unit the model refuses: {legs:?}"
                );
                // One Transfer posting per leg, consecutive, settled.
                let first = before.1;
                let seqs: Vec<u64> = (first..first + legs.len() as u64).collect();
                assert_eq!(accepted.value, seqs);
                assert_eq!(accepted.status.unwrap().state, State::Settled);
                assert_eq!(w.ledger.operation(&ctx.op).unwrap().state(), State::Settled);
                tally.applied.fetch_add(1, Ordering::Relaxed);
                if multi {
                    tally.multi_applied.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(r) => {
                assert!(
                    !expected,
                    "the model takes a unit refused with {r:?}: {legs:?}"
                );
                assert_eq!(r, Refusal::InsufficientAvailable);
                // No partial application: nothing of the unit is anywhere.
                assert_eq!(
                    (
                        w.ledger.book().root().unwrap(),
                        w.ledger.book().store().next_posting_seq().unwrap()
                    ),
                    before
                );
                assert!(w.ledger.operation(&ctx.op).is_none());
                assert!(w.ledger.state().record(&ctx.op).is_none());
                tally.refused.fetch_add(1, Ordering::Relaxed);
                if multi {
                    tally.multi_refused.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // INV-3: every Customer balance is the model's, and none is negative.
        for (i, expected) in model.iter().enumerate() {
            let got = w.available(i);
            assert!(got >= 0, "customer {i} overdrawn: {got}");
            assert_eq!(got, *expected, "customer {i}");
        }
        assert_eq!(w.ledger.trial_balance(&usd()), Ok(0));
        // INV-1: every posting the kernel accepted since balances per asset.
        let log = delta_log(w.ledger.book().store()).unwrap();
        for record in &log[log_seen..] {
            for t in record.delta.transfers.values() {
                assert_eq!(
                    PostingMeta::kind_of(&t.user_data),
                    Some(PostingKind::Transfer)
                );
                assert_eq!(t.entries.len(), 2);
                let (d, c) = (&t.entries[0], &t.entries[1]);
                assert_eq!(
                    (d.direction, c.direction),
                    (Direction::Debit, Direction::Credit)
                );
                assert_eq!(d.ledger, c.ledger, "one asset per posting");
                assert_eq!(d.amount, c.amount, "debit and credit commit to one value");
            }
        }
        log_seen = log.len();
    }
}

fn units(var: &str, default: u32) -> u32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// How many parts the `MemStore` units are split into, to run in parallel.
const PARTS: u32 = 4;

fn run(file: bool, part: u8, total: u32, parts: u32) {
    let tally = Tally::default();
    let books = total.div_ceil(UNITS_PER_BOOK as u32).div_ceil(parts).max(1);
    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: books,
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(
            RngAlgorithm::ChaCha,
            &[part + if file { 0x80 } else { 0x08 }; 32],
        ),
    );
    runner
        .run(&a_case(), |case| {
            let keys = KeyRing::from_root([0xF8; 32]);
            if file {
                let book = Book::bootstrap(
                    FileStore::in_memory().unwrap(),
                    FileVault::in_memory().unwrap(),
                    keys,
                )
                .unwrap();
                check_case(book, &case, &tally);
            } else {
                let book = Book::bootstrap(MemStore::new(), MemVault::new(), keys).unwrap();
                check_case(book, &case, &tally);
            }
            Ok(())
        })
        .unwrap();
    // Both outcomes occur, for multi-leg units too.
    let (applied, refused) = (tally.applied.into_inner(), tally.refused.into_inner());
    let (multi_applied, multi_refused) = (
        tally.multi_applied.into_inner(),
        tally.multi_refused.into_inner(),
    );
    assert_eq!(applied + refused, books as usize * UNITS_PER_BOOK);
    assert!(
        applied > 0 && refused > 0,
        "{applied} applied, {refused} refused"
    );
    assert!(
        multi_applied > 0 && multi_refused > 0,
        "{multi_applied} multi-leg applied, {multi_refused} refused"
    );
}

#[test]
fn ac_b3_1_random_units_apply_whole_or_not_at_all__mem_store_part_1() {
    run(false, 1, units("F008_UNITS", 10_000), PARTS);
}

#[test]
fn ac_b3_1_random_units_apply_whole_or_not_at_all__mem_store_part_2() {
    run(false, 2, units("F008_UNITS", 10_000), PARTS);
}

#[test]
fn ac_b3_1_random_units_apply_whole_or_not_at_all__mem_store_part_3() {
    run(false, 3, units("F008_UNITS", 10_000), PARTS);
}

#[test]
fn ac_b3_1_random_units_apply_whole_or_not_at_all__mem_store_part_4() {
    run(false, 4, units("F008_UNITS", 10_000), PARTS);
}

#[test]
fn ac_b3_1_random_units_apply_whole_or_not_at_all__file_store_part_1() {
    run(true, 1, units("F008_UNITS_FILE", 1_000), 1);
}
