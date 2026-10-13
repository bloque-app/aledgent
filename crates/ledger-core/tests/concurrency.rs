// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The randomised concurrency test of the reservation model (`F-004` T-004-07, `AC-B2.1`, `INV-3`,
//! `INV-5`, `REQ-KER-4`).
//!
//! The core is a single writer: one lane per Agent. "Concurrent" therefore means any order of
//! arrival and any split into apply units and chains. Each case generates a small book (four
//! customers, one asset, the issuance counterparty), and a few dozen commands — fund, transfer,
//! reserve, capture, release — over a small pool of operation references, so that captures and
//! releases also hit reservations that are unknown, already closed, or over-captured. It shuffles
//! the commands, splits them at random into units of one to three steps, each step one chain of one
//! to three commands, and applies them through `F-001`'s book with the reservation engine
//! ([`CHOSEN`]), exactly as the service does, without the authority layer.
//!
//! A plaintext model runs the same schedule in sequence order and predicts, step by step, what the
//! kernel will do: a chain applies whole, or not at all if any posting of it would leave a
//! never-negative account (a customer, a holding) below zero or touch a closed holding. After every
//! unit the test checks:
//!
//! - every step applied exactly when the model says it does: so exactly as many reserves succeed
//!   as the balance covers, in the model's sequence order (`AC-B2.1`);
//! - every kernel balance equals `F-015`'s [`ShadowBook`] fed with the applied postings, and the
//!   trial balance is zero (`INV-5`);
//! - no customer's `available` is ever negative (`INV-3`);
//! - the index agrees with the kernel's holding positions, and `reserved` with the model
//!   (`INV-6`, `REQ-BA-6`);
//!
//! and at the end of each case that no entry of the whole delta log is on any layer but `Settled`,
//! and no kernel account holds anything on `Pending` or `Encumbrance`.
//!
//! It runs on both in-process stores: `MemStore` and `FileStore` (redb's in-memory backend, the
//! same code without the fsync, as `F-001`'s differential test does). `F004_CONCURRENCY_CASES`
//! sets the number of cases per store (default 10 000), split in parts that run in parallel.

// Test names end in `__<store>_part_<n>`, after the criterion they verify.
#![allow(non_snake_case)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};

use cipher_clerk::{Account, AccountFlags, AccountId as KernelAccountId, Direction, Layer};
use conformance::harness::delta_log;
use conformance::tools::ShadowBook;
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::id::{AccountId, OperationId};
use ledger_core::reservation::{
    CHOSEN, HoldingState, IndexChange, Position, ReservationBook, ReservationIndex, UnitBuilder,
};
use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{BindingFault, Book, Step, Unit};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::{FileStore, LeafStore, MemStore};
use ledger_kernel::vault::{FileVault, MemVault, VaultBackend};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

const TAG: AssetTag = AssetTag(1);
const CUSTOMERS: u8 = 4;
/// The pool of operation references reserves draw from.
const OPS: u8 = 10;
/// Commands per case.
const COMMANDS: usize = 36;

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

fn nzu(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).unwrap()
}

/// One command, as a caller would submit it.
#[derive(Clone, Copy, Debug)]
enum Cmd {
    /// Issue value into a customer.
    Fund { to: u8, value: u64 },
    /// An internal transfer between customers: it competes with reserves for available.
    Transfer { from: u8, to: u8, value: u64 },
    /// Reserve for operation `op`.
    Reserve { op: u8, payer: u8, value: u64 },
    /// Capture operation `op` into a customer.
    Capture { op: u8, to: u8, cleared: u64 },
    /// Release operation `op`.
    Release { op: u8 },
}

