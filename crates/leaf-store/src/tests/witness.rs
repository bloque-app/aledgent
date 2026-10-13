// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The witness path (`F-006` T-006-04, `DEC-4`): witnesses for random states verify against the
//! root, give the root after from the proofs alone, and are accepted by CipherClerk's own
//! verifier; a tampered leaf, a wrong sibling and a read outside the set are all rejected.

use std::collections::BTreeSet;

use cipher_clerk::crypto::{Amount, Blinding};
use cipher_clerk::state::Opening;
use cipher_clerk::succinct::SuccinctTransitionWitness;
use cipher_clerk::{AccountId, Direction, Layer, Transfer};
use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{Book, Step, Unit};
use ledger_kernel::build::{build_creation, build_transfer};
use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::{MemVault, RowKey};
use ledger_kernel::working_set::UnloadedKey;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

use super::fixture::{Shape, a_schedule, book, keys, nz, op, run};
use crate::BookStore;
use crate::witness::{StepInput, Witness, WitnessError, read_set};

const SHAPE: Shape = Shape {
    customers: 5,
    assets: 1,
};

type B = Book<BookStore, MemVault>;

/// The post-state openings of the never-negative accounts a transfer touches.
fn net_openings(b: &B, t: &Transfer) -> Vec<Opening> {
    let ids: BTreeSet<AccountId> = t.entries.iter().map(|e| e.account_id).collect();
    let mut out = Vec::new();
    for id in ids {
        let acct = b.kernel_account(&id).unwrap().unwrap();
        if BalanceRule::of(&acct) != BalanceRule::NeverNegative {
            continue;
        }
        let (net, bl) = b
            .vault()
            .sums(&RowKey::new(id, Layer::Settled.as_byte()))
            .net(Direction::Credit)
            .unwrap();
        let value = u64::try_from(net).unwrap();
        let blinding = Blinding::from_bytes(bl.to_bytes()).unwrap();
        out.push(Opening {
            amount: Amount::commit(value, &blinding),
            value,
            blinding,
        });
    }
    out
}

struct Case {
    witness: Witness,
    root_before: [u8; 32],
    root_after: [u8; 32],
    payee: AccountId,
}

/// Build the witness of one step from a view taken before it, then apply the step for real.
fn witness_for(
    b: &mut B,
    creations: Vec<KernelAccountSpec>,
    draft: PostingDraft,
    ts: u64,
    drop_from_read_set: Option<AccountId>,
) -> Case {
    let k = keys();
    let journal = b.journal();
    let built = build_transfer(&k, journal, &draft, false);
    let created: Vec<_> = creations
        .iter()
        .map(|c| build_creation(&k, journal, c))
        .collect();
    let events = vec![built.transfer.clone()];
    let view = b.store().view().unwrap();
    let mut rs = read_set(&view, &created, &events).unwrap();
    if let Some(id) = drop_from_read_set {
        rs.accounts.remove(&id);
    }
    let root_before = b.root().unwrap();
    let applied = b
        .apply(
            &Unit {
                steps: vec![Step {
                    creations,
                    chain: vec![draft],
                }],
            },
            ts,
        )
        .unwrap();
    assert!(applied.steps[0].applied);
    let mut openings = built.openings.clone();
    openings.extend(net_openings(b, &built.transfer));
    let payee = built.transfer.entries.last().unwrap().account_id;
    let input = StepInput {
        creations: created,
        creation_seed: ts - 1,
        events,
        batch_seed: ts,
        openings,
    };
    Case {
        witness: Witness::build(&view, &rs, input).unwrap(),
        root_before,
        root_after: applied.root,
        payee,
    }
}

fn transfer(n: u64, from: u8, to: u8, value: u64) -> PostingDraft {
    PostingDraft::balanced(
        PostingMeta::new(PostingKind::Transfer, op(1_000 + n, 0), 0),
        vec![
            EntryDraft::debit(SHAPE.spec(from, 1), nz(value)),
            EntryDraft::credit(SHAPE.spec(to, 1), nz(value)),
        ],
    )
    .unwrap()
}

fn holding(n: u64) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::Holding,
        op(2_000 + n, 0).0,
        AssetTag(1),
        BalanceRule::NeverNegative,
    )
}

fn check(case: &Case) {
    let v = case.witness.verify(case.root_before).unwrap();
    assert_eq!(
        v.root_after, case.root_after,
        "root after from the proofs alone"
    );
    let s = case.witness.to_succinct().unwrap();
    let mut aligned = rkyv::util::AlignedVec::<16>::with_capacity(s.bytes.len());
    aligned.extend_from_slice(&s.bytes);
    let theirs = rkyv::from_bytes::<SuccinctTransitionWitness, rkyv::rancor::Error>(&aligned)
        .expect("the bytes are CipherClerk's witness");
    // CipherClerk's own verifier: panics on any violation.
    theirs.verify_transition(s.chain_root_before, case.root_after);
}

