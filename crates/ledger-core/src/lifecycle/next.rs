// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The one function that moves state, and the types around it (`F-007` §3, §4, §9, §10,
//! T-007-02, T-007-10).
//!
//! [`next`] is pure. It looks up the rows of [`TRANSITIONS`](super::table::TRANSITIONS) that
//! leave `state` on `trigger`, checks each row's guard over [`Facts`], and returns
//! [`Decision::Move`] only for a row of the table. Nothing else chooses a state: an operation's
//! state lives in a private field of [`Lifecycle`], and the store changes it only by a transition
//! it checks against the same table.

use ledger_kernel::posting::PostingKind;

use super::table::{Edge, KindClass, Origin, State, Trigger, rows};
use crate::account::Time;
use crate::refusal::Refusal;

/// What an operator resolution of `Needs-Review` asks for. `Needs-Review` resolves only to
/// `Settled` or `Released` (amendment 0006 item 4): value to return after that is a reversal
/// operation of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Resolution {
    /// Resolve to `Settled`.
    Settle,
    /// Resolve to `Released`.
    Release,
}

impl Resolution {
    /// Every resolution.
    pub const ALL: [Resolution; 2] = [Resolution::Settle, Resolution::Release];

    /// The state it resolves to.
    pub const fn target(self) -> State {
        match self {
            Resolution::Settle => State::Settled,
            Resolution::Release => State::Released,
        }
    }
}

/// What the caller of [`next`] established from the record, the evidence and the book. Small and
/// plain, so the model check can enumerate every combination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Facts {
    /// How the operation came to exist.
    pub origin: Origin,
    /// What its kind implies.
    pub kind: KindClass,
    /// Whether it holds a reservation.
    pub reserved: bool,
    /// Whether its deadline has passed.
    pub deadline_passed: bool,
    /// Whether its instruction's explicit expiry has passed.
    pub expiry_passed: bool,
    /// Whether the settlement layer's finality point has passed the observation.
    pub finality_passed: bool,
    /// Whether the settlement layer's canonical record still matches the observation.
    pub canonical_match: bool,
    /// Whether it is proven that the external instruction can no longer take effect
    /// (`REQ-SL-4`).
    pub non_inclusion_proven: bool,
    /// Whether a proved rejection holds: the settlement layer refused the instruction, final, in
    /// a transaction shown included (amendment 0006 item 3, `REQ-OP-9`).
    pub failure_proven: bool,
    /// Whether the deciding component, and the kernel for the edge's posting, accept.
    pub accepts: bool,
    /// Whether an operator's justification is present (`REQ-AT-3`).
    pub justified: bool,
    /// Whether a second, distinct principal approved (`REQ-RC-2`, `REQ-OS-3`).
    pub two_approvers: bool,
    /// What an operator resolution asks for.
    pub resolution: Resolution,
}

impl Facts {
    /// Facts with every condition false, for an operation of `kind` from `origin`.
    pub const fn new(origin: Origin, kind: KindClass) -> Facts {
        Facts {
            origin,
            kind,
            reserved: false,
            deadline_passed: false,
            expiry_passed: false,
            finality_passed: false,
            canonical_match: false,
            non_inclusion_proven: false,
            failure_proven: false,
            accepts: false,
            justified: false,
            two_approvers: false,
            resolution: Resolution::Settle,
        }
    }

    /// Every combination of facts: 2 × 5 × 2¹⁰ × 2 = 20 480.
    pub fn all() -> impl Iterator<Item = Facts> {
        Origin::ALL.into_iter().flat_map(|origin| {
            KindClass::ALL.into_iter().flat_map(move |kind| {
                (0u16..1024).flat_map(move |bits| {
                    Resolution::ALL.into_iter().map(move |resolution| {
                        let b = |i: u16| bits & (1 << i) != 0;
                        Facts {
                            origin,
                            kind,
                            reserved: b(0),
                            deadline_passed: b(1),
                            expiry_passed: b(2),
                            finality_passed: b(3),
                            canonical_match: b(4),
                            non_inclusion_proven: b(5),
                            accepts: b(6),
                            justified: b(7),
                            two_approvers: b(8),
                            failure_proven: b(9),
                            resolution,
                        }
                    })
                })
            })
        })
    }
}

