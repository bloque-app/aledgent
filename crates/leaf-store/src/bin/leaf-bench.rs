// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Scale benchmarks for the leaf store (`F-006` T-006-10, `NFR-3`, `NFR-10`, `NFR-2`).
//!
//! ```text
//! cargo run --release -p leaf-store --bin leaf-bench -- --entries 1000000 --dir /path/with/space
//! ```
//!
//! It builds a file-backed store of `--entries` entries, `--hot` percent of them on one account
//! (the extreme skew of the current estate), through the native commit path, then reports:
//!
//! - **apply**: commit time per record and entries per second while filling;
//! - **balance read**: p50/p95/p99/max of a full balance read (account leaf, its opening checked
//!   against the leaf's commitments, the reserved total), for the hot account and for uniformly
//!   drawn accounts (`NFR-3`: 5 ms at p99);
//! - **witness**: build time and size of a transfer's read-set proofs at this size;
//! - **rebuild**: records per second applying the archive into an empty store (`NFR-10`);
//! - bytes on disk per entry.
//!
//! The book is synthetic: the store does not run the kernel, so transfers carry placeholder
//! commitments, except that every account's balance opens with the openings store's row. It
//! measures the store, not the kernel (`F-002` measured the kernel).

use std::path::PathBuf;
use std::time::Instant;

use cipher_clerk::crypto::{Amount, AuthKey, blake2b_256};
use cipher_clerk::ids::{EntryId, ExternalId};
use cipher_clerk::{
    Account, AccountId, BalancePair, Direction, Entry, Journal, JournalId, Layer, Transfer,
    TransferId,
};
use curve25519_dalek::scalar::Scalar;
use leaf_store::balance::read_balance;
use leaf_store::engine::{Durability, RedbEngine};
use leaf_store::rebuild::rebuild;
use leaf_store::witness::keys_of;
use leaf_store::{ArchiveConfig, BookStore, OpeningsStore, StoreConfig};
use ledger_kernel::store::{BookDelta, KeySet, LeafStore, PostingRecord};
use ledger_kernel::vault::{LayerSums, RowKey, Undo, VaultBackend};

struct Args {
    entries: u64,
    hot: u64,
    accounts: u64,
    samples: usize,
    dir: Option<PathBuf>,
    per_record: u64,
    rebuild: bool,
}