fn a_cmd() -> impl Strategy<Value = Cmd> {
    let c = 1..=CUSTOMERS;
    prop_oneof![
        3 => (c.clone(), 1..=1_000u64).prop_map(|(to, value)| Cmd::Fund { to, value }),
        2 => (c.clone(), 1..CUSTOMERS, 1..=300u64).prop_map(|(from, d, value)| Cmd::Transfer {
            from,
            to: (from - 1 + d) % CUSTOMERS + 1,
            value,
        }),
        5 => (0..OPS, c.clone(), 1..=300u64)
            .prop_map(|(op, payer, value)| Cmd::Reserve { op, payer, value }),
        3 => (0..OPS, c, 1..=450u64).prop_map(|(op, to, cleared)| Cmd::Capture { op, to, cleared }),
        2 => (0..OPS).prop_map(|op| Cmd::Release { op }),
    ]
}

/// A schedule: units of steps of commands. The commands are shuffled, then split: chain sizes
/// and steps per unit are drawn alongside.
type Schedule = Vec<Vec<Vec<Cmd>>>;

fn a_schedule() -> impl Strategy<Value = Schedule> {
    (
        vec(a_cmd(), COMMANDS).prop_shuffle(),
        vec(1..=3usize, COMMANDS),
        vec(1..=3usize, COMMANDS),
    )
        .prop_map(|(cmds, chains, steps)| {
            let mut chained: Vec<Vec<Cmd>> = Vec::new();
            let mut rest = cmds.as_slice();
            for n in chains {
                if rest.is_empty() {
                    break;
                }
                let (head, tail) = rest.split_at(n.min(rest.len()));
                chained.push(head.to_vec());
                rest = tail;
            }
            let mut units: Schedule = Vec::new();
            let mut rest = chained.as_slice();
            for n in steps {
                if rest.is_empty() {
                    break;
                }
                let (head, tail) = rest.split_at(n.min(rest.len()));
                units.push(head.to_vec());
                rest = tail;
            }
            units
        })
}

fn issuance() -> KernelAccountSpec {
    KernelAccountSpec::new(Role::Issuance, [0xEE; 16], TAG, BalanceRule::Unchecked)
}

fn customer(n: u8) -> Position {
    let id = AccountId::mint(&OperationId::from_ref(OperationRef([0xA0 + n; 16])), 0);
    Position {
        account: id,
        spec: KernelAccountSpec::new(
            Role::LedgerAccount,
            *id.as_bytes(),
            TAG,
            BalanceRule::NeverNegative,
        ),
    }
}

fn op(slot: u8) -> OperationId {
    let mut b = [0x0F; 16];
    b[15] = slot;
    OperationId::from_ref(OperationRef(b))
}

/// What the whole run saw, to show both outcomes of a reserve occur.
#[derive(Default)]
struct Tally {
    reserves_applied: AtomicUsize,
    reserves_refused_by_kernel: AtomicUsize,
    steps: AtomicUsize,
    steps_applied: AtomicUsize,
    refused_at_build: AtomicUsize,
}

/// The plaintext model: nets, closed accounts, and the open reservations.
#[derive(Clone, Default)]
struct Model {
    nets: BTreeMap<KernelAccountId, i128>,
    never_negative: BTreeSet<KernelAccountId>,
    closed: BTreeSet<KernelAccountId>,
}

impl Model {
    /// Run a chain on the model: `true`, with the model updated, if the kernel will apply it.
    fn chain(&mut self, chain: &[PostingDraft], creations: &[KernelAccountSpec]) -> bool {
        let before = self.clone();
        for c in creations {
            if c.rule == BalanceRule::NeverNegative {
                self.never_negative.insert(c.id());
            }
        }
        for p in chain {
            let mut touched = BTreeSet::new();
            for e in p.entries() {
                let id = e.account.id();
                if self.closed.contains(&id) {
                    *self = before;
                    return false;
                }
                let v = i128::from(e.value.get());
                let net = self.nets.entry(id).or_insert(0);
                match e.side {
                    Direction::Credit => *net += v,
                    Direction::Debit => *net -= v,
                }
                touched.insert(id);
            }
            if touched
                .iter()
                .any(|id| self.never_negative.contains(id) && self.nets[id] < 0)
            {
                *self = before;
                return false;
            }
            if p.closes_debited() {
                for e in p.entries() {
                    if e.side == Direction::Debit {
                        self.closed.insert(e.account.id());
                    }
                }
            }
        }
        true
    }