/// Why [`next`] leaves the state as it is, with no refusal: the trigger is legitimate here, but
/// the facts do not allow the move yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// The deadline or the expiry has not passed.
    NotDue,
    /// Finality has not passed the observation.
    AwaitingFinality,
    /// The facts point to a different trigger (the canonical record matches, or does not; the
    /// non-inclusion is, or is not, proven). The evidence has to be read again.
    FactsDisagree,
    /// The kernel refused the edge's posting. The record stays open and queued: a kernel refusal
    /// is never, alone, a terminal outcome for a leg that can be observed (`INV-23`).
    KernelRefused,
    /// The instruction's expiry has passed, so it cannot be watched again; it waits for its
    /// deadline and an operator.
    NoLongerIncludable,
}

/// What [`next`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Move along a row of the table, appending this posting (or none).
    Move {
        /// The row.
        edge: &'static Edge,
        /// The posting the edge appends, chosen by the book-effect table.
        posting: Option<PostingKind>,
    },
    /// Stay, with no refusal.
    Stay(Reason),
    /// Refuse, with a condition of SPEC §10.
    Refuse(Refusal),
}

impl Decision {
    /// The state moved to, if the decision is a move.
    pub fn target(&self) -> Option<State> {
        match self {
            Decision::Move { edge, .. } => Some(edge.to),
            Decision::Stay(_) | Decision::Refuse(_) => None,
        }
    }
}

/// The only row of `rows(from, on)` that goes to `to`.
fn row(from: State, on: Trigger, to: State) -> &'static Edge {
    rows(from, on)
        .find(|e| e.to == to)
        .unwrap_or_else(|| unreachable!("§5.6 has {from:?} → {to:?} on {on:?}"))
}

fn go(from: State, on: Trigger, to: State, posting: Option<PostingKind>) -> Decision {
    Decision::Move {
        edge: row(from, on, to),
        posting,
    }
}

/// `Release` if the operation holds a reservation, otherwise none (the book effect of
/// `Released` and `Rejected`).
const fn release_if(reserved: bool) -> Option<PostingKind> {
    if reserved {
        Some(PostingKind::Release)
    } else {
        None
    }
}

/// The settling posting of an external leg: `Redeem` out, `Issue` in.
const fn leg_settlement(kind: KindClass) -> Option<PostingKind> {
    match kind {
        KindClass::Outbound => Some(PostingKind::Redeem),
        KindClass::Inbound => Some(PostingKind::Issue),
        KindClass::NoReservation | KindClass::ReservationOnly | KindClass::Reversal => None,
    }
}

/// Whether a release could let value go while the external leg may still execute
/// (`REQ-OP-9`, `ERR-ReleaseUnsafe`): the states between hand-off and a proven outcome.
/// `Reserved` is decided by its own row's guard: an outbound leg there may have been handed to
/// the adapter already.
const fn leg_may_still_execute(s: State) -> bool {
    match s {
        State::Submitted
        | State::Observed
        | State::Orphaned
        | State::Expired
        | State::NeedsReview => true,
        State::Received
        | State::Accepted
        | State::Reserved
        | State::Final
        | State::Settled
        | State::Released
        | State::Rejected => false,
    }
}

