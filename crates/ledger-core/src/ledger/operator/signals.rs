// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core's operational signals (`F-018` T-018-04, T-018-07, `NFR-11`, `REQ-OP-8`, `REQ-OP-6`,
//! `REQ-PR-5`, `AC-C4.1`, `AC-C4.2`).
//!
//! What `F-018`'s exporter scrapes from the core, through ingress, as an operator principal
//! holding the `ReadSignals` operator kind: counts, ages and times, **never a ledger fact** — no
//! account, amount, balance, identity or operation reference leaves here. The read changes
//! nothing, and needs no justification: it is not a privileged read (`REQ-PV-4`), since it
//! bypasses no authority check over any account; ingress's access log attributes it to its
//! principal and request (`REQ-PV-5`).

use alloc::collections::BTreeMap;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::super::Ledger;
use super::operator_authority_in;
use crate::account::Time;
use crate::anchor::{AnchorIncident, CheckpointStatus};
#[cfg(target_has_atomic = "8")]
use crate::leaves::kind;
use crate::ledger::lifecycle::ReviewReason;
use crate::lifecycle::State;
use crate::operator::{IncidentKind, OperatorOpKind};
use crate::principal::{Caller, resolve_unsealed};
use crate::refusal::Refusal;

/// The core's signals at one time. See the module documentation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signals {
    /// Non-terminal operations.
    pub open: u64,
    /// The age of the oldest non-terminal operation (`REQ-OP-8`, `AC-C4.2`); `None` when nothing
    /// is open.
    pub oldest_open_age: Option<Time>,
    /// Operations in `Received`: the ones the decision bound of `REQ-CS-7` covers.
    pub received: u64,
    /// The review queue's length (`REQ-OP-6`, `AC-C4.1`): `Needs-Review`, past deadline, flagged.
    pub review: u64,
    /// Of which in `Needs-Review`.
    pub needs_review: u64,
    /// Of which past their service level.
    pub review_overdue: u64,
    /// How far past its service level the most overdue entry is; zero when none is.
    pub review_most_overdue: Time,
    /// The review queue's service level, the deployment's.
    pub review_service_level: Time,
    /// The time of the last tick the core applied (`DEC-6`, `RISK-7`).
    pub last_tick: Option<Time>,
    /// Incidents raised on the operator channel, per kind, and when the latest was raised.
    pub incidents: BTreeMap<&'static str, (u64, Time)>,
    /// Anchor lag (`NFR-11`): time since the newest `Final` checkpoint was created; before the
    /// first is final, the age of the oldest checkpoint; `None` while there is none.
    pub anchor_lag: Option<Time>,
    /// How long the checkpoint in flight has waited for finality, if one is.
    pub anchor_in_flight_age: Option<Time>,
    /// Anchoring's incidents (`F-017`), per kind: a commitment mismatch and an unknown anchor stop
    /// or question anchoring until an operator acts.
    pub anchor_incidents: BTreeMap<&'static str, u64>,
    /// Assets whose latest settlement comparison differs, by as little as one minor unit
    /// (`REQ-PR-5`). A count: no figure.
    pub settlement_differing: u64,
    /// The age of the latest settlement comparison, if one was made.
    pub settlement_comparison_age: Option<Time>,
    /// A seeded book's progress (`F-036`): chunks applied, of how many, and whether the seed is
    /// sealed. `None` for a book founded without a seed.
    pub seeding: Option<SpecSignal>,
    /// The book's size (`T-018-11`): what `T-036-17`'s disk-backed store is required by.
    pub book: BookSize,
}

/// The book's size, counted by the collections that hold it, never by visiting a record
/// (`T-018-11`, the trigger of `T-036-17`, ruling `a0022-q5`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BookSize {
    /// The book's records, in a chainspec's unit (`F-036`): one for each identity, ledger
    /// account, grant (authority, issuance and operator-surface), alias and operation (`F-007`)
    /// the book holds, and one for each of the seed's opening rows and credential imports, which
    /// have no collection of their own. A seeded book holds its chainspec's records and the rows
    /// genesis itself writes (the operator's identity, the issuance counterparty, an opening
    /// position per asset); every later command adds the identity, account, grant, alias or
    /// operation it writes.
    pub records: u64,
    /// The leaves of the service store, of every kind: what the core host's replica holds in
    /// memory (`F-036` `NOTES.md` §11) and `T-036-17` moves to disk.
    pub leaves: u64,
    /// The records of the chainspec the book was founded on, once its header is applied; `None`
    /// for a book founded without one.
    pub genesis_records: Option<u64>,
}

/// The leaf kinds of which each leaf is one of the book's records ([`BookSize::records`]).
#[cfg(target_has_atomic = "8")]
const RECORD_KINDS: [u8; 6] = [
    kind::IDENTITY,
    kind::LEDGER_ACCOUNT,
    kind::GRANT,
    kind::OPERATOR_GRANT,
    kind::SPEC_ALIAS,
    kind::OPERATION,
];

