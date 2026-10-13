// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-012` follow-up (issue #50): opening many owned-custody accounts costs the same per account
//! at the end as at the start.
//!
//! ```text
//! f012-scale [--accounts N] [--batches B]
//! ```
//!
//! Opens `N` people, each with one owned-custody Customer account, through the command surface
//! of a book in memory (`MemStore`, `MemVault`: nothing on disk), in the conformance deployment,
//! which offers holder-held recovery credentials. Opening an owned account reads the owner's
//! recovery mechanisms twice (the acknowledgement, then the command's own check), and each read
//! lists the owner's credential set. The run prints the median and mean time per open for each of
//! `B` batches and fails if the last batch's median is more than three times the first's: with a
//! scan of every credential of the book, it grows with the book instead.

use std::time::Instant;

use conformance::tools::book::RandomBook;
use ledger_core::account::Custody;
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;

struct Args {
    accounts: usize,
    batches: usize,
}

fn args() -> Args {
    let mut a = Args {
        accounts: 50_000,
        batches: 10,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().expect("a value");
        match k.as_str() {
            "--accounts" => a.accounts = v().parse().expect("a count"),
            "--batches" => a.batches = v().parse().expect("a count"),
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(
        a.batches > 0 && a.accounts >= a.batches,
        "at least one open per batch"
    );
    a
}

fn main() {
    let a = args();
    println!(
        "# f012-scale: {} owned-custody accounts in {} batches",
        a.accounts, a.batches
    );
    let t = Instant::now();
    let mut b = RandomBook::new(MemStore::new(), MemVault::new(), 0xF012);
    println!("genesis: {} ms", t.elapsed().as_millis());
    let per = a.accounts / a.batches;
    // The median time of one open, per batch, in nanoseconds: integers only (`INV-7`'s gate).
    // The median, not the mean, so that a host busy with other work does not decide the check.
    let mut medians: Vec<u128> = Vec::with_capacity(a.batches);
    let mut each: Vec<u128> = Vec::with_capacity(per);
    let all = Instant::now();
    for batch in 0..a.batches {
        each.clear();
        let t = Instant::now();
        for _ in 0..per {
            let one = Instant::now();
            let h = b.person_with(Custody::Owned);
            each.push(one.elapsed().as_nanos());
            assert!(!h.delegated);
        }
        let mean = t.elapsed().as_nanos() / per as u128;
        each.sort_unstable();
        let median = each[per / 2];
        println!(
            "batch {:>3}: accounts {:>7}..{:>7}, median {:>6} µs, mean {:>6} µs per open",
            batch + 1,
            batch * per,
            (batch + 1) * per,
            median / 1_000,
            mean / 1_000
        );
        medians.push(median);
    }
    let first = medians[0];
    let last = medians[medians.len() - 1];
    println!(
        "total: {} owned accounts in {} ms; credentials in the book: {}; last/first median per open: {}.{:02}",
        per * a.batches,
        all.elapsed().as_millis(),
        b.ledger.state().credentials().count(),
        last / first.max(1),
        (last * 100 / first.max(1)) % 100
    );
    assert!(
        last <= first * 3,
        "the time per open grew with the book: median {first} ns first, {last} ns last"
    );
    println!("flat");
}