    fn net(&self, id: &KernelAccountId) -> i128 {
        self.nets.get(id).copied().unwrap_or(0)
    }
}

/// Every stored kernel account of the book, by id.
fn account<S: LeafStore, V: VaultBackend>(book: &Book<S, V>, id: &KernelAccountId) -> Account {
    book.kernel_account(id)
        .unwrap()
        .expect("the account exists")
}

/// Build one step from its commands. Commands the engine refuses to build are dropped, as the
/// service would refuse them before any kernel call; so are commands on an operation an earlier
/// step of the same unit already touched (`UnitBuilder`: one operation per unit).
fn build(
    cmds: &[Cmd],
    index: &ReservationIndex,
    claimed: &BTreeSet<OperationId>,
    n: &mut u32,
    salt: u8,
    tally: &Tally,
) -> UnitBuilder {
    let mut u = UnitBuilder::new();
    for cmd in cmds {
        *n += 1;
        let plain = |kind, from: KernelAccountSpec, to: KernelAccountSpec, value| {
            let mut r = [salt; 16];
            r[..4].copy_from_slice(&n.to_be_bytes());
            PostingDraft::balanced(
                PostingMeta::new(kind, OperationRef(r), 0),
                vec![
                    EntryDraft::debit(from, nzu(value)),
                    EntryDraft::credit(to, nzu(value)),
                ],
            )
            .unwrap()
        };
        let result = match *cmd {
            Cmd::Fund { to, value } => {
                u.post(plain(
                    PostingKind::Issue,
                    issuance(),
                    customer(to).spec,
                    value,
                ));
                Ok(())
            }
            Cmd::Transfer { from, to, value } => {
                u.post(plain(
                    PostingKind::Transfer,
                    customer(from).spec,
                    customer(to).spec,
                    value,
                ));
                Ok(())
            }
            Cmd::Reserve {
                op: slot,
                payer,
                value,
            } if !claimed.contains(&op(slot)) => {
                CHOSEN.reserve(index, &mut u, op(slot), customer(payer), usd(), nz(value))
            }
            Cmd::Capture {
                op: slot,
                to,
                cleared,
            } if !claimed.contains(&op(slot)) => {
                CHOSEN.capture(index, &mut u, &op(slot), customer(to), nz(cleared))
            }
            Cmd::Release { op: slot } if !claimed.contains(&op(slot)) => {
                CHOSEN.release(index, &mut u, &op(slot))
            }
            _ => Err(ledger_core::Refusal::ClientKeyReused),
        };
        if result.is_err() {
            tally.refused_at_build.fetch_add(1, Ordering::Relaxed);
        }
    }
    u
}

