// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The lifecycle's property run (`F-007` T-007-09): 10 000 random walks of commands, evidence and
//! ticks, each step applied twice, on the real kernel. After every step: the second application
//! changed nothing and left the composite root as it was (`INV-18`); every open record has a
//! deadline (`INV-15`); no terminal record moved (`INV-21`); every
//! recorded pair is a row of SPEC §5.6; and the book balances.
//!
//! The walk itself is the conformance suite's ([`conformance::cases::f007::walks`], registered
//! there as `inv_18_random_walks_applied_twice_change_nothing__service` with fewer walks). Here it
//! runs as 400 books of 25 walks, in four parallel parts, on `MemStore`, as `F-015` runs its
//! re-derivation (small books check faster and fail smaller). `F007_WALKS` changes the total.

use std::collections::BTreeSet;
use std::sync::Mutex;

use conformance::cases::f007::walks;
use conformance::harness::{Harness, InProcess, StoreKind};
use ledger_core::lifecycle::{State, TRANSITIONS};

const PER_BOOK: usize = 25;
const PARTS: usize = 4;

#[test]
fn inv_18_ten_thousand_walks_applied_twice_change_nothing() {
    let total: usize = std::env::var("F007_WALKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let books = total.div_ceil(PER_BOOK);
    let pairs: Mutex<BTreeSet<(State, State)>> = Mutex::new(BTreeSet::new());
    let walked = Mutex::new(0usize);
    std::thread::scope(|scope| {
        for part in 0..PARTS {
            let pairs = &pairs;
            let walked = &walked;
            scope.spawn(move || {
                for book in (part..books).step_by(PARTS) {
                    let mut h = InProcess::new(StoreKind::Mem);
                    let s = h.service().expect("the in-process target has a service");
                    let seed = 0xF007_0000_0000 + book as u64;
                    let stats = walks(s, PER_BOOK, seed).unwrap_or_else(|e| panic!("{e}"));
                    pairs.lock().unwrap().extend(stats.pairs);
                    *walked.lock().unwrap() += stats.walks;
                }
            });
        }
    });
    assert!(*walked.lock().unwrap() >= total);
    // The walks take every pair but five: the two into Rejected and the two out of `Received` and
    // `Accepted` to `Needs-Review` (a walk's operations are decided and accepted at once, before any
    // deadline passes), and the one whose posting only another feature builds (`Accepted →
    // Settled`: `Transfer`, `F-008`; `Reversal`, `F-013`). The edge witnesses take those.
    let pairs = pairs.into_inner().unwrap();
    let not_walked = [
        (State::Received, State::Rejected),
        (State::Accepted, State::Rejected),
        (State::Accepted, State::Settled),
        (State::Received, State::NeedsReview),
        (State::Accepted, State::NeedsReview),
    ];
    for e in TRANSITIONS {
        let pair = (e.from, e.to);
        assert_eq!(
            pairs.contains(&pair),
            !not_walked.contains(&pair),
            "{pair:?}; covered: {pairs:?}"
        );
    }
}
