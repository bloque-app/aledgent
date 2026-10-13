// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The transition table of SPEC §5.6, as data (`F-007` §1–§2, T-007-01).
//!
//! [`TRANSITIONS`] is the only source of edges. [`next`](super::next::next) looks rows up in it, the
//! store refuses any row outside it ([`super::store`]), and a test reads SPEC §5.6 itself and
//! asserts that the pairs are the same. An amendment that changes the table fails the build until
//! this constant follows.
//!
//! A row is a (from, to, trigger) triple. Where one "When" cell of §5.6 names several conditions,
//! the pair has one row per condition, and every row of a pair appends the same kind of posting
//! (a test checks it). Amendment 0006 (ruled 2026-09-29) gave the table its present shape: no
//! `Reversed` state and no row out of a terminal state (items 1 and 4); a proved rejection
//! releases (item 3); every non-terminal state has a deadline row to `Needs-Review` (item 5); a
//! release requested without proof moves to `Needs-Review` and releases nothing (item 11); and a
//! reversal is an operation of its own, settled by approval (item 1).

use ledger_kernel::posting::PostingKind;

/// A closed, fieldless enum with its `ALL` array generated from the same list, so a variant
/// cannot be added without appearing in `ALL`. Every `match` on these enums in this module names
/// every variant, with no wildcard, so adding one also fails to compile until it is handled.
macro_rules! closed_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: [$name; [$($name::$variant),+].len()] = [$($name::$variant),+];
        }
    };
}

closed_enum! {
    /// The twelve states of an operation (SPEC §5.6). `Reversed` (code 13) was withdrawn by
    /// amendment 0006 item 1; a reversal is an operation of its own.
    pub enum State {
        /// Durably taken in, not yet decided. No balance effect.
        Received,
        /// Decided and taken.
        Accepted,
        /// The reservation is posted.
        Reserved,
        /// The instruction is durably handed to the settlement adapter.
        Submitted,
        /// A provisional observation names the leg.
        Observed,
        /// Finality has passed the observation and the canonical record still matches.
        Final,
        /// Terminal: the settling posting is appended.
        Settled,
        /// Terminal: nothing moved, or the reservation returned to available.
        Released,
        /// Terminal: refused before any balance effect.
        Rejected,
        /// The instruction's explicit expiry passed with no observation.
        Expired,
        /// The canonical record no longer matches the observation.
        Orphaned,
        /// Its outcome can be neither established nor shown impossible: a human resolves it.
        NeedsReview,
    }
}

impl State {
    /// The spelling SPEC §5.6 uses.
    pub const fn name(self) -> &'static str {
        match self {
            State::Received => "Received",
            State::Accepted => "Accepted",
            State::Reserved => "Reserved",
            State::Submitted => "Submitted",
            State::Observed => "Observed",
            State::Final => "Final",
            State::Settled => "Settled",
            State::Released => "Released",
            State::Rejected => "Rejected",
            State::Expired => "Expired",
            State::Orphaned => "Orphaned",
            State::NeedsReview => "Needs-Review",
        }
    }

    /// The state SPEC §5.6 spells `name`.
    pub fn from_name(name: &str) -> Option<State> {
        State::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Whether the state is terminal (`REQ-OP-4`): `Settled`, `Released`, `Rejected`.
    pub const fn is_terminal(self) -> bool {
        match self {
            State::Settled | State::Released | State::Rejected => true,
            State::Received
            | State::Accepted
            | State::Reserved
            | State::Submitted
            | State::Observed
            | State::Final
            | State::Expired
            | State::Orphaned
            | State::NeedsReview => false,
        }
    }

    /// A stable one-byte code, for key derivations. Never reorder.
    pub const fn code(self) -> u8 {
        match self {
            State::Received => 1,
            State::Accepted => 2,
            State::Reserved => 3,
            State::Submitted => 4,
            State::Observed => 5,
            State::Final => 6,
            State::Settled => 7,
            State::Released => 8,
            State::Rejected => 9,
            State::Expired => 10,
            State::Orphaned => 11,
            State::NeedsReview => 12,
            // 13 was `Reversed`: never reused.
        }
    }

    /// The book effect of entering this state (SPEC §5.6, second table): what posting the edge
    /// into it may append.
    pub const fn book_effect(self) -> PostingRule {
        match self {
            State::Received | State::Accepted => PostingRule::None,
            State::Reserved => PostingRule::Reserve,
            State::Submitted
            | State::Observed
            | State::Final
            | State::Expired
            | State::Orphaned
            | State::NeedsReview => PostingRule::None,
            State::Settled => PostingRule::SettleOneOf(&SETTLE_ANY),
            State::Released | State::Rejected => PostingRule::ReleaseIfReserved,
        }
    }
}