/// Run one schedule on a fresh book; check everything after every unit.
fn check_schedule<S: LeafStore, V: VaultBackend>(
    mut book: Book<S, V>,
    schedule: &Schedule,
    tally: &Tally,
) {
    let customers: Vec<Position> = (1..=CUSTOMERS).map(customer).collect();
    let mut creations = vec![issuance()];
    creations.extend(customers.iter().map(|c| c.spec));
    let genesis = Unit {
        steps: vec![Step {
            creations,
            chain: Vec::new(),
        }],
    };
    book.apply(&genesis, 1).unwrap();

    let mut model = Model::default();
    for c in &customers {
        model.never_negative.insert(c.spec.id());
    }
    let mut index = ReservationIndex::new();
    let mut shadow: ShadowBook<KernelAccountId> = ShadowBook::new();
    let mut n: u32 = 0;
    let mut ts: u64 = 10;

    for (unit_no, unit) in schedule.iter().enumerate() {
        let salt = u8::try_from(unit_no % 200).unwrap() + 1;
        let mut claimed: BTreeSet<OperationId> = BTreeSet::new();
        let mut steps = Vec::new();
        let mut changes = Vec::new();
        let mut predicted = Vec::new();
        for cmds in unit {
            let u = build(cmds, &index, &claimed, &mut n, salt, tally);
            if u.chain().is_empty() {
                continue;
            }
            claimed.extend(u.changes().iter().map(IndexChange::op));
            let built = u
                .finish(|id| book.kernel_account(id).map(|a| a.is_some()))
                .unwrap();
            predicted.push(model.chain(&built.step.chain, &built.step.creations));
            steps.push(built.step);
            changes.push(built.changes);
        }
        if steps.is_empty() {
            continue;
        }
        ts += 100;
        let applied = book
            .apply(
                &Unit {
                    steps: steps.clone(),
                },
                ts,
            )
            .unwrap();
        for (i, outcome) in applied.steps.iter().enumerate() {
            tally.steps.fetch_add(1, Ordering::Relaxed);
            assert_eq!(
                outcome.applied, predicted[i],
                "unit {unit_no} step {i}: the kernel and the model disagree ({:?})",
                outcome.events
            );
            let reserves = changes[i]
                .iter()
                .filter(|c| matches!(c, IndexChange::Open(_)))
                .count();
            if outcome.applied {
                tally.steps_applied.fetch_add(1, Ordering::Relaxed);
                tally
                    .reserves_applied
                    .fetch_add(reserves, Ordering::Relaxed);
                index.apply_all(&changes[i]).unwrap();
                for p in &steps[i].chain {
                    shadow.posting(p).unwrap();
                }
            } else {
                tally
                    .reserves_refused_by_kernel
                    .fetch_add(reserves, Ordering::Relaxed);
            }
        }
        check_unit(&book, &index, &model, &shadow, &customers);
    }
    check_layers(&book);
}

fn check_unit<S: LeafStore, V: VaultBackend>(
    book: &Book<S, V>,
    index: &ReservationIndex,
    model: &Model,
    shadow: &ShadowBook<KernelAccountId>,
    customers: &[Position],
) {
    // INV-5: every entry balanced, so the whole book sums to zero.
    assert_eq!(shadow.trial_balance(TAG.0), 0);
    let mut kernel_sum: i128 = 0;
    let mut ids: Vec<KernelAccountId> = vec![issuance().id()];
    ids.extend(customers.iter().map(|c| c.spec.id()));
    ids.extend(index.iter().map(|s| s.reservation().holding.id()));
    for id in &ids {
        let net = book.settled_net(id).unwrap().unwrap_or(0);
        assert_eq!(net, shadow.net(id, TAG.0), "{id:?}: kernel and shadow");
        assert_eq!(net, model.net(id), "{id:?}: kernel and model");
        kernel_sum += net;
    }
    assert_eq!(kernel_sum, 0, "the trial balance");
    // INV-3 and REQ-BA-6: available never negative; reserved is the model's open sum.
    for c in customers {
        let available = book.settled_net(&c.spec.id()).unwrap().unwrap_or(0);
        assert!(available >= 0, "{:?} overdrawn: {available}", c.account);
        let open: u64 = index
            .open_on(&c.account, &usd())
            .map(|(_, a)| a.get().get())
            .sum();
        assert_eq!(index.reserved(&c.account, &usd()), Minor::new(open));
    }
    // INV-6: the index is what the kernel's holdings hold.
    CHOSEN
        .check_holdings(index, |id| {
            let Some(a) = book.kernel_account(id)? else {
                return Ok::<_, BindingFault>(None);
            };
            Ok(Some(HoldingState {
                net: book.settled_net(id)?.unwrap_or(0),
                closed: a.flags.contains(AccountFlags::CLOSED),
            }))
        })
        .unwrap();
    // A closed holding is closed in the model too, and an open one is not.
    for s in index.iter() {
        let h = s.reservation().holding.id();
        let closed = account(book, &h).flags.contains(AccountFlags::CLOSED);
        assert_eq!(closed, model.closed.contains(&h));
    }
}

