// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement legs' property run (`F-010` T-010-10, `NFR-14`): 10 000 seeded histories of
//! deposits, withdrawals, forks, delays, drops, duplicates, corroborating copies, pruned ranges,
//! amount mismatches and deadlines against the simulated settlement layer, on the real kernel.
//! After every step `INV-10`–`INV-13` and `INV-23` hold, no Customer balance is negative and the
//! book balances; once each history has settled, every external leg that ended is checked
//! against the layer's own finalised record.
//!
//! The run itself is the conformance suite's ([`conformance::cases::f010::runs`], registered there
//! as `inv_11_seeded_simulator_runs_derive_every_outcome_from_evidence__service` with fewer
//! runs). Here it runs as 1 000 books of 10 histories, in parallel parts (one per available core, at least four), on `MemStore`.
//! `F010_RUNS` changes the total; a failure names its seed and history, and replays from them
//! (`F010_SEED` replaces the base of the seeds, and a failure prints it with the command that
//! repeats the run).

#![cfg(feature = "sim")]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use conformance::cases::f010::runs;
use conformance::harness::{Harness, InProcess, StoreKind};
use conformance::tools::seed::{Replay, Rotation};
use ledger_core::lifecycle::State;

const PER_BOOK: u64 = 10;

/// The seed of the first book: book `n` runs under `FIXED_SEED + n`, unless `F010_SEED` (a
/// number; the nightly run rotates it) replaces the base.
const FIXED_SEED: u64 = 0xF010_0000_0000;

#[test]
fn inv_10_to_13_ten_thousand_seeded_histories_against_the_simulated_layer() {
    let total: u64 = std::env::var("F010_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let books = total.div_ceil(PER_BOOK);
    let parts = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(4)
        .max(4);
    let rotation = Rotation::from_env("F010_SEED");
    let base = rotation.base(FIXED_SEED);
    let pairs: Mutex<BTreeSet<(State, State)>> = Mutex::new(BTreeSet::new());
    let terminal: Mutex<BTreeMap<State, usize>> = Mutex::new(BTreeMap::new());
    let done = Mutex::new(0usize);
    std::thread::scope(|scope| {
        for part in 0..parts {
            let (pairs, terminal, done) = (&pairs, &terminal, &done);
            let rotation = &rotation;
            scope.spawn(move || {
                for book in (part..books).step_by(parts as usize) {
                    let mut h = InProcess::new(StoreKind::Mem);
                    let s = h.service().expect("the in-process target has a service");
                    let seed = base + book;
                    let stats = runs(s, 0, PER_BOOK, seed).unwrap_or_else(|e| {
                        rotation.fail(
                            format!("{e} (the book of seed {seed:#x})"),
                            Replay {
                                target: "-p ledger-core --features sim --test settlement_sim",
                                size_var: "F010_RUNS",
                                size: total,
                            },
                        )
                    });
                    pairs.lock().unwrap().extend(stats.pairs);
                    let mut t = terminal.lock().unwrap();
                    for (k, v) in stats.terminal {
                        *t.entry(k).or_default() += v;
                    }
                    *done.lock().unwrap() += stats.runs;
                }
            });
        }
    });
    assert!(*done.lock().unwrap() as u64 >= total);
    // The histories reach every disposition: executed, provably not executed, orphaned, unknown.
    let pairs = pairs.into_inner().unwrap();
    for pair in [
        (State::Reserved, State::Submitted),
        (State::Submitted, State::Observed),
        (State::Observed, State::Final),
        (State::Final, State::Settled),
        (State::Observed, State::Orphaned),
        (State::Orphaned, State::Released),
        (State::Orphaned, State::Submitted),
        (State::Submitted, State::Expired),
        (State::Expired, State::Released),
        (State::Expired, State::NeedsReview),
        (State::Submitted, State::NeedsReview),
    ] {
        if total >= 1_000 {
            assert!(
                pairs.contains(&pair),
                "{pair:?} never taken; took {pairs:?}"
            );
        }
    }
    let terminal = terminal.into_inner().unwrap();
    if total >= 1_000 {
        assert!(terminal.get(&State::Settled).copied().unwrap_or(0) > 0);
        assert!(terminal.get(&State::Released).copied().unwrap_or(0) > 0);
    }
}