closed_enum! {
    /// What moves an operation: one variant per "When" cell of SPEC §5.6.
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub enum Trigger {
        /// The deciding component takes the command (`Received → Accepted | Rejected`).
        Decide,
        /// Refused before any balance effect (`Accepted → Rejected`).
        RefuseBeforeEffect,
        /// No external leg and no reservation: final on acceptance (`Accepted → Settled`).
        SettleWithoutReservation,
        /// The reservation is posted (`Accepted → Reserved`).
        PostReservation,
        /// Captured, with no external leg (`Reserved → Settled`).
        Capture,
        /// Released, with no external leg or one that provably cannot execute
        /// (`Reserved → Released`).
        ReleaseSafe,
        /// The instruction is durably handed to the settlement adapter (`Reserved → Submitted`).
        HandOffAcknowledged,
        /// A provisional observation names the leg (`Submitted → Observed`).
        ProvisionalObservation,
        /// The explicit expiry passed with no observation (`Submitted → Expired`).
        ExpiryPassedUnobserved,
        /// The operation's deadline passed: every non-terminal state but `Needs-Review` →
        /// `Needs-Review` (amendment 0006 item 5, `REQ-OP-5`).
        DeadlinePassed,
        /// Finality passed the observation and the canonical record matches (`Observed → Final`).
        FinalityAndCanonicalMatch,
        /// The canonical record no longer matches (`Observed → Orphaned`).
        CanonicalMismatch,
        /// The same instruction can still be included (`Orphaned → Submitted`).
        ReincludableAgain,
        /// An inbound arrival aledgent did not originate was withdrawn (`Orphaned → Released`).
        InboundWithdrawn,
        /// The settling posting is appended (`Final → Settled`).
        AppendSettling,
        /// Non-inclusion is proven (`Expired → Released`).
        NonInclusionProven,
        /// Non-inclusion cannot be proven (`Expired → Needs-Review`).
        NonInclusionUnprovable,
        /// An operator resolves it with evidence and a justification
        /// (`Needs-Review → Settled | Released`).
        OperatorResolve,
        /// A proved rejection: the settlement layer refused the instruction, final, in a
        /// transaction shown included, so it cannot take effect (`Reserved | Submitted | Observed |
        /// Orphaned | Expired → Released`; amendment 0006 item 3, `REQ-OP-9`).
        ProvedRejection,
        /// A release requested without proof, while the external leg may still execute, refused
        /// with `ERR-ReleaseUnsafe` (`Reserved | Submitted | Observed | Orphaned | Expired →
        /// Needs-Review`; amendment 0006 item 11, `AC-C3.2`). It moves no value.
        UnsafeReleaseRefused,
        /// A reversal operation approved by a principal other than its proposer
        /// (`Accepted → Settled`; amendment 0006 item 1, `REQ-OS-3`).
        ApproveReversal,
        /// The settlement adapter proves the outcome of a `Needs-Review` operation, on its own or
        /// from evidence an authority submitted (`Needs-Review → Settled | Released`; `REQ-RC-5`,
        /// steps 1 and 2). No human decides it.
        ProvenOutcome,
    }
}

