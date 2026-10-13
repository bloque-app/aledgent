// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The store's per-apply cost is flat in the book's size (`F-006` T-006-10, `NFR-1`, `NFR-3`;
//! the store's half of issue #37).
//!
//! Wall time is noisy under a parallel test run, so the assertion is on the engine's own work
//! counters: point reads, range reads and written rows per commit. They depend on the tree depth,
//! which grows as `log₂` of a tree's leaves, and on nothing else — not on how many entries an
//! account has. `src/bin/leaf-bench.rs` measures the time.

use cipher_clerk::crypto::{Amount, AuthKey, blake2b_256};
use cipher_clerk::ids::{EntryId, ExternalId};
use cipher_clerk::{Account, AccountId, Direction, Entry, Layer, Transfer, TransferId};
use ledger_kernel::store::suite;
use ledger_kernel::store::{BookDelta, LeafStore, PostingRecord};

use crate::BookStore;
use crate::engine::{Engine, Stats};

fn id16(domain: &[u8], n: u64) -> [u8; 16] {
    let h = blake2b_256(domain, &[&n.to_be_bytes()]);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h[..16]);
    out
}

fn account(n: u64) -> Account {
    Account::new(
        AccountId(id16(b"flat/account", n)),
        suite::JOURNAL,
        AuthKey([1; 32]),
        1u32,
        1u16,
        Direction::Credit,
    )
}

/// A synthetic two-entry transfer between two accounts, as the store sees one: a transfer leaf,
/// its external id, both accounts rolled forward. The store does not run the kernel, so the
/// commitments need not open.
struct Filler {
    next: u64,
    accounts: Vec<Account>,
}

impl Filler {
    fn new(n_accounts: u64) -> Self {
        Self {
            next: 0,
            accounts: (0..n_accounts).map(account).collect(),
        }
    }

    fn transfer(&mut self, d: &mut BookDelta, posting: u64, from: usize, to: usize) {
        let n = self.next;
        self.next += 1;
        let id = TransferId(id16(b"flat/transfer", n));
        let entry = |i: u8, a: &Account, dir| Entry {
            id: EntryId(id16(b"flat/entry", n * 2 + u64::from(i))),
            transfer_id: id,
            journal_id: suite::JOURNAL,
            account_id: a.id,
            direction: dir,
            layer: Layer::Settled,
            amount: Amount([i; 32]),
            ledger: 1,
            code: 1,
            timestamp: n,
        };
        let entries = vec![
            entry(0, &self.accounts[from], Direction::Debit),
            entry(1, &self.accounts[to], Direction::Credit),
        ];
        let t = Transfer::draft(id, suite::JOURNAL, entries);
        for i in [from, to] {
            self.accounts[i].timestamp = n + 1;
            d.accounts
                .insert(self.accounts[i].id, self.accounts[i].clone());
        }
        d.external_ids
            .insert(ExternalId(blake2b_256(b"flat/eid", &[&n.to_be_bytes()])));
        d.postings.push(PostingRecord {
            seq: posting,
            transfer: id,
        });
        d.transfers.insert(id, t);
    }

    /// Commit `count` postings, in commits of up to 50, all debiting account 0 (the hot one).
    fn fill(&mut self, s: &mut BookStore, count: u64) {
        let mut left = count;
        while left > 0 {
            let k = left.min(50);
            let mut d = BookDelta::default();
            let first = s.next_posting_seq().unwrap();
            for i in 0..k {
                let to = 1 + (self.next % (self.accounts.len() as u64 - 1)) as usize;
                self.transfer(&mut d, first + i, 0, to);
            }
            let seq = s.last_committed().unwrap().map_or(0, |a| a + 1);
            s.commit(seq, &d).unwrap();
            left -= k;
        }
    }

    /// The engine work of one commit of one transfer between `from` and `to`.
    fn one(&mut self, s: &mut BookStore, from: usize, to: usize) -> Stats {
        let mut d = BookDelta::default();
        let posting = s.next_posting_seq().unwrap();
        self.transfer(&mut d, posting, from, to);
        let seq = s.last_committed().unwrap().map_or(0, |a| a + 1);
        let before = s.engine().stats();
        s.commit(seq, &d).unwrap();
        s.engine().stats() - before
    }
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn cost(f: &mut Filler, s: &mut BookStore, from: usize, to: usize) -> (u64, u64) {
    let samples: Vec<Stats> = (0..21).map(|_| f.one(s, from, to)).collect();
    (
        median(samples.iter().map(|x| x.gets + x.ranges).collect()),
        median(samples.iter().map(|x| x.writes).collect()),
    )
}

/// Engine work per apply at 10 000 postings stays within a small constant factor of the work at
/// 100, and an account holding most of the book's entries costs what any other does.
#[test]
fn nfr_1_apply_cost_does_not_grow_with_the_book() {
    let mut s = BookStore::in_memory().unwrap();
    suite::bootstrap(&mut s);
    let mut f = Filler::new(64);
    f.fill(&mut s, 100);
    let (reads_small, writes_small) = cost(&mut f, &mut s, 0, 1);
    f.fill(&mut s, 10_000);
    let (reads_large, writes_large) = cost(&mut f, &mut s, 0, 1);
    // The hot account (0) now holds more than 10 000 entries; two cold accounts hold ~300 each.
    let (reads_cold, writes_cold) = cost(&mut f, &mut s, 5, 6);
    let hot = s
        .view()
        .unwrap()
        .history(&f.accounts[0].id, None, usize::MAX)
        .unwrap()
        .len();
    assert!(hot > 10_000, "{hot}");

    // log₂ grows from about 7 to about 14 over the transfers and external-id trees; nothing else
    // grows.
    assert!(
        reads_large <= 2 * reads_small,
        "reads per apply: {reads_small} at 100 postings, {reads_large} at 10 000"
    );
    assert!(
        writes_large <= 2 * writes_small,
        "writes per apply: {writes_small} at 100 postings, {writes_large} at 10 000"
    );
    // Four leaves written, each with a path of at most 2·log₂(n) + 8 branches read.
    let bound = 4 * (2 * 15 + 8) + 16;
    assert!(reads_large <= bound, "{reads_large} > {bound}");
    // A hot account costs what cold ones do: its history is never read on apply.
    assert!(
        reads_cold.abs_diff(reads_large) <= 4,
        "{reads_cold} vs {reads_large}"
    );
    assert!(
        writes_cold.abs_diff(writes_large) <= 4,
        "{writes_cold} vs {writes_large}"
    );
}
