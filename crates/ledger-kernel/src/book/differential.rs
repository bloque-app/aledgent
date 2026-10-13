// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Differential tests against CipherClerk's own reference (`F-001` T-001-08).
//!
//! 1. **Differential.** Random units of creations and chains are applied to a
//!    book over `MemStore`, to a book over `FileStore`, and, call by call, to
//!    the kernel's `MemLedger` with a full-knowledge oracle. At every unit the
//!    kernel's results and the composite roots are equal, and equal
//!    `composite_state_root` over the full leaf sets.
//! 2. **Chain by chain.** Applying a unit's chains one kernel call at a time
//!    gives the same results and root as one CipherClerk batch over all of
//!    them (`F-001` §7).
//! 3. **Golden vectors.** Fixed units with a fixed core root reproduce their
//!    recorded roots on both stores.
//! 4. **Root versions** (`DEC-25`). The differential also runs
//!    every unit on a `MemStore` of the other root version: the two books make
//!    the same decisions, kernel results and commits, the v1 book's roots are
//!    the kernel reference's, and each book's roots are its own version's
//!    recomputation over the same leaves.
//!
//! The reference oracle is built from a plaintext shadow kept here, apart
//! from the vault and the simulation the book uses, so the two are checked
//! against each other rather than against themselves.
//!
//! The differential runs `LEDGER_KERNEL_DIFF_CASES` cases (10 000 by
//! default), split across four tests so they run in parallel. `FileStore`
//! and `FileVault` run on redb's in-memory backend here, which is the same
//! code without the fsync; the file-backed tests cover the disk.

use super::*;
use crate::account::{AssetTag, BalanceRule, Role};
use crate::posting::{EntryDraft, OperationRef, PostingKind, PostingMeta};
use crate::store::{FileStore, MemStore};
use crate::testkit::{ROOT, v};
use crate::vault::{FileVault, MemVault};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use cipher_clerk::helpers::{MemLedger, MemOracle};
use cipher_clerk::{AccountFlags, Direction};
use core::sync::atomic::{AtomicUsize, Ordering};
use curve25519_dalek::scalar::Scalar;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

// ---------------------------------------------------------------------------
// The plaintext shadow and the reference oracle.

/// Per (account, layer): debit and credit value and blinding sums.
#[derive(Clone, Default)]
struct Shadow {
    rows: BTreeMap<(AccountId, u8), (u128, Scalar, u128, Scalar)>,
}

impl Shadow {
    /// Apply `t`'s entries and record in `oracle` the opening of each touched
    /// flagged account's net after it (credit-normal: cr − dr).
    fn apply(&mut self, t: &Transfer, flagged: &BTreeSet<AccountId>, oracle: &mut MemOracle) {
        let mut touched = Vec::new();
        for e in &t.entries {
            let (value, blinding) = oracle.openings[&e.amount.0];
            let b = Scalar::from_canonical_bytes(blinding.0).unwrap();
            let row = self
                .rows
                .entry((e.account_id, e.layer.as_byte()))
                .or_insert((0, Scalar::ZERO, 0, Scalar::ZERO));
            match e.direction {
                Direction::Debit => {
                    row.0 += u128::from(value);
                    row.1 += b;
                }
                Direction::Credit => {
                    row.2 += u128::from(value);
                    row.3 += b;
                }
            }
            if !touched.contains(&(e.account_id, e.layer.as_byte())) {
                touched.push((e.account_id, e.layer.as_byte()));
            }
        }
        for key in touched {
            if !flagged.contains(&key.0) {
                continue;
            }
            let (dv, db, cv, cb) = self.rows[&key];
            if cv >= dv {
                let net = u64::try_from(cv - dv).unwrap();
                let blinding =
                    cipher_clerk::crypto::Blinding::from_bytes((cb - db).to_bytes()).unwrap();
                oracle.commit_with_blinding(net, blinding);
            }
        }
    }
}

/// The kernel's `MemLedger`, driven call by call like the book.
struct Reference {
    ledger: MemLedger,
    oracle: MemOracle,
    shadow: Shadow,
    flagged: BTreeSet<AccountId>,
}

impl Reference {
    fn new(journal: Journal) -> Self {
        let mut ledger = MemLedger::new();
        let mut oracle = MemOracle::new();
        ledger.put_journal(journal, &mut oracle);
        Self {
            ledger,
            oracle,
            shadow: Shadow::default(),
            flagged: BTreeSet::new(),
        }
    }