impl Trigger {
    /// A stable one-byte code. Never reorder.
    pub const fn code(self) -> u8 {
        match self {
            Trigger::Decide => 1,
            Trigger::RefuseBeforeEffect => 2,
            Trigger::SettleWithoutReservation => 3,
            Trigger::PostReservation => 4,
            Trigger::Capture => 5,
            Trigger::ReleaseSafe => 6,
            Trigger::HandOffAcknowledged => 7,
            Trigger::ProvisionalObservation => 8,
            Trigger::ExpiryPassedUnobserved => 9,
            Trigger::DeadlinePassed => 10,
            Trigger::FinalityAndCanonicalMatch => 11,
            Trigger::CanonicalMismatch => 12,
            Trigger::ReincludableAgain => 13,
            Trigger::InboundWithdrawn => 14,
            Trigger::AppendSettling => 15,
            Trigger::NonInclusionProven => 16,
            Trigger::NonInclusionUnprovable => 17,
            Trigger::OperatorResolve => 18,
            // 19 was `ApprovedReversal` (`Settled → Reversed`): never reused.
            Trigger::ProvedRejection => 20,
            Trigger::UnsafeReleaseRefused => 21,
            Trigger::ApproveReversal => 22,
            Trigger::ProvenOutcome => 23,
        }
    }
}

closed_enum! {
    /// How an operation came to exist.
    pub enum Origin {
        /// A command a principal submitted. Enters at `Received`.
        Command,
        /// An inbound arrival aledgent did not originate. Enters at `Observed`.
        UnoriginatedInbound,
    }
}

impl Origin {
    /// A stable one-byte code.
    pub const fn code(self) -> u8 {
        match self {
            Origin::Command => 1,
            Origin::UnoriginatedInbound => 2,
        }
    }
}

closed_enum! {
    /// What an operation's kind implies for the lifecycle (SPEC §5.6: "an operation's kind, as set
    /// in §8.1, decides whether it reserves at all").
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub enum KindClass {
        /// No reservation and no external leg: an internal transfer (`F-008`).
        NoReservation,
        /// A reservation, captured or released, with no external leg (`F-004`).
        ReservationOnly,
        /// An outbound leg: always reserves first (`REQ-SO-1`, `F-010`).
        Outbound,
        /// An inbound leg: nothing reserved, value issued at finality (`F-010`).
        Inbound,
        /// A reversal operation: no reservation and no external leg; settled only by approval,
        /// with one `Reversal` posting (`F-013`, amendment 0006 item 1).
        Reversal,
    }
}

impl KindClass {
    /// A stable one-byte code.
    pub const fn code(self) -> u8 {
        match self {
            KindClass::NoReservation => 1,
            KindClass::ReservationOnly => 2,
            KindClass::Outbound => 3,
            KindClass::Inbound => 4,
            KindClass::Reversal => 5,
        }
    }

    /// Whether operations of this class reserve before anything else.
    pub const fn reserves(self) -> bool {
        match self {
            KindClass::ReservationOnly | KindClass::Outbound => true,
            KindClass::NoReservation | KindClass::Inbound | KindClass::Reversal => false,
        }
    }

    /// Whether operations of this class have a leg on the settlement layer that can be observed
    /// independently (`INV-23`).
    pub const fn has_external_leg(self) -> bool {
        match self {
            KindClass::Outbound | KindClass::Inbound => true,
            KindClass::NoReservation | KindClass::ReservationOnly | KindClass::Reversal => false,
        }
    }
}

/// Every settling kind of SPEC §5.6's book-effect table.
const SETTLE_ANY: [PostingKind; 5] = [
    PostingKind::Transfer,
    PostingKind::Capture,
    PostingKind::Redeem,
    PostingKind::Issue,
    PostingKind::Reversal,
];

/// Which posting an edge may append (SPEC §5.6, book effects; `REQ-OP-1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostingRule {
    /// No posting.
    None,
    /// `Reserve`: debit available, credit reserved.
    Reserve,
    /// Exactly one of these settling kinds, chosen by the operation's kind class.
    SettleOneOf(&'static [PostingKind]),
    /// `Release` if a reservation exists; otherwise none.
    ReleaseIfReserved,
}

