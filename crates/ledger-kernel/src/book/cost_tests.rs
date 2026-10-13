// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The cost of applying a posting does not grow with the book (issue #37,
//! `NFR-1`).
//!
//! `MemStore` used to ask CipherClerk's `SparseMerkleTree` for each touched
//! sub-tree's root on every commit, and that tree rebuilds its root from every
//! leaf: one posting cost about 5 ms on an empty book and over 60 ms after
//! 1 600. Both stores now update the path of each written leaf only.
//!
//! Each test grows one book to 100 postings and another to `large`, then
//! times single-posting units on the two in turn, so that whatever else the
//! machine is doing weighs on both samples alike. It asserts that the median
//! apply on the large book costs at most [`FACTOR`] times the median on the
//! small one. The always-on tests stop at 1 000 postings, where the old
//! `MemStore` was already more than ten times slower; the `#[ignore]`d ones
//! go to the 10 000 of the issue:
//!
//! ```text
//! cargo test -p ledger-kernel --lib cost_tests -- --ignored
//! ```
//!
//! `FileStore` runs on redb's in-memory backend: the same code and the same
//! tables, without the two `fsync`s per commit, whose cost does not depend on
//! the book and would only hide growth.

use super::*;
use crate::posting::{EntryDraft, OperationRef, PostingKind, PostingMeta};
use crate::store::{FileStore, MemStore};
use crate::testkit::*;
use crate::vault::{FileVault, MemVault};
use alloc::vec;
use std::time::Instant;

/// How much dearer a posting may be on the large book than on the small one.
const FACTOR: u128 = 3;

/// The size of the small book, in postings.
const SMALL: u64 = 100;

/// Timed applies per book.
const SAMPLES: usize = 51;

/// The `n`-th single-posting unit: one unit of value between customers 1
/// and 2, alternating direction so that neither runs dry.
fn posting(n: u64) -> Unit {
    let mut op = [0u8; 16];
    op[..8].copy_from_slice(&n.to_be_bytes());
    let (from, to) = if n % 2 == 0 {
        (customer(1, 1), customer(2, 1))
    } else {
        (customer(2, 1), customer(1, 1))
    };
    let draft = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Transfer, OperationRef(op), 0),
        vec![EntryDraft::debit(from, v(1)), EntryDraft::credit(to, v(1))],
    )
    .expect("test postings balance");
    unit(vec![chain(vec![draft])])
}

/// A funded book and the number of the next posting to apply to it.
struct Grown<S: LeafStore, V: VaultBackend> {
    book: Book<S, V>,
    next: u64,
}

impl<S: LeafStore, V: VaultBackend> Grown<S, V> {
    /// Fund `book` and apply postings until it holds `size`.
    fn new(mut book: Book<S, V>, size: u64) -> Self {
        fund(&mut book, 1_000_000);
        let mut g = Self { book, next: 0 };
        while g.book.store().next_posting_seq().unwrap() < size {
            g.apply();
        }
        assert_eq!(g.book.store().next_posting_seq().unwrap(), size);
        g
    }

    /// Apply the next posting; return how long `Book::apply` took, in
    /// nanoseconds.
    fn apply(&mut self) -> u128 {
        let u = posting(self.next);
        let ts = 10_000 + self.next;
        self.next += 1;
        let start = Instant::now();
        let applied = self.book.apply(&u, ts).expect("apply");
        let took = start.elapsed().as_nanos();
        assert!(applied.steps[0].applied);
        took
    }
}

fn median(mut xs: Vec<u128>) -> u128 {
    xs.sort_unstable();
    xs[xs.len() / 2]
}

/// Grow two books from `make` to [`SMALL`] and `large` postings, time
/// [`SAMPLES`] applies on each in turn, and compare the medians.
fn assert_flat<S: LeafStore, V: VaultBackend>(make: impl Fn() -> Book<S, V>, large: u64) {
    let mut small_book = Grown::new(make(), SMALL);
    let mut large_book = Grown::new(make(), large);
    // One untimed apply each, so neither sample pays a first-touch cost.
    small_book.apply();
    large_book.apply();
    let mut small = Vec::with_capacity(SAMPLES);
    let mut big = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        small.push(small_book.apply());
        big.push(large_book.apply());
    }
    let (small, big) = (median(small), median(big));
    assert!(
        big <= FACTOR * small,
        "a posting costs {big} ns at {large} postings but {small} ns at {SMALL}: \
         more than {FACTOR}x, so apply cost grows with the book"
    );
}

fn mem_store_book() -> Book<MemStore, MemVault> {
    mem_book()
}

fn file_store_book() -> Book<FileStore, FileVault> {
    Book::bootstrap(
        FileStore::in_memory().expect("store"),
        FileVault::in_memory().expect("vault"),
        crate::keys::KeyRing::from_root(ROOT),
    )
    .expect("bootstrap")
}

#[test]
fn nfr_1_apply_cost_is_flat_to_1_000_postings_on_mem_store() {
    assert_flat(mem_store_book, 1_000);
}

#[test]
fn nfr_1_apply_cost_is_flat_to_1_000_postings_on_file_store() {
    assert_flat(file_store_book, 1_000);
}

#[test]
#[ignore = "slow: grows a book to 10 000 postings; run with --ignored"]
fn nfr_1_apply_cost_is_flat_to_10_000_postings_on_mem_store() {
    assert_flat(mem_store_book, 10_000);
}

#[test]
#[ignore = "slow: grows a book to 10 000 postings; run with --ignored"]
fn nfr_1_apply_cost_is_flat_to_10_000_postings_on_file_store() {
    assert_flat(file_store_book, 10_000);
}