/// Decide what `trigger` does to an operation in `state`, given `facts` (SPEC §5.6).
///
/// - From a terminal state: `ERR-OperationTerminal`. No row leaves one (`INV-21`).
/// - No row leaves `state` on `trigger`: `ERR-ReleaseUnsafe` for a release while an external leg
///   may still execute, otherwise `ERR-InvalidTransition`.
/// - A row exists: its guard decides between [`Decision::Move`], [`Decision::Stay`] and a named
///   refusal. A kernel refusal on an edge that appends a posting is `Stay(KernelRefused)`: the
///   only edges a refusal alone can take are `Received → Rejected` and `Accepted → Rejected`,
///   before any balance effect (`INV-23`).
pub fn next(state: State, trigger: Trigger, f: &Facts) -> Decision {
    use Decision::{Refuse, Stay};
    use State as S;
    use Trigger as T;

    let has_row = rows(state, trigger).next().is_some();
    if state.is_terminal() {
        return Refuse(Refusal::OperationTerminal);
    }
    if !has_row {
        if trigger == T::ReleaseSafe && leg_may_still_execute(state) {
            return Refuse(Refusal::ReleaseUnsafe);
        }
        return Refuse(Refusal::InvalidTransition);
    }
    let invalid = Refuse(Refusal::InvalidTransition);
    match (state, trigger) {
        (S::Received, T::Decide) => {
            if f.origin != Origin::Command {
                invalid
            } else if f.accepts {
                go(state, trigger, S::Accepted, None)
            } else {
                go(state, trigger, S::Rejected, release_if(f.reserved))
            }
        }
        (S::Accepted, T::RefuseBeforeEffect) => {
            if f.reserved {
                invalid
            } else {
                go(state, trigger, S::Rejected, None)
            }
        }
        (S::Accepted, T::ApproveReversal) => {
            if f.kind != KindClass::Reversal {
                invalid
            } else if !f.justified {
                Refuse(Refusal::JustificationRequired)
            } else if !f.two_approvers {
                Refuse(Refusal::ApprovalRequired)
            } else if !f.accepts {
                Stay(Reason::KernelRefused)
            } else {
                go(state, trigger, S::Settled, Some(PostingKind::Reversal))
            }
        }
        (S::Accepted, T::SettleWithoutReservation) => {
            if f.kind != KindClass::NoReservation {
                invalid
            } else if !f.accepts {
                Stay(Reason::KernelRefused)
            } else {
                go(state, trigger, S::Settled, Some(PostingKind::Transfer))
            }
        }
        (S::Accepted, T::PostReservation) => {
            if !f.kind.reserves() {
                invalid
            } else if !f.accepts {
                Stay(Reason::KernelRefused)
            } else {
                go(state, trigger, S::Reserved, Some(PostingKind::Reserve))
            }
        }
        (S::Reserved, T::Capture) => {
            if f.kind != KindClass::ReservationOnly || !f.reserved {
                invalid
            } else if !f.accepts {
                Stay(Reason::KernelRefused)
            } else {
                go(state, trigger, S::Settled, Some(PostingKind::Capture))
            }
        }
        (S::Reserved, T::ReleaseSafe) => {
            let safe = match f.kind {
                KindClass::ReservationOnly => true,
                KindClass::Outbound => f.non_inclusion_proven,
                KindClass::NoReservation | KindClass::Inbound | KindClass::Reversal => false,
            };
            if !f.kind.reserves() {
                invalid
            } else if !safe {
                Refuse(Refusal::ReleaseUnsafe)
            } else if !f.accepts {
                Stay(Reason::KernelRefused)
            } else {
                go(state, trigger, S::Released, release_if(f.reserved))
            }
        }
        (S::Reserved, T::HandOffAcknowledged) => {
            if f.kind != KindClass::Outbound {
                invalid
            } else {
                go(state, trigger, S::Submitted, None)
            }
        }
        (S::Submitted, T::ProvisionalObservation) => {
            if f.kind != KindClass::Outbound {
                invalid
            } else {
                go(state, trigger, S::Observed, None)
            }
        }
        (S::Submitted, T::ExpiryPassedUnobserved) => {
            if !f.expiry_passed {
                Stay(Reason::NotDue)
            } else {
                go(state, trigger, S::Expired, None)
            }
        }
        // Amendment 0006 item 5: every non-terminal state but `Needs-Review` has this row.
        (
            S::Received
            | S::Accepted
            | S::Reserved
            | S::Submitted
            | S::Observed
            | S::Orphaned
            | S::Final
            | S::Expired,
            T::DeadlinePassed,
        ) => {
            if !f.deadline_passed {
                Stay(Reason::NotDue)
            } else {
                go(state, trigger, S::NeedsReview, None)
            }
        }
        // Amendment 0006 item 11: a release requested without proof, while the outbound leg may
        // still execute, is refused and the operation goes to a human. It moves no value. With a
        // proof in hand the facts disagree: the release itself applies, not this row.
        (
            S::Reserved | S::Submitted | S::Observed | S::Orphaned | S::Expired,
            T::UnsafeReleaseRefused,
        ) => {
            if f.kind != KindClass::Outbound {
                invalid
            } else if f.non_inclusion_proven || f.failure_proven {
                Stay(Reason::FactsDisagree)
            } else {
                go(state, trigger, S::NeedsReview, None)
            }
        }
        // Amendment 0006 item 3: a proved rejection releases at once, from every state it can
        // arrive in.
        (
            S::Reserved | S::Submitted | S::Observed | S::Orphaned | S::Expired,
            T::ProvedRejection,
        ) => {
            if f.kind != KindClass::Outbound {
                invalid
            } else if !f.failure_proven {
                Stay(Reason::FactsDisagree)
            } else if !f.accepts {
                Stay(Reason::KernelRefused)
            } else {
                go(state, trigger, S::Released, release_if(f.reserved))
            }
        }
        (S::Observed, T::FinalityAndCanonicalMatch) => {
            if !f.finality_passed {
                Stay(Reason::AwaitingFinality)
            } else if !f.canonical_match {
                Stay(Reason::FactsDisagree)
            } else {
                go(state, trigger, S::Final, None)
            }
        }
        (S::Observed, T::CanonicalMismatch) => {
            if f.canonical_match {
                Stay(Reason::FactsDisagree)
            } else {
                go(state, trigger, S::Orphaned, None)
            }
        }
        (S::Orphaned, T::ReincludableAgain) => {
            if f.origin != Origin::Command || f.kind != KindClass::Outbound {
                invalid
            } else if f.expiry_passed {
                Stay(Reason::NoLongerIncludable)
            } else {
                go(state, trigger, S::Submitted, None)
            }
        }
        (S::Orphaned, T::InboundWithdrawn) => {
            if f.origin != Origin::UnoriginatedInbound {
                invalid
            } else {
                go(state, trigger, S::Released, release_if(f.reserved))
            }
        }
        (S::Final, T::AppendSettling) => match leg_settlement(f.kind) {
            None => invalid,
            Some(_) if !f.accepts => Stay(Reason::KernelRefused),
            Some(k) => go(state, trigger, S::Settled, Some(k)),
        },
        (S::Expired, T::NonInclusionProven) => {
            if !f.non_inclusion_proven {
                Stay(Reason::FactsDisagree)
            } else if !f.accepts {
                Stay(Reason::KernelRefused)
            } else {
                go(state, trigger, S::Released, release_if(f.reserved))
            }
        }
        (S::Expired, T::NonInclusionUnprovable) => {
            if f.non_inclusion_proven {
                Stay(Reason::FactsDisagree)
            } else {
                go(state, trigger, S::NeedsReview, None)
            }
        }
        // `REQ-RC-5` steps 1 and 2: the settlement adapter's proof resolves it, no human. Settled
        // on a final record of the leg (finality and a canonical match); Released on a proved
        // rejection or proven non-inclusion. Only an external leg has a proof to give.
        (S::NeedsReview, T::ProvenOutcome) => {
            let proven_settled = f.finality_passed && f.canonical_match;
            let proven_released = f.failure_proven || f.non_inclusion_proven;
            match (leg_settlement(f.kind), proven_settled, proven_released) {
                (None, _, _) => invalid,
                (Some(_), true, true) | (Some(_), false, false) => Stay(Reason::FactsDisagree),
                (Some(_), _, _) if !f.accepts => Stay(Reason::KernelRefused),
                (Some(k), true, false) => go(state, trigger, S::Settled, Some(k)),
                (Some(_), false, true) => go(state, trigger, S::Released, release_if(f.reserved)),
            }
        }
        // `REQ-RC-5` steps 2 and 3: a human resolution. With the adapter's proof one principal is
        // enough; without it, a release needs two distinct principals (step 3), and a settlement
        // of an external leg is never made without the proof of finality (`INV-10`, `OQ-62`).
        (S::NeedsReview, T::OperatorResolve) => {
            if !f.justified {
                return Refuse(Refusal::JustificationRequired);
            }
            let leg = f.kind.has_external_leg();
            match f.resolution {
                Resolution::Settle if leg && !(f.finality_passed && f.canonical_match) => {
                    Refuse(Refusal::EvidenceInsufficient)
                }
                Resolution::Release
                    if leg && !(f.failure_proven || f.non_inclusion_proven || f.two_approvers) =>
                {
                    Refuse(Refusal::ApprovalRequired)
                }
                Resolution::Settle => {
                    let posting = match f.kind {
                        KindClass::ReservationOnly if f.reserved => Some(PostingKind::Capture),
                        KindClass::Outbound if f.reserved => Some(PostingKind::Redeem),
                        KindClass::Inbound => Some(PostingKind::Issue),
                        KindClass::ReservationOnly
                        | KindClass::Outbound
                        | KindClass::NoReservation
                        | KindClass::Reversal => None,
                    };
                    match posting {
                        None => invalid,
                        Some(_) if !f.accepts => Stay(Reason::KernelRefused),
                        Some(k) => go(state, trigger, S::Settled, Some(k)),
                    }
                }
                Resolution::Release => {
                    if !f.accepts {
                        Stay(Reason::KernelRefused)
                    } else {
                        go(state, trigger, S::Released, release_if(f.reserved))
                    }
                }
            }
        }
        // Every row of the table is handled above; `has_row` rules the rest out.
        _ => unreachable!("§5.6 has no row for {state:?} on {trigger:?}"),
    }
}