/// No entry of the whole log is off the settled layer, no transfer is `PENDING`, and no kernel
/// account holds anything on `Pending` or `Encumbrance`.
fn check_layers<S: LeafStore, V: VaultBackend>(book: &Book<S, V>) {
    let log = delta_log(book.store()).unwrap();
    let mut touched = BTreeSet::new();
    for record in &log {
        assert!(record.delta.pending.is_empty());
        for t in record.delta.transfers.values() {
            assert!(!t.flags.contains(cipher_clerk::TransferFlags::PENDING));
            for e in &t.entries {
                assert_eq!(e.layer, Layer::Settled);
            }
        }
        touched.extend(record.delta.accounts.keys().copied());
    }
    for id in touched {
        let a = account(book, &id);
        let blank = Account::new(
            id,
            a.journal_id,
            a.auth_key,
            a.ledger,
            a.code,
            a.normal_balance,
        );
        for layer in [Layer::Pending, Layer::Encumbrance] {
            let i = layer.as_index();
            assert_eq!(a.balances[i], blank.balances[i], "{id:?} on {layer:?}");
        }
    }
}

fn cases() -> u32 {
    std::env::var("F004_CONCURRENCY_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

/// How many parts each store's cases are split into, to run in parallel.
const PARTS: u32 = 4;

fn run(file: bool, part: u8) {
    let tally = Tally::default();
    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: cases().div_ceil(PARTS).max(1),
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(
            RngAlgorithm::ChaCha,
            &[part + if file { 16 } else { 0 }; 32],
        ),
    );
    runner
        .run(&a_schedule(), |schedule| {
            let keys = KeyRing::from_root([0xC4; 32]);
            if file {
                let book = Book::bootstrap(
                    FileStore::in_memory().unwrap(),
                    FileVault::in_memory().unwrap(),
                    keys,
                )
                .unwrap();
                check_schedule(book, &schedule, &tally);
            } else {
                let book = Book::bootstrap(MemStore::new(), MemVault::new(), keys).unwrap();
                check_schedule(book, &schedule, &tally);
            }
            Ok(())
        })
        .unwrap();
    // Both outcomes of a reserve occur, and most steps apply.
    let applied = tally.reserves_applied.into_inner();
    let refused = tally.reserves_refused_by_kernel.into_inner();
    let (steps, steps_applied) = (tally.steps.into_inner(), tally.steps_applied.into_inner());
    assert!(
        applied > 0 && refused > 0,
        "{applied} applied, {refused} refused"
    );
    assert!(
        steps_applied * 3 > steps && steps_applied < steps,
        "{steps_applied} of {steps} steps applied"
    );
    assert!(tally.refused_at_build.into_inner() > 0);
}

#[test]
fn ac_b2_1_random_schedules_never_overdraw__mem_store_part_1() {
    run(false, 1);
}

#[test]
fn ac_b2_1_random_schedules_never_overdraw__mem_store_part_2() {
    run(false, 2);
}

#[test]
fn ac_b2_1_random_schedules_never_overdraw__mem_store_part_3() {
    run(false, 3);
}

#[test]
fn ac_b2_1_random_schedules_never_overdraw__mem_store_part_4() {
    run(false, 4);
}

#[test]
fn ac_b2_1_random_schedules_never_overdraw__file_store_part_1() {
    run(true, 1);
}

#[test]
fn ac_b2_1_random_schedules_never_overdraw__file_store_part_2() {
    run(true, 2);
}

#[test]
fn ac_b2_1_random_schedules_never_overdraw__file_store_part_3() {
    run(true, 3);
}

#[test]
fn ac_b2_1_random_schedules_never_overdraw__file_store_part_4() {
    run(true, 4);
}
