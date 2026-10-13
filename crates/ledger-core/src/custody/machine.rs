// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The closed state machine of a pending recovery or succession (`F-012` §6, `REQ-AU-16`).
//!
//! One record type, [`PendingChange`](super::PendingChange), for both kinds. Three states, two
//! edges, as a constant table in the manner of `F-007`'s [`crate::lifecycle::TRANSITIONS`]:
//!
//! | From | To | On |
//! |---|---|---|
//! | `Pending` | `Cancelled` | `Cancel`: an active member of the holder's set, scope covering the account |
//! | `Pending` | `Completed` | `Complete`: approved, and the delay elapsed |
//!
//! `Cancelled` and `Completed` absorb. [`next`] is the one decision function; the store
//! ([`super::store`]) refuses any pair outside [`CHANGE_TRANSITIONS`], whoever writes it. The
//! enumeration in this module's tests is the `F-007` model check applied to this table:
//! soundness, completeness, reachability from [`CHANGE_ENTRY`], absorption, and the store's
//! refusal of every other pair.

use crate::refusal::Refusal;

/// Where a pending change is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeState {
    /// Recorded, waiting for its delay; cancellable.
    Pending,
    /// Cancelled by the holder. Absorbing.
    Cancelled,
    /// Completed. Absorbing.
    Completed,
}

impl ChangeState {
    /// Every state.
    pub const ALL: [ChangeState; 3] = [
        ChangeState::Pending,
        ChangeState::Cancelled,
        ChangeState::Completed,
    ];

    /// Whether nothing leaves it.
    pub const fn terminal(self) -> bool {
        !matches!(self, ChangeState::Pending)
    }
}

/// What moves a pending change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeTrigger {
    /// `cancel_recovery`.
    Cancel,
    /// `complete_recovery`, or the successor's second `claim_succession`.
    Complete,
}

impl ChangeTrigger {
    /// Every trigger.
    pub const ALL: [ChangeTrigger; 2] = [ChangeTrigger::Cancel, ChangeTrigger::Complete];
}

/// One row of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangeEdge {
    /// The state it leaves.
    pub from: ChangeState,
    /// The state it enters.
    pub to: ChangeState,
    /// What moves it.
    pub on: ChangeTrigger,
}

/// The transitions of a pending change. No other transition exists.
pub const CHANGE_TRANSITIONS: [ChangeEdge; 2] = [
    ChangeEdge {
        from: ChangeState::Pending,
        to: ChangeState::Cancelled,
        on: ChangeTrigger::Cancel,
    },
    ChangeEdge {
        from: ChangeState::Pending,
        to: ChangeState::Completed,
        on: ChangeTrigger::Complete,
    },
];

/// The one state a pending change is recorded in.
pub const CHANGE_ENTRY: ChangeState = ChangeState::Pending;

/// What the command establishes before asking [`next`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangeFacts {
    /// The canceller presented an active member of the holder's set whose scope covers the
    /// account (`AC-A8.2`).
    pub holder: bool,
    /// The completion is approved: by the recovery authority's signature for a recovery
    /// (`REQ-AU-15`), by the named successor's own claim for a succession (`REQ-AU-17`).
    pub approved: bool,
    /// The delay has elapsed: `now ≥ effective_at` (`REQ-AU-16`).
    pub elapsed: bool,
}

impl ChangeFacts {
    /// Every combination, for the enumeration.
    pub fn all() -> impl Iterator<Item = ChangeFacts> {
        (0u8..8).map(|b| ChangeFacts {
            holder: b & 1 != 0,
            approved: b & 2 != 0,
            elapsed: b & 4 != 0,
        })
    }
}

/// What [`next`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeDecision {
    /// Move along this row.
    Move(&'static ChangeEdge),
    /// Refuse, with the named error; nothing changes.
    Refuse(Refusal),
}