impl PostingRule {
    /// Whether a posting of `kind` (or none) is one this rule allows. `reserved` says whether the
    /// operation holds a reservation.
    pub fn allows(self, kind: Option<PostingKind>, reserved: bool) -> bool {
        match (self, kind) {
            (PostingRule::None, None) => true,
            (PostingRule::Reserve, Some(PostingKind::Reserve)) => true,
            (PostingRule::SettleOneOf(kinds), Some(k)) => kinds.contains(&k),
            (PostingRule::ReleaseIfReserved, Some(PostingKind::Release)) => reserved,
            (PostingRule::ReleaseIfReserved, None) => !reserved,
            _ => false,
        }
    }

    /// Every posting kind the rule may append.
    pub fn kinds(self) -> &'static [PostingKind] {
        match self {
            PostingRule::None => &[],
            PostingRule::Reserve => &[PostingKind::Reserve],
            PostingRule::SettleOneOf(kinds) => kinds,
            PostingRule::ReleaseIfReserved => &[PostingKind::Release],
        }
    }
}

/// One row of SPEC §5.6: its "or" cells expanded into one row per (from, to) pair, and each
/// condition of a "When" cell into one row per trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    /// The state it leaves.
    pub from: State,
    /// The state it enters.
    pub to: State,
    /// What moves it.
    pub on: Trigger,
    /// The posting it may append.
    pub posting: PostingRule,
}

const fn edge(from: State, to: State, on: Trigger, posting: PostingRule) -> Edge {
    Edge {
        from,
        to,
        on,
        posting,
    }
}

/// The settling kinds each settling edge may append.
const SETTLE_TRANSFER: [PostingKind; 1] = [PostingKind::Transfer];
const SETTLE_REVERSAL: [PostingKind; 1] = [PostingKind::Reversal];
const SETTLE_CAPTURE: [PostingKind; 1] = [PostingKind::Capture];
const SETTLE_LEG: [PostingKind; 2] = [PostingKind::Redeem, PostingKind::Issue];
const SETTLE_RESOLVED: [PostingKind; 3] = [
    PostingKind::Capture,
    PostingKind::Redeem,
    PostingKind::Issue,
];

use PostingRule as P;
use State as S;
use Trigger as T;