    /// Run one unit's kernel calls; return the chain results per step.
    fn run(&mut self, built: &[BuiltStep], seed_ts: u64) -> Vec<Vec<EventResult>> {
        let total: usize = built
            .iter()
            .map(|s| s.creations.len() + s.chain.len())
            .sum();
        let base = seed_ts.saturating_sub(total as u64);
        let mut position = 0u64;
        let mut out = Vec::new();
        for step in built {
            let saved = (
                self.ledger.clone(),
                self.shadow.clone(),
                self.flagged.clone(),
            );
            let c = step.creations.len() as u64;
            let created = apply_account_creations(
                &mut self.ledger,
                &step.creations,
                &mut self.oracle,
                base + position + c,
            );
            position += c;
            assert!(created.iter().all(|r| r.status == EventStatus::Created));
            for ca in &step.creations {
                if ca
                    .account
                    .flags
                    .contains(AccountFlags::BALANCE_MUST_NOT_BE_NEGATIVE)
                {
                    self.flagged.insert(ca.account.id);
                }
            }
            for b in &step.chain {
                for o in &b.openings {
                    self.oracle.record(o.amount, o.value, o.blinding);
                }
            }
            // Register the nets of the whole chain, assuming it succeeds.
            let chain: Vec<Transfer> = step.chain.iter().map(|b| b.transfer.clone()).collect();
            for t in &chain {
                self.shadow.apply(t, &self.flagged, &mut self.oracle);
            }
            let k = chain.len() as u64;
            let results = if chain.is_empty() {
                Vec::new()
            } else {
                apply_batch(
                    &mut self.ledger,
                    &chain,
                    &mut self.oracle,
                    base + position + k,
                )
            };
            position += k;
            if results.iter().any(|r| r.status != EventStatus::Created) {
                (self.ledger, self.shadow, self.flagged) = saved;
            }
            out.push(results);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Random units.

const ASSETS: [u32; 2] = [1, 2];

/// Account `x` of `asset`: the issuer, one of four customers or one of two
/// holding positions.
fn account(asset: u32, x: u8) -> KernelAccountSpec {
    match x % 7 {
        0 => KernelAccountSpec::new(
            Role::Issuance,
            [0xEE; 16],
            AssetTag(asset),
            BalanceRule::Unchecked,
        ),
        n @ 1..=4 => KernelAccountSpec::new(
            Role::LedgerAccount,
            [n; 16],
            AssetTag(asset),
            BalanceRule::NeverNegative,
        ),
        n => KernelAccountSpec::new(
            Role::Holding,
            [n; 16],
            AssetTag(asset),
            BalanceRule::NeverNegative,
        ),
    }
}

/// One generated operation: a shape and its parameters.
type Op = (u8, u8, u8, u8, u16, u16);

fn ops() -> impl Strategy<Value = Vec<Vec<Vec<Op>>>> {
    let op = (
        any::<u8>(),
        any::<u8>(),
        any::<u8>(),
        any::<u8>(),
        any::<u16>(),
        any::<u16>(),
    );
    let step = prop::collection::vec(op, 1..=3);
    let unit = prop::collection::vec(step, 1..=3);
    prop::collection::vec(unit, 1..=4)
}

/// Turns generated operations into units, lazily creating the accounts they
/// touch, as `F-003` will.
struct Planner {
    next_op: u32,
    history: Vec<PostingDraft>,
}

impl Planner {
    fn draft(&mut self, op: &Op) -> Option<PostingDraft> {
        let (shape, a, b, c, v1, v2) = *op;
        let asset = ASSETS[usize::from(a % 2)];
        let val = u64::from(v1 % 150) + 1;
        self.next_op += 1;
        let meta = |kind| {
            PostingMeta::new(
                kind,
                OperationRef([(self.next_op % 251) as u8; 16]),
                self.next_op,
            )
        };
        let draft = match shape % 6 {
            0 => PostingDraft::balanced(
                meta(PostingKind::Issue),
                vec![
                    EntryDraft::debit(account(asset, 0), v(val)),
                    EntryDraft::credit(account(asset, 1 + b % 4), v(val)),
                ],
            ),
            1 | 4 => PostingDraft::balanced(
                meta(PostingKind::Transfer),
                vec![
                    EntryDraft::debit(account(asset, b), v(val)),
                    EntryDraft::credit(account(asset, c), v(val)),
                ],
            ),
            2 => {
                let part = u64::from(v2) % val;
                let mut entries = vec![EntryDraft::debit(account(asset, b), v(val))];
                if part > 0 {
                    entries.push(EntryDraft::credit(account(asset, c), v(part)));
                }
                entries.push(EntryDraft::credit(
                    account(asset, c.wrapping_add(1)),
                    v(val - part),
                ));
                PostingDraft::balanced(meta(PostingKind::Capture), entries)
            }
            3 => {
                let other = u64::from(v2 % 90) + 1;
                PostingDraft::balanced(
                    meta(PostingKind::Conversion),
                    vec![
                        EntryDraft::debit(account(1, b), v(val)),
                        EntryDraft::credit(account(1, c), v(val)),
                        EntryDraft::debit(account(2, c), v(other)),
                        EntryDraft::credit(account(2, b), v(other)),
                    ],
                )
            }
            _ => {
                // Replay an earlier posting: the kernel must refuse it.
                if self.history.is_empty() {
                    return None;
                }
                return Some(self.history[usize::from(b) % self.history.len()].clone());
            }
        }
        .ok()?;
        let draft = if c % 5 == 0 {
            draft.with_closing_debit()
        } else {
            draft
        };
        self.history.push(draft.clone());
        Some(draft)
    }

    /// The next unit, given the accounts the book already holds.
    fn unit<S: LeafStore, V: VaultBackend>(
        &mut self,
        book: &Book<S, V>,
        steps: &[Vec<Op>],
    ) -> Unit {
        let mut pending: BTreeSet<AccountId> = BTreeSet::new();
        let mut out = Unit::default();
        for ops in steps {
            let mut step = Step::default();
            for op in ops {
                let Some(d) = self.draft(op) else { continue };
                let mut needed = Vec::new();
                for e in d.entries() {
                    let id = e.account.id();
                    if step.creations.iter().any(|c| c.id() == id) || needed.contains(&e.account) {
                        continue;
                    }
                    if book.kernel_account(&id).unwrap().is_none() {
                        needed.push(e.account);
                    }
                }
                // An account another step of this unit creates may or may
                // not exist when this step runs: leave such postings out.
                if needed.iter().any(|a| pending.contains(&a.id())) {
                    continue;
                }
                step.creations.extend(needed);
                step.chain.push(d);
            }
            pending.extend(step.creations.iter().map(KernelAccountSpec::id));
            out.steps.push(step);
        }
        out
    }
}

/// How often each class was seen, so a run can show it covered them all:
/// applied, insufficient, duplicate, closed, chain-aborted.
static SEEN: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];

fn count(applied: &Applied) {
    for e in applied.steps.iter().flat_map(|s| s.events.iter()) {
        let i = match e {
            EventClass::Applied { .. } => 0,
            EventClass::Insufficient { .. } => 1,
            EventClass::Duplicate => 2,
            EventClass::Closed => 3,
            EventClass::ChainAborted { .. } => 4,
        };
        SEEN[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// Apply `script` to a MemStore book, a FileStore book and the reference,
/// comparing at every unit.
fn differential(script: &[Vec<Vec<Op>>]) -> Result<(), TestCaseError> {
    use compact_smt::Version;
    let mut mem =
        Book::bootstrap(MemStore::new(), MemVault::new(), KeyRing::from_root(ROOT)).unwrap();
    let other_version = match Version::ACTIVE {
        Version::V1 => Version::V2,
        Version::V2 => Version::V1,
    };
    let mut other = Book::bootstrap(
        MemStore::with_version(other_version),
        MemVault::new(),
        KeyRing::from_root(ROOT),
    )
    .unwrap();
    let mut file = Book::bootstrap(
        FileStore::in_memory().unwrap(),
        FileVault::in_memory().unwrap(),
        KeyRing::from_root(ROOT),
    )
    .unwrap();
    let journal = Journal::new(mem.journal(), mem.keys().registrar_key(), 0);
    let mut reference = Reference::new(journal);
    let v1_root = |mem: &Book<MemStore, MemVault>, other: &Book<MemStore, MemVault>| {
        if Version::ACTIVE == Version::V1 {
            mem.root().unwrap()
        } else {
            other.root().unwrap()
        }
    };
    prop_assert_eq!(v1_root(&mem, &other), reference.ledger.root());

    let mut planner = Planner {
        next_op: 0,
        history: Vec::new(),
    };
    let mut seed = 1_000u64;
    for steps in script {
        let unit = planner.unit(&mem, steps);
        seed += 50;
        let built = mem.build(&unit);
        let a = mem
            .apply(&unit, seed)
            .expect("no binding fault on the MemStore book");
        let b = file
            .apply(&unit, seed)
            .expect("no binding fault on the FileStore book");
        let c = other
            .apply(&unit, seed)
            .expect("no binding fault on the other version's book");
        let expected = reference.run(&built, seed);

        prop_assert_eq!(&a, &b);
        count(&a);
        let got: Vec<Vec<EventResult>> = a.steps.iter().map(|s| s.kernel_results.clone()).collect();
        prop_assert_eq!(got, expected);
        // The same transition in the other root version: everything but the roots agrees.
        prop_assert_eq!(&a.steps, &c.steps);
        prop_assert_eq!(a.seq, c.seq);
        prop_assert_ne!(a.root, c.root);
        prop_assert_eq!(v1_root(&mem, &other), reference.ledger.root());
        prop_assert_eq!(a.root, mem.store().recomputed_root());
        prop_assert_eq!(c.root, other.store().recomputed_root());
        prop_assert_eq!(file.root().unwrap(), a.root);
    }
    other.verify_openings().unwrap();
    mem.verify_openings().unwrap();
    file.verify_openings().unwrap();
    Ok(())
}

fn cases() -> u32 {
    std::env::var("LEDGER_KERNEL_DIFF_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

fn run_differential(part: u32) {
    let per = cases().div_ceil(4);
    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: per,
            failure_persistence: None,
            ..Config::default()
        },
        proptest::test_runner::TestRng::from_seed(
            proptest::test_runner::RngAlgorithm::ChaCha,
            &[part as u8; 32],
        ),
    );
    runner.run(&ops(), |script| differential(&script)).unwrap();
    if per >= 100 {
        for (i, n) in SEEN.iter().enumerate() {
            assert!(
                n.load(Ordering::Relaxed) > 0,
                "class {i} never occurred: the generator lost coverage"
            );
        }
    }
}

#[test]
fn differential_against_the_kernel_reference_part_1() {
    run_differential(1);
}

#[test]
fn differential_against_the_kernel_reference_part_2() {
    run_differential(2);
}

#[test]
fn differential_against_the_kernel_reference_part_3() {
    run_differential(3);
}

#[test]
fn differential_against_the_kernel_reference_part_4() {
    run_differential(4);
}

// ---------------------------------------------------------------------------
// Chain by chain equals one batch.

/// Every account of both assets, created and funded in one unit.
fn setup_unit() -> Unit {
    let mut creations = Vec::new();
    let mut chain = Vec::new();
    for (i, asset) in ASSETS.iter().enumerate() {
        for x in 0..7u8 {
            creations.push(account(*asset, x));
        }
        for x in 1..=4u8 {
            let ordinal = (i as u32) * 10 + u32::from(x);
            chain.push(
                PostingDraft::balanced(
                    PostingMeta::new(PostingKind::Issue, OperationRef([0xAB; 16]), ordinal),
                    vec![
                        EntryDraft::debit(account(*asset, 0), v(500)),
                        EntryDraft::credit(account(*asset, x), v(500)),
                    ],
                )
                .unwrap(),
            );
        }
    }
    Unit {
        steps: vec![Step { creations, chain }],
    }
}

fn chain_by_chain(chains: &[Vec<Op>]) -> Result<(), TestCaseError> {
    let mut book =
        Book::bootstrap(MemStore::new(), MemVault::new(), KeyRing::from_root(ROOT)).unwrap();
    let journal = Journal::new(book.journal(), book.keys().registrar_key(), 0);
    let mut reference = Reference::new(journal);
    let setup = setup_unit();
    let built = book.build(&setup);
    book.apply(&setup, 500).unwrap();
    reference.run(&built, 500);
    prop_assert_eq!(
        book.root().unwrap(),
        crate::testkit::kernel_reference_root(&reference.ledger)
    );

    let mut planner = Planner {
        next_op: 1_000,
        history: Vec::new(),
    };
    let unit = Unit {
        steps: chains
            .iter()
            .map(|ops| Step {
                creations: Vec::new(),
                chain: ops.iter().filter_map(|op| planner.draft(op)).collect(),
            })
            .collect(),
    };
    let built = book.build(&unit);
    let applied = book.apply(&unit, 9_000).unwrap();

    // One batch over every chain, each terminated, with the unit's seed. The
    // reference oracle follows the outcome of each chain in turn.
    let mut all = Vec::new();
    for (step, outcome) in built.iter().zip(&applied.steps) {
        let saved = reference.shadow.clone();
        for b in &step.chain {
            for o in &b.openings {
                reference.oracle.record(o.amount, o.value, o.blinding);
            }
            reference
                .shadow
                .apply(&b.transfer, &reference.flagged, &mut reference.oracle);
            all.push(b.transfer.clone());
        }
        if !outcome.applied {
            reference.shadow = saved;
        }
    }
    let one_batch = if all.is_empty() {
        Vec::new()
    } else {
        apply_batch(&mut reference.ledger, &all, &mut reference.oracle, 9_000)
    };
    let chain_by_chain: Vec<EventResult> = applied
        .steps
        .iter()
        .flat_map(|s| s.kernel_results.iter().copied())
        .collect();
    prop_assert_eq!(chain_by_chain, one_batch);
    prop_assert_eq!(
        applied.root,
        crate::testkit::kernel_reference_root(&reference.ledger)
    );
    Ok(())
}

#[test]
fn req_ker_10_chain_by_chain_equals_one_kernel_batch() {
    let op = (
        any::<u8>(),
        any::<u8>(),
        any::<u8>(),
        any::<u8>(),
        any::<u16>(),
        any::<u16>(),
    );
    let chains = prop::collection::vec(prop::collection::vec(op, 1..=3), 1..=5);
    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: cases().div_ceil(10),
            failure_persistence: None,
            ..Config::default()
        },
        proptest::test_runner::TestRng::from_seed(
            proptest::test_runner::RngAlgorithm::ChaCha,
            &[0xCB; 32],
        ),
    );
    runner.run(&chains, |c| chain_by_chain(&c)).unwrap();
}

// ---------------------------------------------------------------------------
// Golden vectors.

fn two_entry(
    kind: PostingKind,
    op: u8,
    ordinal: u32,
    from: KernelAccountSpec,
    to: KernelAccountSpec,
    value: u64,
) -> PostingDraft {
    PostingDraft::balanced(
        PostingMeta::new(kind, OperationRef([op; 16]), ordinal),
        vec![
            EntryDraft::debit(from, v(value)),
            EntryDraft::credit(to, v(value)),
        ],
    )
    .unwrap()
}

fn only(chain: Vec<PostingDraft>) -> Step {
    Step {
        creations: Vec::new(),
        chain,
    }
}

/// A fixed history: fund, move and refuse, convert, reserve and capture for
/// less with a closing debit, touch the closed position, replay. Its roots
/// after each unit are recorded in [`GOLDEN_ROOTS`].
fn golden_units() -> Vec<Unit> {
    use PostingKind::*;
    let (i1, c1, c2, h1) = (account(1, 0), account(1, 1), account(1, 2), account(1, 5));
    let (i2, d1, d2) = (account(2, 0), account(2, 1), account(2, 2));
    let conversion = PostingDraft::balanced(
        PostingMeta::new(Conversion, OperationRef([3; 16]), 0),
        vec![
            EntryDraft::debit(c1, v(50)),
            EntryDraft::credit(c2, v(50)),
            EntryDraft::debit(d2, v(7)),
            EntryDraft::credit(d1, v(7)),
        ],
    )
    .unwrap();
    let capture_for_less = PostingDraft::balanced(
        PostingMeta::new(Capture, OperationRef([4; 16]), 1),
        vec![
            EntryDraft::debit(h1, v(60)),
            EntryDraft::credit(c2, v(45)),
            EntryDraft::credit(c1, v(15)),
        ],
    )
    .unwrap()
    .with_closing_debit();
    vec![
        setup_unit(),
        Unit {
            steps: vec![
                only(vec![
                    two_entry(Transfer, 1, 0, c1, c2, 120),
                    two_entry(Transfer, 1, 1, c2, c1, 20),
                ]),
                only(vec![two_entry(Transfer, 2, 0, c1, c2, 10_000)]),
            ],
        },
        Unit {
            steps: vec![only(vec![conversion])],
        },
        Unit {
            steps: vec![
                only(vec![two_entry(Reserve, 4, 0, c1, h1, 60)]),
                only(vec![capture_for_less]),
                only(vec![two_entry(Transfer, 4, 2, c1, h1, 1)]),
                only(vec![
                    two_entry(Issue, 5, 0, i1, c1, 1),
                    two_entry(Issue, 5, 1, i2, d1, 1),
                ]),
            ],
        },
        Unit {
            steps: vec![only(vec![two_entry(Transfer, 1, 0, c1, c2, 120)])],
        },
    ]
}

/// The recorded roots of the build's root version.
fn golden_roots() -> [&'static str; 5] {
    match compact_smt::Version::ACTIVE {
        compact_smt::Version::V1 => GOLDEN_ROOTS,
        compact_smt::Version::V2 => GOLDEN_ROOTS_V2,
    }
}

/// The same history's v2 roots (`DEC-25`): the same leaves, hashed compact.
const GOLDEN_ROOTS_V2: [&str; 5] = [
    "48a56e20c86cea23cf11419451e303ac7293f74d4e75dc9c4ae1de6e04e34249",
    "41d874a6ea4e125b12ab5f08c9787413c85b8fe8ec740802ff746732d90f826f",
    "a532acbacb3028b54a729d892be06f8db8ee155a37be2aa68cc454d4a47e01c3",
    "f7273cd297706c7b17f77064cb8d0dbfcfd1bef42a059a2bfb350dded6aa5ac4",
    // The replay is refused as a duplicate: nothing changes.
    "f7273cd297706c7b17f77064cb8d0dbfcfd1bef42a059a2bfb350dded6aa5ac4",
];

/// The recorded roots, one per golden unit, in hex. Re-recorded by `T-031-09`, when kernel
/// account ids and transfer ids took their partition in their top sixteen bits (`REQ-RT-9`).
const GOLDEN_ROOTS: [&str; 5] = [
    "a9268a3f5a06ffed2e4ccbb1da96f7e860030a9d9d00f902b671749f9d676c1c",
    "9eeeba2d5ccc96670f0111059b9ed6e29f1a5bcf8b44a398549bea6fc460b649",
    "42dd08863f178e732eefe25aab0e29653807b2605e72c75c6dcd857f96332d6d",
    "15f9d6d99bb5e8c00132f9342b245fc8b1f44a15320493b79f0bc7556a23309d",
    // The replay is refused as a duplicate: nothing changes.
    "15f9d6d99bb5e8c00132f9342b245fc8b1f44a15320493b79f0bc7556a23309d",
];

/// Which steps of each golden unit apply.
const GOLDEN_APPLIED: [&[bool]; 5] = [
    &[true],
    &[true, false],
    &[true],
    &[true, true, false, true],
    &[false],
];

fn hex(b: &[u8; 32]) -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    for x in b {
        write!(s, "{x:02x}").unwrap();
    }
    s
}

fn golden_on<S: LeafStore, V: VaultBackend>(mut book: Book<S, V>) {
    let mut seed = 10_000;
    let mut roots = Vec::new();
    for (u, applied) in golden_units().iter().zip(GOLDEN_APPLIED) {
        seed += 100;
        let a = book.apply(u, seed).unwrap();
        let got: Vec<bool> = a.steps.iter().map(|s| s.applied).collect();
        assert_eq!(got, applied);
        roots.push(hex(&a.root));
    }
    assert_eq!(roots, golden_roots());
    book.verify_openings().unwrap();
}

/// Both versions' golden roots on one binary: the v1 vectors stay what they were, and the v2
/// vectors are the same history's (`DEC-25`).
#[test]
fn req_rt_3_golden_vectors_in_both_root_versions() {
    for (version, expected) in [
        (compact_smt::Version::V1, GOLDEN_ROOTS),
        (compact_smt::Version::V2, GOLDEN_ROOTS_V2),
    ] {
        let mut book = Book::bootstrap(
            MemStore::with_version(version),
            MemVault::new(),
            KeyRing::from_root(ROOT),
        )
        .unwrap();
        let mut seed = 10_000;
        let mut roots = Vec::new();
        for u in golden_units() {
            seed += 100;
            roots.push(hex(&book.apply(&u, seed).unwrap().root));
        }
        assert_eq!(roots, expected, "{version:?}");
    }
}

#[test]
fn req_rt_3_golden_vectors_reproduce_their_roots_on_mem_store() {
    golden_on(Book::bootstrap(MemStore::new(), MemVault::new(), KeyRing::from_root(ROOT)).unwrap());
}

#[test]
fn req_rt_3_golden_vectors_reproduce_their_roots_on_file_store() {
    let dir = tempfile::tempdir().unwrap();
    golden_on(
        Book::bootstrap(
            FileStore::open(dir.path().join("book.redb")).unwrap(),
            FileVault::open(dir.path().join("vault.redb")).unwrap(),
            KeyRing::from_root(ROOT),
        )
        .unwrap(),
    );
}
