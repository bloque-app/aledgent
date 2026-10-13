// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The model check (`F-007` test plan (a)–(d), T-007-03): hand-written, no external crate. The
//! space is small enough to enumerate: 12 states × 21 triggers × 20 480 fact combinations.
//!
//! - **Soundness**: every `Move` [`next`] returns is a pair of [`TRANSITIONS`], with a posting
//!   the row's rule allows.
//! - **Completeness**: every row is produced by at least one case.
//! - **Reachability**: a breadth-first search from [`ENTRY`], with `next` as the successor
//!   function, reaches exactly the 28 pairs and all 12 states.
//! - **Absorption** (`INV-21`): from a terminal state `next` never moves (amendment 0006 items 1
//!   and 4 withdrew the one row that did).
//!
//! The store half, all 144 pairs sent straight to the store, is in [`super::store`]'s tests.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use ledger_kernel::posting::PostingKind;

use super::next::{Decision, Facts, next};
use super::table::{ENTRY, KindClass, Origin, State, TRANSITIONS, Trigger};

/// Every `(state, trigger, facts)` case and what `next` decided.
fn every_case() -> impl Iterator<Item = (State, Trigger, Facts, Decision)> {
    State::ALL.into_iter().flat_map(|s| {
        Trigger::ALL
            .into_iter()
            .flat_map(move |t| Facts::all().map(move |f| (s, t, f, next(s, t, &f))))
    })
}

#[test]
fn model_soundness_and_completeness() {
    let mut produced = BTreeSet::new();
    let mut cases = 0usize;
    for (s, t, f, d) in every_case() {
        cases += 1;
        if let Decision::Move { edge, posting } = d {
            assert_eq!(edge.from, s, "{s:?} {t:?} {f:?}");
            assert_eq!(edge.on, t, "{s:?} {t:?} {f:?}");
            assert!(
                TRANSITIONS.contains(edge),
                "{s:?} {t:?} {f:?} moved along a row outside the table"
            );
            assert!(
                edge.posting.allows(posting, f.reserved),
                "{s:?} {t:?} {f:?}: {posting:?} is not {:?}",
                edge.posting
            );
            produced.insert((edge.from, edge.to, edge.on));
        }
    }
    assert_eq!(cases, 12 * 22 * 20_480);
    let rows: BTreeSet<(State, State, Trigger)> =
        TRANSITIONS.iter().map(|e| (e.from, e.to, e.on)).collect();
    assert_eq!(produced, rows, "every row is produced by some case");
}

#[test]
fn model_reachability_from_entry() {
    // A node is (state, origin, kind): origin and kind are fixed for an operation's life.
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    for (state, origin) in ENTRY {
        for kind in KindClass::ALL {
            if seen.insert((state, origin, kind)) {
                queue.push_back((state, origin, kind));
            }
        }
    }
    let mut pairs = BTreeSet::new();
    while let Some((s, origin, kind)) = queue.pop_front() {
        for t in Trigger::ALL {
            for f in Facts::all().filter(|f| f.origin == origin && f.kind == kind) {
                if let Decision::Move { edge, .. } = next(s, t, &f) {
                    pairs.insert((edge.from, edge.to));
                    if seen.insert((edge.to, origin, kind)) {
                        queue.push_back((edge.to, origin, kind));
                    }
                }
            }
        }
    }
    let rows: BTreeSet<(State, State)> = TRANSITIONS.iter().map(|e| (e.from, e.to)).collect();
    assert_eq!(pairs, rows, "exactly the 28 pairs are reachable");
    let states: BTreeSet<State> = seen.iter().map(|(s, _, _)| *s).collect();
    assert_eq!(states.len(), 12, "all 12 states are reachable");
}

#[test]
fn model_absorption_inv_21() {
    for (s, t, f, d) in every_case() {
        if !s.is_terminal() {
            continue;
        }
        assert_eq!(
            d,
            Decision::Refuse(crate::refusal::Refusal::OperationTerminal),
            "{s:?} on {t:?} {f:?}"
        );
    }
}

/// No posting kind is appended twice on any path through the table, so `F-001`'s posting identity
/// `(operation, kind, ordinal 0)` never repeats within an operation (`REQ-KER-6`, `F-007` §6).
#[test]
fn model_no_posting_kind_repeats_on_any_path() {
    type Node = (State, Origin, KindClass, Vec<PostingKind>);
    let mut seen: BTreeSet<Node> = BTreeSet::new();
    let mut queue: VecDeque<Node> = VecDeque::new();
    for (state, origin) in ENTRY {
        for kind in KindClass::ALL {
            let n = (state, origin, kind, Vec::new());
            seen.insert(n.clone());
            queue.push_back(n);
        }
    }
    let mut longest: BTreeMap<KindClass, usize> = BTreeMap::new();
    while let Some((s, origin, kind, used)) = queue.pop_front() {
        let e = longest.entry(kind).or_default();
        *e = (*e).max(used.len());
        for t in Trigger::ALL {
            for f in Facts::all().filter(|f| f.origin == origin && f.kind == kind) {
                if let Decision::Move { edge, posting } = next(s, t, &f) {
                    let mut after = used.clone();
                    if let Some(k) = posting {
                        assert!(!used.contains(&k), "{k:?} twice: {used:?} then {edge:?}");
                        after.push(k);
                        after.sort();
                    }
                    let n = (edge.to, origin, kind, after);
                    if seen.insert(n.clone()) {
                        queue.push_back(n);
                    }
                }
            }
        }
    }
    // At most a reservation and one settling or releasing posting. A reversal is an operation
    // of its own, with its one `Reversal` posting (amendment 0006 item 1).
    assert!(longest.values().all(|n| *n <= 2), "{longest:?}");
}