/// The transitions of SPEC §5.6. No other transition exists.
pub const TRANSITIONS: [Edge; 40] = [
    edge(S::Received, S::Accepted, T::Decide, P::None),
    edge(S::Received, S::Rejected, T::Decide, P::ReleaseIfReserved),
    edge(S::Received, S::NeedsReview, T::DeadlinePassed, P::None),
    edge(
        S::Accepted,
        S::Rejected,
        T::RefuseBeforeEffect,
        P::ReleaseIfReserved,
    ),
    edge(
        S::Accepted,
        S::Settled,
        T::SettleWithoutReservation,
        P::SettleOneOf(&SETTLE_TRANSFER),
    ),
    edge(
        S::Accepted,
        S::Settled,
        T::ApproveReversal,
        P::SettleOneOf(&SETTLE_REVERSAL),
    ),
    edge(S::Accepted, S::Reserved, T::PostReservation, P::Reserve),
    edge(S::Accepted, S::NeedsReview, T::DeadlinePassed, P::None),
    edge(
        S::Reserved,
        S::Settled,
        T::Capture,
        P::SettleOneOf(&SETTLE_CAPTURE),
    ),
    edge(
        S::Reserved,
        S::Released,
        T::ReleaseSafe,
        P::ReleaseIfReserved,
    ),
    edge(
        S::Reserved,
        S::Released,
        T::ProvedRejection,
        P::ReleaseIfReserved,
    ),
    edge(S::Reserved, S::Submitted, T::HandOffAcknowledged, P::None),
    edge(S::Reserved, S::NeedsReview, T::DeadlinePassed, P::None),
    edge(
        S::Reserved,
        S::NeedsReview,
        T::UnsafeReleaseRefused,
        P::None,
    ),
    edge(
        S::Submitted,
        S::Observed,
        T::ProvisionalObservation,
        P::None,
    ),
    edge(S::Submitted, S::Expired, T::ExpiryPassedUnobserved, P::None),
    edge(
        S::Submitted,
        S::Released,
        T::ProvedRejection,
        P::ReleaseIfReserved,
    ),
    edge(S::Submitted, S::NeedsReview, T::DeadlinePassed, P::None),
    edge(
        S::Submitted,
        S::NeedsReview,
        T::UnsafeReleaseRefused,
        P::None,
    ),
    edge(S::Observed, S::Final, T::FinalityAndCanonicalMatch, P::None),
    edge(S::Observed, S::Orphaned, T::CanonicalMismatch, P::None),
    edge(
        S::Observed,
        S::Released,
        T::ProvedRejection,
        P::ReleaseIfReserved,
    ),
    edge(S::Observed, S::NeedsReview, T::DeadlinePassed, P::None),
    edge(
        S::Observed,
        S::NeedsReview,
        T::UnsafeReleaseRefused,
        P::None,
    ),
    edge(S::Orphaned, S::Submitted, T::ReincludableAgain, P::None),
    edge(
        S::Orphaned,
        S::Released,
        T::InboundWithdrawn,
        P::ReleaseIfReserved,
    ),
    edge(
        S::Orphaned,
        S::Released,
        T::ProvedRejection,
        P::ReleaseIfReserved,
    ),
    edge(S::Orphaned, S::NeedsReview, T::DeadlinePassed, P::None),
    edge(
        S::Orphaned,
        S::NeedsReview,
        T::UnsafeReleaseRefused,
        P::None,
    ),
    edge(
        S::Final,
        S::Settled,
        T::AppendSettling,
        P::SettleOneOf(&SETTLE_LEG),
    ),
    edge(S::Final, S::NeedsReview, T::DeadlinePassed, P::None),
    edge(
        S::Expired,
        S::Released,
        T::NonInclusionProven,
        P::ReleaseIfReserved,
    ),
    edge(
        S::Expired,
        S::Released,
        T::ProvedRejection,
        P::ReleaseIfReserved,
    ),
    edge(
        S::Expired,
        S::NeedsReview,
        T::NonInclusionUnprovable,
        P::None,
    ),
    edge(S::Expired, S::NeedsReview, T::DeadlinePassed, P::None),
    edge(S::Expired, S::NeedsReview, T::UnsafeReleaseRefused, P::None),
    edge(
        S::NeedsReview,
        S::Settled,
        T::OperatorResolve,
        P::SettleOneOf(&SETTLE_RESOLVED),
    ),
    edge(
        S::NeedsReview,
        S::Released,
        T::OperatorResolve,
        P::ReleaseIfReserved,
    ),
    edge(
        S::NeedsReview,
        S::Settled,
        T::ProvenOutcome,
        P::SettleOneOf(&SETTLE_LEG),
    ),
    edge(
        S::NeedsReview,
        S::Released,
        T::ProvenOutcome,
        P::ReleaseIfReserved,
    ),
];

/// Where an operation enters the lifecycle: a command at `Received`, an inbound arrival aledgent
/// did not originate at `Observed` (SPEC §5.6).
pub const ENTRY: [(State, Origin); 2] = [
    (State::Received, Origin::Command),
    (State::Observed, Origin::UnoriginatedInbound),
];

/// The first row for the pair `(from, to)`, if §5.6 has one. Every row of a pair appends the
/// same kind of posting, so any of them answers what the pair may append.
pub fn find(from: State, to: State) -> Option<&'static Edge> {
    TRANSITIONS.iter().find(|e| e.from == from && e.to == to)
}

/// The row `(from, to)` on `on`, if §5.6 has it.
pub fn row(from: State, to: State, on: Trigger) -> Option<&'static Edge> {
    TRANSITIONS
        .iter()
        .find(|e| e.from == from && e.to == to && e.on == on)
}

/// Whether §5.6 has the pair `(from, to)`.
pub fn in_table(from: State, to: State) -> bool {
    find(from, to).is_some()
}

/// The rows that leave `from` on `on`, in table order.
pub fn rows(from: State, on: Trigger) -> impl Iterator<Item = &'static Edge> {
    TRANSITIONS
        .iter()
        .filter(move |e| e.from == from && e.on == on)
}