/// A seeded genesis's progress, as the signals carry it: counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpecSignal {
    /// Chunks applied, the header included.
    pub applied: u32,
    /// Chunks the seed has.
    pub chunks: u32,
    /// Whether the last chunk sealed it.
    pub sealed: bool,
}

/// The name of an anchoring incident's kind, as the exporter labels it.
pub const fn anchor_incident_name(i: &AnchorIncident) -> &'static str {
    match i {
        AnchorIncident::CommitmentMismatch { .. } => "commitment_mismatch",
        AnchorIncident::UnknownAnchor { .. } => "unknown_anchor",
        AnchorIncident::PositionDifference { .. } => "position_difference",
    }
}

/// The name of an incident kind, as the exporter labels it.
pub const fn incident_name(k: IncidentKind) -> &'static str {
    match k {
        IncidentKind::ReversalProposed => "reversal_proposed",
        IncidentKind::AuditGap => "audit_gap",
        IncidentKind::ResolvedWithoutProof => "resolved_without_proof",
    }
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// The core's signals at `now` (module documentation), as `caller`: an unscoped operator
    /// principal with a live `ReadSignals` operator grant; anyone else is refused.
    pub fn signals(&self, caller: &Caller, now: Time) -> Result<Signals, Refusal> {
        // The signals read through the seal's guard: a seeded book shows its progress (`F-036`).
        let p = resolve_unsealed(&self.state, caller)?;
        operator_authority_in(&self.state, &p, OperatorOpKind::ReadSignals, now)?;
        let ops = self.state.operations();
        let seeding = self.state.chainspec().progress().map(|p| SpecSignal {
            applied: p.applied,
            chunks: p.commitment.chunks,
            sealed: p.sealed,
        });
        let mut s = Signals {
            seeding,
            book: self.book_size(),
            oldest_open_age: self.oldest_open_age(now),
            review_service_level: self.config.review_service_level,
            last_tick: ops.clock(),
            ..Signals::default()
        };
        for o in ops.open() {
            s.open += 1;
            if o.state() == State::Received {
                s.received += 1;
            }
        }
        for e in self.review_queue(now) {
            s.review += 1;
            if matches!(e.reason, ReviewReason::NeedsReview) {
                s.needs_review += 1;
            }
            if e.resolve_by < now {
                s.review_overdue += 1;
                s.review_most_overdue = s.review_most_overdue.max(now - e.resolve_by);
            }
        }
        for i in self.state.ops().incidents() {
            let e = s.incidents.entry(incident_name(i.kind)).or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(i.at);
        }
        // `ERR-Internal`'s incidents (`REQ-ER-2`): counted by the process, not stored.
        let (faults, latest) = self.internal_faults;
        if faults > 0 {
            s.incidents.insert("internal_fault", (faults, latest));
        }
        let anchoring = self.state.anchoring();
        let age = |at: Time| now.saturating_sub(at);
        s.anchor_lag = match anchoring
            .checkpoints()
            .rev()
            .find(|c| c.status == CheckpointStatus::Final)
        {
            Some(c) => Some(age(c.created_at)),
            None => anchoring.checkpoints().next().map(|c| age(c.created_at)),
        };
        s.anchor_in_flight_age = anchoring.in_flight().map(|c| age(c.created_at));
        for i in anchoring.incidents() {
            *s.anchor_incidents
                .entry(anchor_incident_name(i))
                .or_insert(0) += 1;
        }
        if let Some(c) = anchoring.latest_comparison() {
            s.settlement_differing = c.rows.iter().filter(|r| r.difference() != 0).count() as u64;
            s.settlement_comparison_age = Some(age(c.at));
        }
        Ok(s)
    }

    /// The book's size ([`BookSize`]): a few collections' counts, whatever the book's size. The
    /// host's alone: the core's guest answers no signals, and keeps its code unchanged.
    #[cfg(not(target_has_atomic = "8"))]
    fn book_size(&self) -> BookSize {
        BookSize::default()
    }

    /// The book's size ([`BookSize`]): a few collections' counts, whatever the book's size.
    #[cfg(target_has_atomic = "8")]
    fn book_size(&self) -> BookSize {
        let state = &self.state;
        let progress = state.chainspec().progress();
        // The seed's records that are no collection's leaves: its opening rows, posted at the
        // seal, and its credential imports, which join an identity's set.
        let unkept = progress.map_or(0, |p| p.seen.openings + p.seen.credentials);
        BookSize {
            records: RECORD_KINDS
                .iter()
                .map(|k| state.leaf_count_of(*k))
                .sum::<u64>()
                + unkept,
            leaves: state.leaf_count(),
            genesis_records: progress
                .and_then(|p| p.header.as_ref())
                .map(|h| h.counts.total()),
        }
    }
}
