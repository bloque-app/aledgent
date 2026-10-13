// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The store commit, split (`T-031-19`).
//!
//! `Book::apply` over `FileStore` and `FileVault`, transfers between funded customers, release
//! profile. For each store (`file-mem`: redb's in-memory backend, no fsync; `file-disk`: the SSD,
//! every commit fsynced) and each batch size, `--samples` batches after `--warmup`, each batch
//! `B` single-posting units. Mode `unit` commits each unit (one commit per posting, as the core
//! does today); mode `batch` stages the `B` units and commits them once (`Book::begin_batch`,
//! `Book::flush`). Per posting: the whole apply, the store's commit, and the store's own split
//! (`FileStore::last_profile`): rules, leaves, paths, log, the transaction's commit (fsync).
//!
//!     cargo run --release --manifest-path spikes/Cargo.toml --bin commit -- \
//!         [--samples N] [--warmup W] [--customers C] [--batches 1,32] [--stores mem,disk] [--modes unit,batch]
//!
//! Prints one markdown table.

use std::time::Instant;

use ledger_kernel::book::{Book, Step, Unit};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::PostingKind;
use ledger_kernel::store::{CommitProfile, FileStore};
use ledger_kernel::vault::FileVault;
use spikes_native::shapes::{ASSET_A, Ops, ROOT, assert_applied, customer, set_up, two};
use spikes_native::stats::{summarize, us};

struct Args {
    samples: usize,
    warmup: usize,
    customers: u64,
    batches: Vec<usize>,
    stores: Vec<String>,
    modes: Vec<String>,
}

fn list(s: &str) -> Vec<String> {
    s.split(',').map(str::to_string).collect()
}

fn args() -> Args {
    let mut a = Args {
        samples: 300,
        warmup: 30,
        customers: 1_000,
        batches: vec![1, 32],
        stores: list("mem,disk"),
        modes: list("unit,batch"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut v = || it.next().expect("a value");
        match x.as_str() {
            "--samples" => a.samples = v().parse().expect("N"),
            "--warmup" => a.warmup = v().parse().expect("N"),
            "--customers" => a.customers = v().parse().expect("N"),
            "--batches" => a.batches = list(&v()).iter().map(|b| b.parse().expect("N")).collect(),
            "--stores" => a.stores = list(&v()),
            "--modes" => a.modes = list(&v()),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

fn book(store: &str, dir: &std::path::Path) -> Book<FileStore, FileVault> {
    let (s, v) = match store {
        "mem" => (
            FileStore::in_memory().expect("store"),
            FileVault::in_memory().expect("vault"),
        ),
        "disk" => (
            FileStore::open(dir.join("book.redb")).expect("store"),
            FileVault::open(dir.join("vault.redb")).expect("vault"),
        ),
        other => panic!("store {other}"),
    };
    Book::bootstrap(s, v, KeyRing::from_root(ROOT)).expect("bootstrap")
}

fn transfers(n: usize, customers: u64, ops: &mut Ops) -> Vec<Unit> {
    (0..n)
        .map(|_| {
            let op = ops.next_ref();
            let i = ops.count();
            let a = customer(i % customers, ASSET_A);
            let b = customer((i * 7 + 1) % customers, ASSET_A);
            Unit {
                steps: vec![Step {
                    creations: Vec::new(),
                    chain: vec![two(PostingKind::Transfer, op, 0, a, b, 1)],
                }],
            }
        })
        .collect()
}

#[derive(Default)]
struct Acc {
    apply: Vec<u64>,
    store: Vec<u64>,
    p: CommitProfile,
    commits: u64,
    postings: u64,
}

impl Acc {
    fn add(&mut self, p: &CommitProfile) {
        let q = &mut self.p;
        q.rules_ns += p.rules_ns;
        q.leaves_ns += p.leaves_ns;
        q.nodes_ns += p.nodes_ns;
        q.log_ns += p.log_ns;
        q.durable_ns += p.durable_ns;
        q.leaves += p.leaves;
        q.node_reads += p.node_reads;
        q.node_writes += p.node_writes;
        q.log_bytes += p.log_bytes;
        self.commits += 1;
    }
}

/// `x / n` to one decimal, in integers.
fn tenths(x: u64, n: u64) -> String {
    let t = x * 10 / n;
    format!("{}.{}", t / 10, t % 10)
}

fn total(p: &CommitProfile) -> u64 {
    p.rules_ns + p.leaves_ns + p.nodes_ns + p.log_ns + p.durable_ns
}

fn main() {
    let a = args();
    println!(
        "| Store | Mode | Batch | Postings | Apply p50 µs per posting | Apply p99 µs per posting | Store commit µs per posting (mean) | Rules | Leaves | Paths | Log | Commit/fsync | Leaves per posting | Node reads per posting | Node writes per posting | Log bytes per posting |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for store in &a.stores {
        for mode in &a.modes {
            for &batch in &a.batches {
                if mode == "batch" && batch == 1 {
                    continue;
                }
                let dir = tempfile::tempdir().expect("dir");
                let mut b = book(store, dir.path());
                set_up(&mut b, a.customers);
                let mut ops = Ops::new(0x19);
                let mut ts = 10_000u64;
                let mut acc = Acc::default();
                for i in 0..(a.warmup + a.samples) {
                    let units = transfers(batch, a.customers, &mut ops);
                    let mut store_ns = 0u64;
                    let mut profiles = Vec::new();
                    let t = Instant::now();
                    if mode == "unit" {
                        for u in &units {
                            ts += 1_000;
                            assert_applied(&b.apply(u, ts).expect("applies"));
                            profiles.push(b.store().last_profile());
                        }
                    } else {
                        b.begin_batch();
                        for u in &units {
                            ts += 1_000;
                            assert_applied(&b.apply(u, ts).expect("applies"));
                        }
                        b.end_batch().expect("flush");
                        profiles.push(b.store().last_profile());
                    }
                    let ns = t.elapsed().as_nanos() as u64;
                    for p in &profiles {
                        store_ns += total(p);
                    }
                    if i >= a.warmup {
                        acc.apply.push(ns / batch as u64);
                        acc.store.push(store_ns / batch as u64);
                        for p in &profiles {
                            acc.add(p);
                        }
                        acc.postings += batch as u64;
                    }
                }
                let s = summarize(&acc.apply);
                let n = acc.postings.max(1);
                let per = |x: u64| us(x / n);
                let store_mean = acc.store.iter().sum::<u64>() / acc.store.len().max(1) as u64;
                println!(
                    "| file-{store} | {mode} | {batch} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    acc.postings,
                    us(s.p50),
                    us(s.p99),
                    us(store_mean),
                    per(acc.p.rules_ns),
                    per(acc.p.leaves_ns),
                    per(acc.p.nodes_ns),
                    per(acc.p.log_ns),
                    per(acc.p.durable_ns),
                    tenths(acc.p.leaves, n),
                    tenths(acc.p.node_reads, n),
                    tenths(acc.p.node_writes, n),
                    acc.p.log_bytes / n,
                );
                let _ = acc.commits;
            }
        }
    }
}