/// The one decision function. Refusals, in order: a terminal change is
/// `ERR-OperationTerminal`; a cancel without the holder, or a completion without its approval, is
/// `ERR-NotAuthorised`; an approved completion before its delay is `ERR-RecoveryNotElapsed`.
pub fn next(state: ChangeState, trigger: ChangeTrigger, facts: ChangeFacts) -> ChangeDecision {
    if state.terminal() {
        return ChangeDecision::Refuse(Refusal::OperationTerminal);
    }
    let ok = match trigger {
        ChangeTrigger::Cancel if !facts.holder => Err(Refusal::NotAuthorised),
        ChangeTrigger::Cancel => Ok(()),
        ChangeTrigger::Complete if !facts.approved => Err(Refusal::NotAuthorised),
        ChangeTrigger::Complete if !facts.elapsed => Err(Refusal::RecoveryNotElapsed),
        ChangeTrigger::Complete => Ok(()),
    };
    match ok {
        Err(r) => ChangeDecision::Refuse(r),
        Ok(()) => CHANGE_TRANSITIONS
            .iter()
            .find(|e| e.from == state && e.on == trigger)
            .map_or(
                ChangeDecision::Refuse(Refusal::InvalidTransition),
                ChangeDecision::Move,
            ),
    }
}

/// Whether the store may move a change from `from` to `to`: exactly the rows of the table.
pub fn in_table(from: ChangeState, to: ChangeState) -> bool {
    CHANGE_TRANSITIONS
        .iter()
        .any(|e| e.from == from && e.to == to)
}

#[cfg(test)]
mod tests {
    //! The `F-007` enumeration, applied to this table (`F-012` test plan).
    use super::*;
    use alloc::collections::{BTreeSet, VecDeque};

    #[test]
    fn model_soundness_and_completeness() {
        let mut produced = BTreeSet::new();
        let mut cases = 0;
        for s in ChangeState::ALL {
            for t in ChangeTrigger::ALL {
                for f in ChangeFacts::all() {
                    cases += 1;
                    if let ChangeDecision::Move(e) = next(s, t, f) {
                        assert_eq!((e.from, e.on), (s, t));
                        assert!(CHANGE_TRANSITIONS.contains(e), "{s:?} {t:?} {f:?}");
                        produced.insert((e.from, e.to));
                    }
                }
            }
        }
        assert_eq!(cases, 3 * 2 * 8);
        let rows: BTreeSet<_> = CHANGE_TRANSITIONS.iter().map(|e| (e.from, e.to)).collect();
        assert_eq!(produced, rows, "every row is produced by some case");
    }

    #[test]
    fn model_reachability_from_entry() {
        let mut seen = BTreeSet::from([CHANGE_ENTRY]);
        let mut queue = VecDeque::from([CHANGE_ENTRY]);
        let mut pairs = BTreeSet::new();
        while let Some(s) = queue.pop_front() {
            for t in ChangeTrigger::ALL {
                for f in ChangeFacts::all() {
                    if let ChangeDecision::Move(e) = next(s, t, f) {
                        pairs.insert((e.from, e.to));
                        if seen.insert(e.to) {
                            queue.push_back(e.to);
                        }
                    }
                }
            }
        }
        assert_eq!(seen, ChangeState::ALL.into_iter().collect());
        assert_eq!(pairs.len(), CHANGE_TRANSITIONS.len());
    }

    #[test]
    fn model_terminal_states_absorb() {
        for s in ChangeState::ALL.into_iter().filter(|s| s.terminal()) {
            for t in ChangeTrigger::ALL {
                for f in ChangeFacts::all() {
                    assert_eq!(
                        next(s, t, f),
                        ChangeDecision::Refuse(Refusal::OperationTerminal)
                    );
                }
            }
        }
    }

    #[test]
    fn err_recovery_not_elapsed_only_for_an_approved_completion_before_its_delay() {
        for f in ChangeFacts::all() {
            let d = next(ChangeState::Pending, ChangeTrigger::Complete, f);
            let expect = match (f.approved, f.elapsed) {
                (false, _) => ChangeDecision::Refuse(Refusal::NotAuthorised),
                (true, false) => ChangeDecision::Refuse(Refusal::RecoveryNotElapsed),
                (true, true) => ChangeDecision::Move(&CHANGE_TRANSITIONS[1]),
            };
            assert_eq!(d, expect, "{f:?}");
        }
    }

    #[test]
    fn in_table_is_exactly_the_rows() {
        for from in ChangeState::ALL {
            for to in ChangeState::ALL {
                let row = CHANGE_TRANSITIONS
                    .iter()
                    .any(|e| (e.from, e.to) == (from, to));
                assert_eq!(in_table(from, to), row, "{from:?} → {to:?}");
            }
        }
    }
}