/// A funded book from a random schedule.
fn random_book(seed: u8) -> B {
    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]),
    );
    let schedule =
        proptest::strategy::Strategy::new_tree(&a_schedule(SHAPE, 15), &mut runner).unwrap();
    let schedule = proptest::strategy::ValueTree::current(&schedule);
    let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    run(&mut b, SHAPE, 0, &schedule);
    // Enough on every customer for the steps below.
    let fund: Vec<_> = (1..=SHAPE.customers)
        .map(|c| {
            vec![super::fixture::Move {
                from: 0,
                to: c,
                asset: 1,
                value: 1_000,
            }]
        })
        .collect();
    run(&mut b, SHAPE, 500, &[fund]);
    b
}

#[test]
fn witnesses_for_random_states_verify_and_match_cipherclerks_verifier() {
    for seed in 1..=4u8 {
        let mut b = random_book(seed);
        let ts = 10_000;
        // A transfer.
        let c = witness_for(
            &mut b,
            Vec::new(),
            transfer(u64::from(seed), 1, 2, 17),
            ts,
            None,
        );
        check(&c);
        // A reservation into a new holding position: a creation and a transfer in one step.
        let h = holding(u64::from(seed));
        let reserve = PostingDraft::balanced(
            PostingMeta::new(PostingKind::Reserve, op(3_000 + u64::from(seed), 0), 0),
            vec![
                EntryDraft::debit(SHAPE.spec(3, 1), nz(40)),
                EntryDraft::credit(h, nz(40)),
            ],
        )
        .unwrap();
        let c = witness_for(&mut b, vec![h], reserve, ts + 10, None);
        check(&c);
        // Its capture for less: three entries, closing the holding position.
        let capture = PostingDraft::balanced(
            PostingMeta::new(PostingKind::Capture, op(3_000 + u64::from(seed), 0), 1),
            vec![
                EntryDraft::debit(h, nz(40)),
                EntryDraft::credit(SHAPE.spec(4, 1), nz(25)),
                EntryDraft::credit(SHAPE.spec(3, 1), nz(15)),
            ],
        )
        .unwrap()
        .with_closing_debit();
        let c = witness_for(&mut b, Vec::new(), capture, ts + 20, None);
        check(&c);
    }
}

#[test]
fn a_tampered_leaf_is_rejected() {
    let mut b = random_book(9);
    let mut c = witness_for(&mut b, Vec::new(), transfer(1, 1, 2, 5), 10_000, None);
    let (_, acct) = c
        .witness
        .leaves_mut()
        .accounts
        .iter_mut()
        .find(|(_, a)| a.is_some())
        .unwrap();
    acct.as_mut().unwrap().timestamp += 1;
    assert!(matches!(
        c.witness.verify(c.root_before),
        Err(WitnessError::RootBefore { .. })
    ));
}

#[test]
fn a_wrong_sibling_is_rejected() {
    let mut b = random_book(10);
    let mut c = witness_for(&mut b, Vec::new(), transfer(1, 1, 2, 5), 10_000, None);
    let tree = (0..6).find(|t| c.witness.frontier_sizes()[*t] > 0).unwrap();
    c.witness.frontier_mut(tree)[0].hash[0] ^= 1;
    assert!(matches!(
        c.witness.verify(c.root_before),
        Err(WitnessError::RootBefore { .. })
    ));
    // A frontier node dropped is a wrong sibling too.
    let mut c = witness_for(&mut b, Vec::new(), transfer(2, 1, 2, 5), 10_010, None);
    c.witness.frontier_mut(1).remove(0);
    assert!(matches!(
        c.witness.verify(c.root_before),
        Err(WitnessError::RootBefore { .. })
    ));
}

/// A read outside the witnessed set fails closed: it is never answered with a default.
#[test]
fn a_read_outside_the_set_is_rejected() {
    let mut b = random_book(11);
    let payee = SHAPE.spec(2, 1).id();
    let c = witness_for(
        &mut b,
        Vec::new(),
        transfer(1, 1, 2, 5),
        10_000,
        Some(payee),
    );
    assert_eq!(c.payee, payee);
    assert_eq!(
        c.witness.verify(c.root_before),
        Err(WitnessError::Unproven(UnloadedKey::Account(payee)))
    );
}

/// `F-002` §6's size, measured again on this store: a transfer's witness at a small book.
#[test]
fn a_witness_is_a_few_kilobytes() {
    let mut b = random_book(12);
    let c = witness_for(&mut b, Vec::new(), transfer(1, 1, 2, 5), 10_000, None);
    let len = c.witness.to_succinct().unwrap().bytes.len();
    assert!(len < 16 * 1024, "{len} B");
}

/// A witness built from a store is also valid for the store's `LeafStore` trait: the recording
/// store gives the same read set as discovery.
#[test]
fn recording_a_native_run_gives_the_read_set() {
    let b = random_book(13);
    let k = keys();
    let built = build_transfer(&k, b.journal(), &transfer(1, 1, 2, 5), false);
    let rec = crate::witness::Recording::new(b.store());
    let ws = ledger_kernel::working_set::WorkingSet::load_for(
        &rec,
        &[],
        std::slice::from_ref(&built.transfer),
    )
    .unwrap();
    drop(ws);
    let view = b.store().view().unwrap();
    assert_eq!(
        rec.read_set(),
        read_set(&view, &[], std::slice::from_ref(&built.transfer)).unwrap()
    );
    assert!(rec.last_committed().unwrap().is_some());
}