/// An operation's place in the lifecycle. An open operation cannot exist without a deadline
/// (`INV-15`, `ERR-DeadlineRequired`), and a closed one only in a terminal state. The fields are
/// private: the only ways to build one check both rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lifecycle(Inner);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Inner {
    Open { state: State, deadline: Time },
    Closed { state: State, at: Time },
}

impl Lifecycle {
    /// A non-terminal operation. `ERR-DeadlineRequired` without a deadline;
    /// `ERR-InvalidTransition` for a terminal state.
    pub fn open(state: State, deadline: Option<Time>) -> Result<Lifecycle, Refusal> {
        if state.is_terminal() {
            return Err(Refusal::InvalidTransition);
        }
        let deadline = deadline.ok_or(Refusal::DeadlineRequired)?;
        Ok(Lifecycle(Inner::Open { state, deadline }))
    }

    /// A terminal operation, closed at `at`. `ERR-InvalidTransition` for a non-terminal state.
    pub fn closed(state: State, at: Time) -> Result<Lifecycle, Refusal> {
        if !state.is_terminal() {
            return Err(Refusal::InvalidTransition);
        }
        Ok(Lifecycle(Inner::Closed { state, at }))
    }

    /// `state`, open with `deadline` or closed at `at` as the state requires.
    pub fn enter(state: State, deadline: Option<Time>, at: Time) -> Result<Lifecycle, Refusal> {
        if state.is_terminal() {
            Lifecycle::closed(state, at)
        } else {
            Lifecycle::open(state, deadline)
        }
    }