fn args() -> Args {
    let mut a = Args {
        entries: 1_000_000,
        hot: 70,
        accounts: 10_000,
        samples: 2_000,
        dir: None,
        per_record: 100,
        rebuild: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().expect("a value");
        match k.as_str() {
            "--entries" => a.entries = v().parse().expect("a number"),
            "--hot" => a.hot = v().parse().expect("a number"),
            "--accounts" => a.accounts = v().parse().expect("a number"),
            "--samples" => a.samples = v().parse().expect("a number"),
            "--per-record" => a.per_record = v().parse().expect("a number"),
            "--dir" => a.dir = Some(PathBuf::from(v())),
            "--no-rebuild" => a.rebuild = false,
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

const JOURNAL: JournalId = JournalId([0x10; 16]);

fn id16(domain: &[u8], n: u64) -> [u8; 16] {
    let h = blake2b_256(domain, &[&n.to_be_bytes()]);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h[..16]);
    out
}

/// Every account's settled sums: fixed, so its leaf's commitments open with them.
fn sums(n: u64) -> LayerSums {
    let mut s = LayerSums::ZERO;
    s.add(Direction::Credit, 1_000_000 + n, Scalar::from(n + 7))
        .expect("small");
    s.add(Direction::Debit, n, Scalar::from(3u64))
        .expect("small");
    s
}

fn account(n: u64) -> Account {
    let mut a = Account::new(
        AccountId(id16(b"bench/account", n)),
        JOURNAL,
        AuthKey([1; 32]),
        1u32,
        1u16,
        Direction::Credit,
    );
    let s = sums(n);
    a.balances[Layer::Settled.as_index()] = BalancePair {
        dr: Amount(s.dr.commitment()),
        cr: Amount(s.cr.commitment()),
    };
    a
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[derive(Default)]
struct Summary {
    p50: u64,
    p95: u64,
    p99: u64,
    max: u64,
}

fn summarize(mut v: Vec<u64>) -> Summary {
    v.sort_unstable();
    let at = |q: usize| {
        v[((v.len() * q).div_ceil(100))
            .saturating_sub(1)
            .min(v.len() - 1)]
    };
    Summary {
        p50: at(50),
        p95: at(95),
        p99: at(99),
        max: *v.last().expect("samples"),
    }
}

fn us(ns: u64) -> String {
    format!("{}.{}", ns / 1_000, (ns % 1_000) / 100)
}

fn dir_size(p: &std::path::Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(p).expect("dir").flatten() {
        let m = e.metadata().expect("meta");
        total += if m.is_dir() {
            dir_size(&e.path())
        } else {
            m.len()
        };
    }
    total
}

fn main() {
    let a = args();
    let tmp;
    let dir = match &a.dir {
        Some(d) => d.clone(),
        None => {
            tmp = std::env::temp_dir().join(format!("leaf-bench-{}", std::process::id()));
            tmp.clone()
        }
    };
    std::fs::create_dir_all(&dir).expect("dir");
    println!(
        "# leaf-bench: {} entries, {}% on one account, {} accounts, {} entries per record",
        a.entries, a.hot, a.accounts, a.per_record
    );
    println!("dir: {}", dir.display());

    let config = StoreConfig {
        durability: Durability::Visible,
        verify_reads: true,
    };
    let archive = ArchiveConfig {
        segment_bytes: 64 * 1024 * 1024,
        fsync: true,
    };
    let (mut store, _) = BookStore::open(dir.join("store"), config, archive).expect("open");

    // Bootstrap: the journal and every account.
    let mut accounts: Vec<Account> = (0..a.accounts).map(account).collect();
    let mut d = BookDelta::default();
    d.journals
        .insert(JOURNAL, Journal::new(JOURNAL, AuthKey([2; 32]), 0));
    store.commit(0, &d).expect("bootstrap");
    for chunk in accounts.chunks(100) {
        let mut d = BookDelta::default();
        for acc in chunk {
            d.accounts.insert(acc.id, acc.clone());
        }
        let seq = store.last_committed().unwrap().unwrap() + 1;
        store.commit(seq, &d).expect("accounts");
    }

    // Fill: two-entry transfers; a share `hot` of them move the hot account (0) against itself,
    // so `hot`% of all entries are on it.
    let mut rng = Rng(0xF006);
    let transfers = a.entries / 2;
    let mut next_t = 0u64;
    let mut commit_ns: Vec<u64> = Vec::new();
    let fill_start = Instant::now();
    while next_t < transfers {
        let k = (a.per_record / 2).max(1).min(transfers - next_t);
        let mut d = BookDelta::default();
        let first = store.next_posting_seq().unwrap();
        for i in 0..k {
            let n = next_t + i;
            let (from, to) = if rng.next() % 100 < a.hot {
                (0usize, 0usize)
            } else {
                let x = 1 + (rng.next() % (a.accounts - 1)) as usize;
                let y = 1 + (rng.next() % (a.accounts - 1)) as usize;
                (x, y)
            };
            let id = TransferId(id16(b"bench/transfer", n));
            let entry = |j: u64, acc: &Account, dir| Entry {
                id: EntryId(id16(b"bench/entry", n * 2 + j)),
                transfer_id: id,
                journal_id: JOURNAL,
                account_id: acc.id,
                direction: dir,
                layer: Layer::Settled,
                amount: Amount([j as u8 + 1; 32]),
                ledger: 1,
                code: 1,
                timestamp: n,
            };
            let entries = vec![
                entry(0, &accounts[from], Direction::Debit),
                entry(1, &accounts[to], Direction::Credit),
            ];
            for x in [from, to] {
                accounts[x].timestamp = n + 1;
                d.accounts.insert(accounts[x].id, accounts[x].clone());
            }
            d.external_ids
                .insert(ExternalId(blake2b_256(b"bench/eid", &[&n.to_be_bytes()])));
            d.postings.push(PostingRecord {
                seq: first + i,
                transfer: id,
            });
            d.transfers
                .insert(id, Transfer::draft(id, JOURNAL, entries));
        }
        let seq = store.last_committed().unwrap().unwrap() + 1;
        let t = Instant::now();
        store.commit(seq, &d).expect("fill");
        commit_ns.push(t.elapsed().as_nanos() as u64);
        next_t += k;
    }
    store.flush().expect("flush");
    let fill = fill_start.elapsed();
    let records = commit_ns.len() as u64;
    let c = summarize(commit_ns);
    println!();
    println!("## Apply (native commit path, archive fsynced per record)");
    println!();
    println!("| Records | Entries | Wall s | Entries/s | Commit p50 ms | p99 ms | max ms |");
    println!("|---:|---:|---:|---:|---:|---:|---:|");
    println!(
        "| {records} | {} | {} | {} | {} | {} | {} |",
        a.entries,
        fill.as_secs(),
        (a.entries as u128 * 1000 / fill.as_millis().max(1)),
        us(c.p50 / 1000),
        us(c.p99 / 1000),
        us(c.max / 1000)
    );

    // Openings for every account, committed at the store's sequence.
    let mut openings = OpeningsStore::open(dir.join("openings.redb")).expect("openings");
    let rows: Vec<(RowKey, LayerSums)> = accounts
        .iter()
        .enumerate()
        .map(|(n, acc)| {
            (
                RowKey::new(acc.id, Layer::Settled.as_byte()),
                sums(n as u64),
            )
        })
        .collect();
    let applied = store.applied_sequence().unwrap();
    openings
        .write(
            &rows,
            &Undo {
                seq: applied,
                prev_seq: None,
                rows: Vec::new(),
            },
        )
        .expect("openings write");

    // Balance reads.
    let hot_entries = store
        .view()
        .unwrap()
        .history(&accounts[0].id, None, usize::MAX)
        .unwrap()
        .len();
    let read = |acc: &Account| {
        let t = Instant::now();
        let b = read_balance(&store, &openings, &acc.id, &acc.id.0, 1)
            .expect("read")
            .expect("exists");
        let ns = t.elapsed().as_nanos() as u64;
        std::hint::black_box(b);
        ns
    };
    let hot: Vec<u64> = (0..a.samples).map(|_| read(&accounts[0])).collect();
    let uniform: Vec<u64> = (0..a.samples)
        .map(|_| {
            let i = (rng.next() % a.accounts) as usize;
            read(&accounts[i])
        })
        .collect();
    let (h, u) = (summarize(hot), summarize(uniform));
    println!();
    println!("## Balance read (`NFR-3`: 5 ms at p99)");
    println!();
    println!(
        "The hot account holds {hot_entries} of {} entries ({}%).",
        a.entries,
        hot_entries as u64 * 100 / a.entries.max(1)
    );
    println!();
    println!("| Account | Samples | p50 µs | p95 µs | p99 µs | max µs |");
    println!("|---|---:|---:|---:|---:|---:|");
    for (name, s) in [("hot", &h), ("uniform", &u)] {
        println!(
            "| {name} | {} | {} | {} | {} | {} |",
            a.samples,
            us(s.p50),
            us(s.p95),
            us(s.p99),
            us(s.max)
        );
    }

    // Witness build for a transfer's read set: two accounts, a new transfer id, a new external id.
    let mut build_ns = Vec::new();
    let mut nodes = Vec::new();
    for i in 0..a.samples.min(1_000) {
        let mut keys = KeySet::default();
        let x = (rng.next() % a.accounts) as usize;
        let y = (rng.next() % a.accounts) as usize;
        keys.accounts.insert(accounts[x].id);
        keys.accounts.insert(accounts[y].id);
        keys.transfers
            .insert(TransferId(id16(b"bench/new-transfer", i as u64)));
        keys.external_ids.insert(ExternalId(blake2b_256(
            b"bench/new-eid",
            &[&(i as u64).to_be_bytes()],
        )));
        let t = Instant::now();
        let view = store.view().unwrap();
        let _pre = view.load(&keys).unwrap();
        let per_tree = keys_of(&keys);
        let mut count = 0;
        for (tree, k) in per_tree.iter().enumerate() {
            count += view
                .frontier(leaf_store::trie::TreeId(tree as u8), k)
                .unwrap()
                .len();
        }
        build_ns.push(t.elapsed().as_nanos() as u64);
        nodes.push(count as u64);
    }
    let wb = summarize(build_ns);
    let wn = summarize(nodes);
    println!();
    println!("## Witness build (a transfer's read set: loads and six frontiers)");
    println!();
    println!("| p50 µs | p99 µs | Frontier nodes p50 / max |");
    println!("|---:|---:|---:|");
    println!(
        "| {} | {} | {} / {} |",
        us(wb.p50),
        us(wb.p99),
        wn.p50,
        wn.max
    );

    // Rebuild.
    let store_bytes = dir_size(&dir.join("store"));
    println!();
    println!("## Size");
    println!();
    println!(
        "Leaf store and archive: {} bytes, {} bytes per entry.",
        store_bytes,
        store_bytes / a.entries.max(1)
    );
    if a.rebuild {
        std::fs::create_dir_all(dir.join("rebuilt")).expect("dir");
        let mut target = BookStore::with_engine(
            RedbEngine::open(dir.join("rebuilt").join("leaves.redb")).expect("engine"),
            None,
            StoreConfig {
                durability: Durability::Visible,
                verify_reads: false,
            },
        )
        .expect("rebuild target");
        let t = Instant::now();
        let head = store.applied_sequence().map(|s| (s, store.log_head()));
        let n = rebuild(store.archive().unwrap(), head, &mut target).expect("rebuild");
        let secs = t.elapsed();
        let same = target.roots() == store.roots();
        println!();
        println!("## Rebuild from genesis (`NFR-10`)");
        println!();
        println!("| Records | Wall s | Records/s | Entries/s | Roots equal |");
        println!("|---:|---:|---:|---:|---|");
        println!(
            "| {n} | {} | {} | {} | {same} |",
            secs.as_secs(),
            n as u128 * 1000 / secs.as_millis().max(1),
            a.entries as u128 * 1000 / secs.as_millis().max(1)
        );
    }
    if a.dir.is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
