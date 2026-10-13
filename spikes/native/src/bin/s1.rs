// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! S1 — the kernel, native (T-002-02).
//!
//! For each posting shape, batch size and store variant, times
//! `Book::apply` over at least 1 000 samples after warm-up, and the signing
//! of the same unit. Also times the primitives the kernel runs per posting,
//! so each posting's time can be split into signature verification,
//! commitment checks, negativity openings and root update.
//!
//!     cargo run --release --manifest-path spikes/Cargo.toml --bin s1 [-- --samples N --warmup W --quick]
//!
//! Writes `spikes/results/s1.csv` and prints the summary tables.

use std::collections::BTreeMap;
use std::time::Instant;

use cipher_clerk::crypto::Amount;
use cipher_clerk::kernel::verify::verify_reveal;
use cipher_clerk::merkle::smt_leaf_hash;
use ledger_kernel::book::{Book, Unit};
use ledger_kernel::build::{build_creation, build_transfer};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::smt::{self, NodeAddr, NodeTable};
use ledger_kernel::store::{FileStore, LeafStore};
use ledger_kernel::vault::{FileVault, VaultBackend};
use spikes_native::golden::assert_golden;
use spikes_native::shapes::{
    CUSTOMERS, Ops, ROOT, Shape, assert_applied, file_disk_book, file_mem_book, sample_units,
    set_up,
};
use spikes_native::stats::{Summary, summarize, time_n, us};
use spikes_native::{Sink, shapes};

const BATCHES: [usize; 4] = [1, 4, 10, 64];

struct Args {
    samples: usize,
    warmup: usize,
    quick: bool,
}

fn args() -> Args {
    let mut a = Args {
        samples: 1_000,
        warmup: 50,
        quick: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--samples" => a.samples = it.next().and_then(|s| s.parse().ok()).expect("--samples N"),
            "--warmup" => a.warmup = it.next().and_then(|s| s.parse().ok()).expect("--warmup N"),
            "--quick" => a.quick = true,
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// Build and sign every creation and posting of `unit`, as `Book::apply`
/// does first.
fn sign_unit(keys: &KeyRing, book_journal: cipher_clerk::JournalId, unit: &Unit) -> usize {
    let mut n = 0;
    for step in &unit.steps {
        for c in &step.creations {
            std::hint::black_box(build_creation(keys, book_journal, c));
            n += 1;
        }
        let len = step.chain.len();
        for (i, d) in step.chain.iter().enumerate() {
            std::hint::black_box(build_transfer(keys, book_journal, d, i + 1 < len));
            n += 1;
        }
    }
    n
}

/// Bring `shadow` to `book`'s last commit by replaying the delta log.
fn catch_up<S: LeafStore, V: VaultBackend, T: LeafStore>(book: &Book<S, V>, shadow: &mut T) {
    let last = book.last_committed().expect("last").expect("bootstrapped");
    let from = shadow.last_committed().expect("last").map_or(0, |n| n + 1);
    for seq in from..=last {
        let rec = book.store().log_record(seq).expect("log").expect("record");
        shadow.commit(seq, &rec.delta).expect("replay");
    }
    assert_eq!(shadow.root().expect("root"), book.root().expect("root"));
}

/// Time one (variant, shape, batch) series on `book`. Each sample's delta is
/// also committed, timed, to `shadow` (a second store of the same kind, kept
/// at the same commit): that is the store's own cost, root update included.
/// At batch 64 the shadow is skipped (`None`) to halve memory and disk; the
/// store cost is then not recorded for that batch.
fn run_series<S: LeafStore, V: VaultBackend, T: LeafStore>(
    book: &mut Book<S, V>,
    mut shadow: Option<&mut T>,
    shape: Shape,
    batch: usize,
    warmup: usize,
    samples: usize,
) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
    if let Some(sh) = shadow.as_deref_mut() {
        catch_up(book, sh);
    }
    let mut ops = Ops::new(0x51);
    let keys = KeyRing::from_root(ROOT);
    let journal = book.journal();
    let mut ts = 100_000u64;
    let mut totals = Vec::with_capacity(samples);
    let mut signs = Vec::with_capacity(samples);
    let mut stores = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let (prep, unit) = sample_units(shape, batch, &mut ops);
        if let Some(p) = prep {
            ts += 1_000;
            assert_applied(&book.apply(&p, ts).expect("prep applies"));
            if let Some(sh) = shadow.as_deref_mut() {
                catch_up(book, sh);
            }
        }
        let t = Instant::now();
        sign_unit(&keys, journal, &unit);
        let sign = t.elapsed().as_nanos() as u64;
        ts += 1_000;
        let t = Instant::now();
        let applied = book.apply(&unit, ts).expect("unit applies");
        let total = t.elapsed().as_nanos() as u64;
        assert_applied(&applied);
        let seq = applied.seq.expect("a sample must commit");
        let store = shadow.as_deref_mut().map(|sh| {
            let rec = book.store().log_record(seq).expect("log").expect("record");
            let t = Instant::now();
            let sub = sh.commit(seq, &rec.delta).expect("shadow commit");
            let ns = t.elapsed().as_nanos() as u64;
            assert_eq!(ledger_kernel::store::composite_root(&sub), applied.root);
            ns
        });
        if i >= warmup {
            totals.push(total);
            signs.push(sign);
            stores.extend(store);
        }
    }
    (totals, signs, stores)
}