    /// The state.
    pub const fn state(&self) -> State {
        match self.0 {
            Inner::Open { state, .. } | Inner::Closed { state, .. } => state,
        }
    }

    /// The deadline, while open.
    pub const fn deadline(&self) -> Option<Time> {
        match self.0 {
            Inner::Open { deadline, .. } => Some(deadline),
            Inner::Closed { .. } => None,
        }
    }

    /// When it closed, once closed.
    pub const fn closed_at(&self) -> Option<Time> {
        match self.0 {
            Inner::Open { .. } => None,
            Inner::Closed { at, .. } => Some(at),
        }
    }

    /// Whether it is open.
    pub const fn is_open(&self) -> bool {
        matches!(self.0, Inner::Open { .. })
    }
}

/// How a state reads to a caller (`REQ-OP-7`): never ambiguous, never success while open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateClass {
    /// Terminal: the outcome is final.
    Terminal,
    /// Open: not terminal and not refunding. Nothing has returned yet.
    OpenNonRefunding,
    /// `Needs-Review`: waiting for a human. Not terminal and not refunding.
    AwaitingHuman,
}

impl StateClass {
    /// The class of `state`.
    pub const fn of(state: State) -> StateClass {
        match state {
            State::NeedsReview => StateClass::AwaitingHuman,
            State::Settled | State::Released | State::Rejected => StateClass::Terminal,
            State::Received
            | State::Accepted
            | State::Reserved
            | State::Submitted
            | State::Observed
            | State::Final
            | State::Expired
            | State::Orphaned => StateClass::OpenNonRefunding,
        }
    }