/// Whether `(state, origin)` is a place an operation may enter the lifecycle.
pub fn is_entry(state: State, origin: Origin) -> bool {
    ENTRY.contains(&(state, origin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeSet;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// SPEC.md as it is in this checkout.
    const SPEC_MD: &str = include_str!("../../../../SPEC.md");

    /// The lines of §5.6.
    fn section_5_6() -> Vec<&'static str> {
        let mut lines = SPEC_MD.lines();
        lines
            .by_ref()
            .find(|l| l.starts_with("### 5.6 "))
            .expect("SPEC.md has §5.6");
        lines.take_while(|l| !l.starts_with("### ")).collect()
    }

    /// The backticked words of a table cell.
    fn ticked(cell: &str) -> Vec<String> {
        cell.split('`')
            .enumerate()
            .filter(|(i, _)| i % 2 == 1)
            .map(|(_, w)| String::from(w))
            .collect()
    }

    /// The rows of the markdown table whose header row starts with `header`.
    fn table(lines: &[&str], header: &str) -> Vec<Vec<String>> {
        let start = lines
            .iter()
            .position(|l| l.starts_with(header))
            .expect("the table header is in §5.6");
        lines[start + 2..]
            .iter()
            .take_while(|l| l.starts_with('|'))
            .map(|l| {
                l.trim_matches('|')
                    .split('|')
                    .map(|c| String::from(c.trim()))
                    .collect()
            })
            .collect()
    }

    fn states(cell: &str) -> Vec<State> {
        ticked(cell)
            .iter()
            .map(|w| State::from_name(w).unwrap_or_else(|| panic!("§5.6 names no state {w}")))
            .collect()
    }

    /// T-007-01: the (from, to) pairs of §5.6's transition table, "or" cells expanded, are exactly
    /// the pairs of `TRANSITIONS`.
    #[test]
    fn spec_sync_transition_pairs_equal_section_5_6() {
        let lines = section_5_6();
        let mut spec = BTreeSet::new();
        for row in table(&lines, "| From | To | When |") {
            for from in states(&row[0]) {
                for to in states(&row[1]) {
                    assert!(
                        spec.insert((from, to)),
                        "§5.6 lists {from:?} → {to:?} twice"
                    );
                }
            }
        }
        let ours: BTreeSet<(State, State)> = TRANSITIONS.iter().map(|e| (e.from, e.to)).collect();
        let rows: BTreeSet<(State, State, Trigger)> =
            TRANSITIONS.iter().map(|e| (e.from, e.to, e.on)).collect();
        assert_eq!(rows.len(), TRANSITIONS.len(), "TRANSITIONS repeats a row");
        assert_eq!(spec, ours);
        assert_eq!(ours.len(), 28);
    }

    /// Every row of a pair appends the same kind of posting, so the pair's book effect does not
    /// depend on which condition moved it.
    #[test]
    fn rows_of_one_pair_share_their_posting_rule() {
        for e in TRANSITIONS {
            let first = find(e.from, e.to).unwrap();
            assert_eq!(
                core::mem::discriminant(&first.posting),
                core::mem::discriminant(&e.posting),
                "{e:?}"
            );
            assert_eq!(row(e.from, e.to, e.on), Some(&e));
        }
    }

    /// The "States:" line of §5.6 lists exactly `State::ALL`, in order.
    #[test]
    fn spec_sync_states_equal_section_5_6() {
        let lines = section_5_6();
        let start = lines
            .iter()
            .position(|l| l.starts_with("States:"))
            .expect("§5.6 lists its states");
        let mut words = Vec::new();
        for l in &lines[start..] {
            words.extend(ticked(l));
            if l.contains("normative") {
                break;
            }
        }
        let named: Vec<State> = words.iter().filter_map(|w| State::from_name(w)).collect();
        assert_eq!(named, State::ALL.to_vec());
    }

    /// The book-effect table of §5.6 names, for each state, exactly the posting kinds of
    /// `State::book_effect`.
    #[test]
    fn spec_sync_book_effects_equal_section_5_6() {
        let lines = section_5_6();
        let mut seen = BTreeSet::new();
        for row in table(&lines, "| State | Posting appended |") {
            let kinds: BTreeSet<PostingKind> = ticked(&row[1])
                .iter()
                .filter_map(|w| {
                    PostingKind::ALL
                        .into_iter()
                        .find(|k| alloc::format!("{k:?}") == *w)
                })
                .collect();
            for s in states(&row[0]) {
                let ours: BTreeSet<PostingKind> = s.book_effect().kinds().iter().copied().collect();
                assert_eq!(kinds, ours, "book effect of {s:?}");
                seen.insert(s);
            }
        }
        assert_eq!(
            seen.len(),
            State::ALL.len(),
            "every state has a book effect"
        );
    }

    /// Terminal states are the three `REQ-OP-4` names. `Final` and `Orphaned` are not terminal
    /// (amendment 0006 item 5).
    #[test]
    fn terminal_states_are_the_three_of_req_op_4() {
        let terminal: Vec<State> = State::ALL.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(terminal, [State::Settled, State::Released, State::Rejected]);
        assert!(!State::Final.is_terminal() && !State::Orphaned.is_terminal());
    }

    /// Amendment 0006 item 5: every non-terminal state but `Needs-Review` has a deadline row to
    /// `Needs-Review`, and no terminal state has one.
    #[test]
    fn every_non_terminal_state_has_a_deadline_row_to_needs_review() {
        for s in State::ALL {
            let has = row(s, State::NeedsReview, Trigger::DeadlinePassed).is_some();
            assert_eq!(has, !s.is_terminal() && s != State::NeedsReview, "{s:?}");
        }
    }

    /// Each edge's posting rule is within the book effect of the state it enters.
    #[test]
    fn every_edge_appends_only_what_its_target_state_allows() {
        for e in TRANSITIONS {
            let allowed = e.to.book_effect().kinds();
            for k in e.posting.kinds() {
                assert!(allowed.contains(k), "{e:?} appends {k:?}");
            }
            assert_eq!(
                core::mem::discriminant(&e.posting),
                core::mem::discriminant(&e.to.book_effect()),
                "{e:?}"
            );
        }
    }

    /// `INV-21`: no row leaves a terminal state (amendment 0006 items 1 and 4 withdrew the one
    /// that did).
    #[test]
    fn inv_21_no_row_leaves_a_terminal_state() {
        assert!(TRANSITIONS.iter().all(|e| !e.from.is_terminal()));
    }

    /// The codes are distinct, and every enum's `ALL` lists each variant once. The `match`es in
    /// `name`, `code`, `is_terminal`, `reserves` and `has_external_leg` name every variant with no
    /// wildcard, so a new variant does not compile until it is handled there; `ALL` is generated
    /// from the same list as the enum, so it cannot miss one.
    #[test]
    fn all_lists_every_variant_once_and_codes_are_distinct() {
        fn distinct<T: Ord + Copy>(xs: impl Iterator<Item = T>) -> usize {
            xs.collect::<BTreeSet<T>>().len()
        }
        assert_eq!(distinct(State::ALL.into_iter()), 12);
        assert_eq!(distinct(State::ALL.into_iter().map(State::code)), 12);
        assert_eq!(distinct(State::ALL.into_iter().map(State::name)), 12);
        assert_eq!(distinct(Trigger::ALL.into_iter()), 22);
        assert_eq!(distinct(Trigger::ALL.into_iter().map(Trigger::code)), 22);
        assert_eq!(distinct(Origin::ALL.into_iter().map(Origin::code)), 2);
        assert_eq!(distinct(KindClass::ALL.into_iter().map(KindClass::code)), 5);
        // Withdrawn codes are never reused (amendment 0006): `Reversed` and `ApprovedReversal`.
        assert!(State::ALL.into_iter().all(|s| s.code() != 13));
        assert!(Trigger::ALL.into_iter().all(|t| t.code() != 19));
        // Every trigger drives at least one row, and every row's trigger is its cell's.
        for t in Trigger::ALL {
            assert!(TRANSITIONS.iter().any(|e| e.on == t), "{t:?} drives no row");
        }
    }
}