#[derive(Default)]
struct Nodes(BTreeMap<NodeAddr, [u8; 32]>);

impl NodeTable for Nodes {
    type Error = core::convert::Infallible;
    fn get(&self, addr: &NodeAddr) -> Result<Option<[u8; 32]>, Self::Error> {
        Ok(self.0.get(addr).copied())
    }
    fn put(&mut self, addr: &NodeAddr, hash: [u8; 32]) -> Result<(), Self::Error> {
        self.0.insert(*addr, hash);
        Ok(())
    }
}

/// The per-operation costs the split is built from.
struct Primitives {
    verify_signature: Summary,
    sign: Summary,
    opening_check: Summary,
    decompress: Summary,
    compress: Summary,
    point_add: Summary,
    smt_set_leaf: Summary,
}

fn primitives(sink: &mut Sink, args: &Args) -> Primitives {
    let keys = ledger_kernel::keys::RootKeys::new(ROOT);
    let spec = shapes::customer(0, shapes::ASSET_A);
    let secret = keys.account_secret(&spec.id());
    let public = keys.account_key(&spec.id());
    let msg = [0x42u8; 200];
    let sig = keys.sign(&secret, &msg);
    assert!(cipher_clerk::crypto::verify_signature(&public, &msg, &sig));

    // A real entry opening, from F-001's builder.
    let draft = shapes::two(
        ledger_kernel::posting::PostingKind::Transfer,
        ledger_kernel::posting::OperationRef([9; 16]),
        0,
        spec,
        shapes::customer(1, shapes::ASSET_A),
        12_345,
    );
    let ring = KeyRing::from_root(ROOT);
    let built = build_transfer(
        &ring,
        ledger_kernel::book::journal_id_for(&ring.registrar_key()),
        &draft,
        false,
    );
    let opening = built.openings[0].clone();
    assert_eq!(
        verify_reveal(&opening.amount, &opening.reveal()),
        Ok(12_345)
    );
    let point = opening.amount.to_point().expect("valid point");
    let other = built.openings[1].amount.to_point().expect("valid point");

    let n = args.samples.max(1_000);
    let w = args.warmup;
    let mut record = |name: &str, samples: Vec<u64>| {
        sink.series(
            "s1",
            &format!("primitive:{name}"),
            "native",
            0,
            "total",
            "ns",
            &samples,
        )
        .expect("csv");
        summarize(&samples)
    };
    let verify_signature = record(
        "verify_signature",
        time_n(w, n, || {
            std::hint::black_box(cipher_clerk::crypto::verify_signature(
                &public,
                std::hint::black_box(&msg),
                &sig,
            ));
        }),
    );
    let sign = record(
        "sign",
        time_n(w, n, || {
            std::hint::black_box(keys.sign(&secret, std::hint::black_box(&msg)));
        }),
    );
    let opening_check = record(
        "opening_check",
        time_n(w, n, || {
            std::hint::black_box(verify_reveal(
                std::hint::black_box(&opening.amount),
                &opening.reveal(),
            ))
            .expect("opens");
        }),
    );
    let decompress = record(
        "decompress",
        time_n(w, n, || {
            std::hint::black_box(std::hint::black_box(&opening.amount).to_point());
        }),
    );
    let compress = record(
        "compress",
        time_n(w, n, || {
            std::hint::black_box(Amount::from_point(std::hint::black_box(&point)));
        }),
    );
    let point_add = record(
        "point_add",
        time_n(w, n, || {
            std::hint::black_box(std::hint::black_box(point) + std::hint::black_box(other));
        }),
    );
    // One leaf write in the incremental SMT: 128 node hashes and table writes.
    let empty = smt::empty_chain();
    let mut nodes = Nodes::default();
    let mut k = 0u64;
    let smt_set_leaf = record(
        "smt_set_leaf",
        time_n(w, n, || {
            k += 1;
            let key = shapes::subject(0x77, k.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            std::hint::black_box(
                smt::set_leaf(&mut nodes, &empty, &key, smt_leaf_hash(&k.to_le_bytes()))
                    .expect("infallible"),
            );
        }),
    );
    Primitives {
        verify_signature,
        sign,
        opening_check,
        decompress,
        compress,
        point_add,
        smt_set_leaf,
    }
}

/// The modelled kernel split of one posting, in nanoseconds at p50:
/// (signature verification, commitment checks, negativity openings, root update).
fn split(shape: Shape, p: &Primitives) -> (u64, u64, u64, u64) {
    let c = shape.counts();
    let sig = (c.signatures + c.creations) * p.verify_signature.p50;
    // Per entry: opening check, zero-sum decompression + addition, balance
    // update (two decompressions, an addition, a compression).
    let per_entry =
        p.opening_check.p50 + 3 * p.decompress.p50 + 2 * p.point_add.p50 + p.compress.p50;
    let commitments = c.entries * per_entry;
    // Per never-negative touched layer: net (two decompressions, a
    // subtraction, a compression) and its opening check.
    let per_neg = 2 * p.decompress.p50 + p.point_add.p50 + p.compress.p50 + p.opening_check.p50;
    let negativity = c.negativity * per_neg;
    let root = c.leaves * p.smt_set_leaf.p50;
    (sig, commitments, negativity, root)
}

fn main() {
    let args = args();
    let started = Instant::now();

    // The binding under measurement is F-001's: its golden roots, on both
    // store variants measured.
    assert_golden(file_mem_book());
    let dir = tempfile::tempdir().expect("tempdir");
    assert_golden(file_disk_book(dir.path()));
    drop(dir);
    println!("golden roots: OK on FileStore (in-memory backend) and FileStore (disk)");

    let mut sink = Sink::create(if args.quick { "s1-quick" } else { "s1" }).expect("csv");
    let prims = primitives(&mut sink, &args);
    println!(
        "\n### Primitives (native, release, n = {} each)\n",
        prims.verify_signature.n
    );
    println!("| Primitive | p50 µs | p95 µs | p99 µs | max µs |");
    println!("|---|---:|---:|---:|---:|");
    for (name, s) in [
        ("verify_signature", &prims.verify_signature),
        ("sign (core, DEC-3)", &prims.sign),
        ("opening check (verify_reveal)", &prims.opening_check),
        ("point decompress", &prims.decompress),
        ("point compress", &prims.compress),
        ("point add", &prims.point_add),
        ("SMT leaf write (128 nodes, in memory)", &prims.smt_set_leaf),
    ] {
        println!(
            "| {name} | {} | {} | {} | {} |",
            us(s.p50),
            us(s.p95),
            us(s.p99),
            us(s.max)
        );
    }

    let shapes_run: Vec<Shape> = if args.quick {
        vec![Shape::Transfer, Shape::CaptureLess]
    } else {
        Shape::ALL.to_vec()
    };
    let batches: &[usize] = if args.quick { &[1, 4] } else { &BATCHES };

    let mut table = Vec::new();
    for variant in ["file-mem", "file-disk"] {
        for &shape in &shapes_run {
            let bs: Vec<usize> = if shape.batched() {
                batches.to_vec()
            } else {
                vec![1]
            };
            for b in bs {
                // A fresh book every SEGMENT samples bounds memory and disk
                // (FileStore writes about 18 KB per leaf; a batch-64 series
                // would otherwise grow to several GB).
                const SEGMENT: usize = 250;
                let (mut totals, mut signs, mut stores) = (Vec::new(), Vec::new(), Vec::new());
                while totals.len() < args.samples {
                    let n = SEGMENT.min(args.samples - totals.len());
                    let w = if totals.is_empty() { args.warmup } else { 10 };
                    let dir = tempfile::tempdir().expect("tempdir");
                    let (t, s, c) = if variant == "file-mem" {
                        let mut book = file_mem_book();
                        set_up(&mut book, CUSTOMERS);
                        let mut shadow = FileStore::in_memory().expect("shadow");
                        run_series(&mut book, (b < 64).then_some(&mut shadow), shape, b, w, n)
                    } else {
                        let mut book: Book<FileStore, FileVault> = file_disk_book(dir.path());
                        set_up(&mut book, CUSTOMERS);
                        let mut shadow =
                            FileStore::open(dir.path().join("shadow.redb")).expect("shadow");
                        run_series(&mut book, (b < 64).then_some(&mut shadow), shape, b, w, n)
                    };
                    drop(dir);
                    totals.extend(t);
                    signs.extend(s);
                    stores.extend(c);
                }
                let case = shape.name();
                let var = format!("{variant}/batch-{b}");
                sink.series("s1", &case, &var, CUSTOMERS, "total", "ns", &totals)
                    .expect("csv");
                sink.series("s1", &case, &var, CUSTOMERS, "sign", "ns", &signs)
                    .expect("csv");
                sink.series("s1", &case, &var, CUSTOMERS, "store_commit", "ns", &stores)
                    .expect("csv");
                let st = summarize(&totals);
                let ss = summarize(&signs);
                let sc = (!stores.is_empty()).then(|| summarize(&stores));
                eprintln!(
                    "[{:>5}s] {variant} {case} batch {b}: p50 {} µs, p99 {} µs",
                    started.elapsed().as_secs(),
                    us(st.p50),
                    us(st.p99)
                );
                table.push((variant, shape, b, st, ss, sc));
            }
        }
    }

    println!(
        "\n### Book::apply per unit (release, n = {} per row, after {} warm-up)\n",
        args.samples, args.warmup
    );
    println!(
        "| Store | Shape | Batch | Postings | p50 µs | p95 µs | p99 µs | max µs | p50 µs / posting | sign p50 µs | store commit p50 µs | store commit p99 µs |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (variant, shape, b, st, ss, sc) in &table {
        let postings = shape.postings(*b) as u64;
        println!(
            "| {variant} | {} | {b} | {postings} | {} | {} | {} | {} | {} | {} | {} | {} |",
            shape.name(),
            us(st.p50),
            us(st.p95),
            us(st.p99),
            us(st.max),
            us(st.p50 / postings),
            us(ss.p50),
            sc.map_or("n/m".into(), |s| us(s.p50)),
            sc.map_or("n/m".into(), |s| us(s.p99))
        );
    }

    println!("\n### Split of one posting at batch 1, file-mem (p50, µs)\n");
    println!(
        "| Shape | Measured total | Sign (measured) | Signature verification (model) | Commitment checks (model) | Negativity openings (model) | Store commit incl. root update (measured) | of which SMT hashing (model) | Binding residual |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (variant, shape, b, st, ss, sc) in &table {
        if *variant != "file-mem" || *b != 1 {
            continue;
        }
        let postings = shape.postings(*b) as u64;
        let (sig, com, neg, root) = split(*shape, &prims);
        let total = st.p50 / postings;
        let sign = ss.p50 / postings;
        let store = sc.map_or(0, |s| s.p50) / postings;
        let residual = total.saturating_sub(sign + sig + com + neg + store);
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            shape.name(),
            us(total),
            us(sign),
            us(sig),
            us(com),
            us(neg),
            us(store),
            us(root),
            us(residual)
        );
    }
    sink.finish().expect("csv");
    eprintln!("done in {} s", started.elapsed().as_secs());
}
