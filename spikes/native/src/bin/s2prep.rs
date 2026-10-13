// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! S2 inputs (T-002-03): for each posting shape, the unit the guest runs,
//! with the touched pre-state as a `VecLedger`, the openings the kernel will
//! ask for, and the root the host gets for the same unit. Also times the
//! kernel alone, natively, on exactly that input — the native counterpart of
//! the guest figure.
//!
//!     cargo run --release --manifest-path spikes/Cargo.toml --bin s2prep [-- --samples N]
//!
//! Writes `spikes/results/guest/s2-<shape>-b<batch>.bin` and
//! `spikes/results/s2-native.csv`.

use std::collections::{BTreeMap, BTreeSet};

use cipher_clerk::crypto::{Amount, Blinding};
use cipher_clerk::kernel::CreateAccount;
use cipher_clerk::state::{LedgerState, Opening};
use cipher_clerk::{
    AccountId, Direction, EventStatus, Layer, OpeningsOracle, Transfer, VecLedger,
    apply_account_creations, apply_batch,
};
use curve25519_dalek::scalar::Scalar;
use ledger_kernel::account::BalanceRule;
use ledger_kernel::book::{Book, Unit};
use ledger_kernel::build::{build_creation, build_transfer};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::{FileStore, KeySet, LeafStore};
use ledger_kernel::vault::{FileVault, LayerSums, RowKey};
use ledger_kernel::working_set::discover;
use spikes_native::shapes::{
    CUSTOMERS, Ops, ROOT, Shape, assert_applied, file_mem_book, sample_units, set_up,
};
use spikes_native::stats::{summarize, time_n, us};
use spikes_native::{Sink, results_dir};

/// One step of a unit, with the seeds `Book::apply` gives it. Field order is
/// the wire contract with `spikes/guest/kernel-probe`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone)]
pub struct S2Step {
    creations: Vec<CreateAccount>,
    creation_seed: u64,
    events: Vec<Transfer>,
    seed: u64,
}

/// A unit and the touched pre-state it runs over (wire contract with the guest).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone)]
pub struct S2Case {
    ledger: VecLedger,
    steps: Vec<S2Step>,
    oracle: OpeningsOracle,
    expected_root: [u8; 32],
}

type FileBook = Book<FileStore, FileVault>;

fn opening_of(sums: &LayerSums) -> Opening {
    let (net, bl) = sums.net(Direction::Credit).expect("sums");
    let value = u64::try_from(net).expect("never negative");
    let blinding = Blinding::from_bytes(bl.to_bytes()).expect("canonical");
    Opening {
        amount: Amount::commit(value, &blinding),
        value,
        blinding,
    }
}

/// Build the case for `unit` against `book`, then apply it to `book`.
fn case_for(book: &mut FileBook, unit: &Unit, ts: u64) -> S2Case {
    let keys = KeyRing::from_root(ROOT);
    let journal = book.journal();

    // Build as Book::apply does, with its seeds.
    let total: u64 = unit
        .steps
        .iter()
        .map(|s| (s.creations.len() + s.chain.len()) as u64)
        .sum();
    let base = ts - total;
    let mut position = 0u64;
    let mut steps = Vec::new();
    let mut entry_openings = Vec::new();
    for s in &unit.steps {
        let creations: Vec<CreateAccount> = s
            .creations
            .iter()
            .map(|c| build_creation(&keys, journal, c))
            .collect();
        let c = creations.len() as u64;
        let creation_seed = base + position + c;
        position += c;
        let built: Vec<_> = s
            .chain
            .iter()
            .enumerate()
            .map(|(i, d)| build_transfer(&keys, journal, d, i + 1 < s.chain.len()))
            .collect();
        let k = built.len() as u64;
        let seed = base + position + k;
        position += k;
        for b in &built {
            entry_openings.extend(b.openings.iter().cloned());
        }
        steps.push(S2Step {
            creations,
            creation_seed,
            events: built.into_iter().map(|b| b.transfer).collect(),
            seed,
        });
    }
    let all_creations: Vec<CreateAccount> =
        steps.iter().flat_map(|s| s.creations.clone()).collect();
    let all_events: Vec<Transfer> = steps.iter().flat_map(|s| s.events.clone()).collect();

    // The touched pre-state, as a VecLedger (sorted by id).
    let mut touched: KeySet = discover(&all_creations, &all_events);
    touched.journals.insert(journal);
    let pre = book.store().load(&touched).expect("load");
    let mut ledger = VecLedger::new();
    ledger.set_journal(pre.journals[&journal].clone().expect("journal"));
    ledger.accounts = pre.accounts.values().flatten().cloned().collect();
    ledger.accounts.sort_by_key(|a| a.id.0);

    // Net openings after every event, from the vault's pre-state sums.
    let created: BTreeSet<AccountId> = all_creations.iter().map(|c| c.account.id).collect();
    let flagged: BTreeSet<AccountId> = pre
        .accounts
        .values()
        .flatten()
        .filter(|a| BalanceRule::of(a) == BalanceRule::NeverNegative)
        .map(|a| a.id)
        .chain(
            all_creations
                .iter()
                .filter(|c| BalanceRule::of(&c.account) == BalanceRule::NeverNegative)
                .map(|c| c.account.id),
        )
        .collect();
    let mut sums: BTreeMap<AccountId, LayerSums> = BTreeMap::new();
    for id in pre.accounts.keys().chain(created.iter()) {
        let s = if created.contains(id) {
            LayerSums::ZERO
        } else {
            book.vault()
                .sums(&RowKey::new(*id, Layer::Settled.as_byte()))
        };
        sums.insert(*id, s);
    }
    let mut net_openings = Vec::new();
    let mut entry_iter = entry_openings.iter();
    for t in &all_events {
        let mut hit = BTreeSet::new();
        for e in &t.entries {
            let o = entry_iter.next().expect("one opening per entry");
            let b = Scalar::from_canonical_bytes(o.blinding.0).expect("canonical");
            sums.get_mut(&e.account_id)
                .expect("touched")
                .add(e.direction, o.value, b)
                .expect("sums");
            hit.insert(e.account_id);
        }
        for id in hit.intersection(&flagged) {
            net_openings.push(opening_of(&sums[id]));
        }
    }
    let mut openings = entry_openings;
    openings.extend(net_openings);
    let oracle = OpeningsOracle::new(openings);

    // The host's own run of the same input: statuses and root.
    let mut probe = ledger.clone();
    let mut o = oracle.clone();
    for s in &steps {
        if !s.creations.is_empty() {
            let r = apply_account_creations(&mut probe, &s.creations, &mut o, s.creation_seed);
            assert!(r.iter().all(|r| r.status == EventStatus::Created), "{r:?}");
        }
        let r = apply_batch(&mut probe, &s.events, &mut o, s.seed);
        assert!(r.iter().all(|r| r.status == EventStatus::Created), "{r:?}");
    }
    let expected_root = probe.root();

    // And the book itself: the same unit applies.
    let a = book.apply(unit, ts).expect("apply");
    assert_applied(&a);
    S2Case {
        ledger,
        steps,
        oracle,
        expected_root,
    }
}