    /// Whether the class is terminal.
    pub const fn is_terminal(self) -> bool {
        matches!(self, StateClass::Terminal)
    }
}

/// A state as shown to callers: the state and its class, side by side (`REQ-OP-7`). Every state
/// is shown, and every transition is pushed (`REQ-EV-1`); `REQ-OP-12` names the floor (`Received`,
/// `Reserved`, `Final`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Shown {
    /// The state.
    pub state: State,
    /// Its class.
    pub class: StateClass,
}

impl Shown {
    /// `state` with its class.
    pub const fn of(state: State) -> Shown {
        Shown {
            state,
            class: StateClass::of(state),
        }
    }
}

/// What happens to an open operation whose deadline has passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PastDeadline {
    /// Fire this trigger: the state's deadline row to `Needs-Review`.
    Fire(Trigger),
    /// Already waiting for an operator: past the review service level, it stays and is escalated.
    InReview,
    /// Terminal: no deadline.
    Closed,
}

/// What a passed deadline does in `state` (`F-007` §8, §9; `REQ-OP-5`).
///
/// Amendment 0006 item 5 (ruled 2026-09-29): one rule for every non-terminal state. Past its
/// deadline an operation moves to `Needs-Review`, where `resolve_operation` applies. `Received`'s
/// deadline is the decision bound of `REQ-CS-7`; passing it is also an incident. Terminal states
/// carry no deadline. `Needs-Review` keeps its review service level and stays where it is.
pub const fn past_deadline(state: State) -> PastDeadline {
    match state {
        State::Received
        | State::Accepted
        | State::Reserved
        | State::Submitted
        | State::Observed
        | State::Final
        | State::Expired
        | State::Orphaned => PastDeadline::Fire(Trigger::DeadlinePassed),
        State::NeedsReview => PastDeadline::InReview,
        State::Settled | State::Released | State::Rejected => PastDeadline::Closed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts::new(Origin::Command, KindClass::ReservationOnly)
    }

    #[test]
    fn open_needs_a_deadline_and_closed_needs_a_terminal_state() {
        assert_eq!(
            Lifecycle::open(State::Reserved, None),
            Err(Refusal::DeadlineRequired)
        );
        assert_eq!(
            Lifecycle::open(State::Settled, Some(5)),
            Err(Refusal::InvalidTransition)
        );
        assert_eq!(
            Lifecycle::closed(State::Reserved, 5),
            Err(Refusal::InvalidTransition)
        );
        let open = Lifecycle::open(State::Reserved, Some(5)).unwrap();
        assert_eq!(
            (open.state(), open.deadline(), open.is_open()),
            (State::Reserved, Some(5), true)
        );
        let closed = Lifecycle::closed(State::Released, 7).unwrap();
        assert_eq!(
            (closed.state(), closed.closed_at(), closed.deadline()),
            (State::Released, Some(7), None)
        );
        assert_eq!(
            Lifecycle::enter(State::Accepted, None, 1),
            Err(Refusal::DeadlineRequired)
        );
        assert!(Lifecycle::enter(State::Rejected, None, 1).is_ok());
    }

    #[test]
    fn guards_decide_between_move_stay_and_refuse() {
        let mut f = facts();
        // Decide: accepted or rejected, by the facts.
        f.accepts = true;
        assert_eq!(
            next(State::Received, Trigger::Decide, &f).target(),
            Some(State::Accepted)
        );
        f.accepts = false;
        assert_eq!(
            next(State::Received, Trigger::Decide, &f).target(),
            Some(State::Rejected)
        );
        // Only a command is decided.
        let inbound = Facts::new(Origin::UnoriginatedInbound, KindClass::Inbound);
        assert_eq!(
            next(State::Received, Trigger::Decide, &inbound),
            Decision::Refuse(Refusal::InvalidTransition)
        );
        // A capture the kernel refuses leaves the record where it is.
        f.reserved = true;
        assert_eq!(
            next(State::Reserved, Trigger::Capture, &f),
            Decision::Stay(Reason::KernelRefused)
        );
        f.accepts = true;
        assert_eq!(
            next(State::Reserved, Trigger::Capture, &f),
            Decision::Move {
                edge: super::super::table::find(State::Reserved, State::Settled).unwrap(),
                posting: Some(PostingKind::Capture)
            }
        );
        // Terminal states absorb.
        assert_eq!(
            next(State::Settled, Trigger::Capture, &f),
            Decision::Refuse(Refusal::OperationTerminal)
        );
        assert_eq!(
            next(State::Released, Trigger::ApproveReversal, &f),
            Decision::Refuse(Refusal::OperationTerminal)
        );
        // No row: invalid, or unsafe for a release while a leg may still execute.
        assert_eq!(
            next(State::Reserved, Trigger::AppendSettling, &f),
            Decision::Refuse(Refusal::InvalidTransition)
        );
        assert_eq!(
            next(State::Submitted, Trigger::ReleaseSafe, &f),
            Decision::Refuse(Refusal::ReleaseUnsafe)
        );
        // An outbound reservation is released only when it provably cannot execute.
        let mut out = Facts::new(Origin::Command, KindClass::Outbound);
        out.reserved = true;
        out.accepts = true;
        assert_eq!(
            next(State::Reserved, Trigger::ReleaseSafe, &out),
            Decision::Refuse(Refusal::ReleaseUnsafe)
        );
        out.non_inclusion_proven = true;
        assert_eq!(
            next(State::Reserved, Trigger::ReleaseSafe, &out).target(),
            Some(State::Released)
        );
        // A deadline that has not passed moves nothing.
        assert_eq!(
            next(State::Submitted, Trigger::DeadlinePassed, &out),
            Decision::Stay(Reason::NotDue)
        );
        // Finality needs both facts (REQ-IN-3).
        out.finality_passed = true;
        assert_eq!(
            next(State::Observed, Trigger::FinalityAndCanonicalMatch, &out),
            Decision::Stay(Reason::FactsDisagree)
        );
        out.canonical_match = true;
        assert_eq!(
            next(State::Observed, Trigger::FinalityAndCanonicalMatch, &out).target(),
            Some(State::Final)
        );
        // Operator resolution needs a justification.
        let mut nr = out;
        nr.resolution = Resolution::Release;
        assert_eq!(
            next(State::NeedsReview, Trigger::OperatorResolve, &nr),
            Decision::Refuse(Refusal::JustificationRequired)
        );
        nr.justified = true;
        assert_eq!(
            next(State::NeedsReview, Trigger::OperatorResolve, &nr).target(),
            Some(State::Released)
        );
        // A reversal operation settles only on an approval by a second principal (item 1).
        let mut rv = Facts::new(Origin::Command, KindClass::Reversal);
        rv.accepts = true;
        assert_eq!(
            next(State::Accepted, Trigger::ApproveReversal, &rv),
            Decision::Refuse(Refusal::JustificationRequired)
        );
        rv.justified = true;
        assert_eq!(
            next(State::Accepted, Trigger::ApproveReversal, &rv),
            Decision::Refuse(Refusal::ApprovalRequired)
        );
        rv.two_approvers = true;
        assert_eq!(
            next(State::Accepted, Trigger::ApproveReversal, &rv),
            Decision::Move {
                edge: super::super::table::row(
                    State::Accepted,
                    State::Settled,
                    Trigger::ApproveReversal
                )
                .unwrap(),
                posting: Some(PostingKind::Reversal)
            }
        );
        // No other kind settles by approval, and a reversal never settles as a transfer.
        assert_eq!(
            next(State::Accepted, Trigger::ApproveReversal, &nr),
            Decision::Refuse(Refusal::InvalidTransition)
        );
        assert_eq!(
            next(State::Accepted, Trigger::SettleWithoutReservation, &rv),
            Decision::Refuse(Refusal::InvalidTransition)
        );
    }

    /// `REQ-OP-9`, amendment 0006 item 3: a proved rejection releases an outbound reservation at
    /// once, from every state it can arrive in; without the proof nothing moves.
    #[test]
    fn req_op_9_a_proved_rejection_releases_at_once() {
        let mut out = Facts::new(Origin::Command, KindClass::Outbound);
        out.reserved = true;
        out.accepts = true;
        for s in [
            State::Reserved,
            State::Submitted,
            State::Observed,
            State::Orphaned,
            State::Expired,
        ] {
            assert_eq!(
                next(s, Trigger::ProvedRejection, &out),
                Decision::Stay(Reason::FactsDisagree),
                "{s:?}"
            );
            let mut proven = out;
            proven.failure_proven = true;
            assert_eq!(
                next(s, Trigger::ProvedRejection, &proven),
                Decision::Move {
                    edge: super::super::table::row(s, State::Released, Trigger::ProvedRejection)
                        .unwrap(),
                    posting: Some(PostingKind::Release)
                },
                "{s:?}"
            );
        }
        // Only an outbound leg has a rejection to prove.
        let mut own = facts();
        own.failure_proven = true;
        own.reserved = true;
        own.accepts = true;
        assert_eq!(
            next(State::Reserved, Trigger::ProvedRejection, &own),
            Decision::Refuse(Refusal::InvalidTransition)
        );
    }

    /// `AC-C3.2`, amendment 0006 item 11: a release without proof, while the leg may still
    /// execute, moves the operation to `Needs-Review` and appends nothing. With a proof it does
    /// not apply: the release does.
    #[test]
    fn ac_c3_2_an_unproven_release_goes_to_review_and_moves_no_value() {
        let mut out = Facts::new(Origin::Command, KindClass::Outbound);
        out.reserved = true;
        out.accepts = true;
        for s in [
            State::Reserved,
            State::Submitted,
            State::Observed,
            State::Orphaned,
            State::Expired,
        ] {
            assert_eq!(
                next(s, Trigger::UnsafeReleaseRefused, &out),
                Decision::Move {
                    edge: super::super::table::row(
                        s,
                        State::NeedsReview,
                        Trigger::UnsafeReleaseRefused
                    )
                    .unwrap(),
                    posting: None
                },
                "{s:?}"
            );
            let mut proven = out;
            proven.failure_proven = true;
            assert_eq!(
                next(s, Trigger::UnsafeReleaseRefused, &proven),
                Decision::Stay(Reason::FactsDisagree)
            );
        }
        let mut own = facts();
        own.reserved = true;
        assert_eq!(
            next(State::Reserved, Trigger::UnsafeReleaseRefused, &own),
            Decision::Refuse(Refusal::InvalidTransition)
        );
    }

    /// INV-23: a kernel refusal on an evidence-driven edge never ends the operation.
    #[test]
    fn inv_23_a_kernel_refusal_at_final_stays_open() {
        for kind in [KindClass::Outbound, KindClass::Inbound] {
            let mut f = Facts::new(Origin::Command, kind);
            f.reserved = kind == KindClass::Outbound;
            f.accepts = false;
            assert_eq!(
                next(State::Final, Trigger::AppendSettling, &f),
                Decision::Stay(Reason::KernelRefused)
            );
        }
    }

    #[test]
    fn state_classes_label_every_open_state_non_terminal() {
        for s in State::ALL {
            let c = StateClass::of(s);
            assert_eq!(c.is_terminal(), s.is_terminal(), "{s:?}");
            if s == State::NeedsReview {
                assert_eq!(c, StateClass::AwaitingHuman);
            }
        }
    }

    /// `REQ-OP-5`, amendment 0006 item 5: past its deadline every non-terminal state fires its
    /// row to `Needs-Review`; terminal states carry no deadline.
    #[test]
    fn req_op_5_past_deadline_fires_needs_review_from_every_non_terminal_state() {
        for s in State::ALL {
            match past_deadline(s) {
                PastDeadline::Fire(t) => {
                    assert_eq!(t, Trigger::DeadlinePassed);
                    let mut f = facts();
                    f.deadline_passed = true;
                    assert_eq!(next(s, t, &f).target(), Some(State::NeedsReview), "{s:?}");
                }
                PastDeadline::Closed => assert!(s.is_terminal()),
                PastDeadline::InReview => assert_eq!(s, State::NeedsReview),
            }
        }
    }
}