fn main() {
    let mut samples = 1_000usize;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--samples" => samples = it.next().and_then(|s| s.parse().ok()).expect("n"),
            other => panic!("unknown argument {other}"),
        }
    }
    let dir = results_dir().join("guest");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut sink = Sink::create("s2-native").expect("csv");
    let mut book = file_mem_book();
    set_up(&mut book, CUSTOMERS);
    let mut ops = Ops::new(0x61);
    let mut ts = 100_000u64;
    let mut cases: Vec<(Shape, usize)> = Shape::ALL.iter().map(|s| (*s, 1)).collect();
    cases.extend([
        (Shape::Transfer, 4),
        (Shape::Transfer, 10),
        (Shape::ReservePooled, 4),
    ]);
    println!(
        "| Shape | Batch | Case bytes | Native kernel p50 µs | p95 µs | p99 µs | max µs | Native root p50 µs |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for (shape, batch) in cases {
        let (prep, unit) = sample_units(shape, batch, &mut ops);
        if let Some(p) = prep {
            ts += 1_000;
            assert_applied(&book.apply(&p, ts).expect("prep"));
        }
        ts += 1_000;
        let case = case_for(&mut book, &unit, ts);
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&case).expect("serialise");
        let name = format!("s2-{}-b{batch}", shape.name());
        std::fs::write(dir.join(format!("{name}.bin")), &bytes).expect("write");

        // The kernel alone, natively, on the same input.
        let kernel = time_n(20, samples, || {
            let mut l = case.ledger.clone();
            let mut o = case.oracle.clone();
            for s in &case.steps {
                if !s.creations.is_empty() {
                    apply_account_creations(&mut l, &s.creations, &mut o, s.creation_seed);
                }
                std::hint::black_box(apply_batch(&mut l, &s.events, &mut o, s.seed));
            }
        });
        let mut after = case.ledger.clone();
        {
            let mut o = case.oracle.clone();
            for s in &case.steps {
                if !s.creations.is_empty() {
                    apply_account_creations(&mut after, &s.creations, &mut o, s.creation_seed);
                }
                apply_batch(&mut after, &s.events, &mut o, s.seed);
            }
        }
        let root = time_n(20, samples, || {
            std::hint::black_box(after.root());
        });
        sink.series(
            "s2",
            &shape.name(),
            &format!("native-kernel/batch-{batch}"),
            bytes.len() as u64,
            "total",
            "ns",
            &kernel,
        )
        .expect("csv");
        sink.series(
            "s2",
            &shape.name(),
            &format!("native-root/batch-{batch}"),
            bytes.len() as u64,
            "total",
            "ns",
            &root,
        )
        .expect("csv");
        let k = summarize(&kernel);
        let r = summarize(&root);
        println!(
            "| {} | {batch} | {} | {} | {} | {} | {} | {} |",
            shape.name(),
            bytes.len(),
            us(k.p50),
            us(k.p95),
            us(k.p99),
            us(k.max),
            us(r.p50)
        );
    }
    sink.finish().expect("csv");
}
