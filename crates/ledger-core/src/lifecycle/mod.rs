// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The operation lifecycle (`F-007`, SPEC §5.6, `DEC-1`, `DEC-6`, `DEC-7`).
//!
//! Every operation has one record with one state. That state changes only along the table of
//! SPEC §5.6, and each change is idempotent. Terminal states absorb. Every open record carries a
//! deadline, and deadlines advance only from a time the core is given, never from a clock it reads.
//! Two idempotency keys exist and are never confused: the caller's client key and the service's
//! operation reference.
//!
//! ## Map
//!
//! - [`table`]: [`State`], [`Trigger`], [`Origin`], [`KindClass`], [`PostingRule`], [`Edge`],
//!   [`TRANSITIONS`] and [`ENTRY`]: the table as data, checked against SPEC §5.6 by a test that
//!   reads the spec.
//! - [`next`](mod@next): [`next()`], the one function that moves state; [`Facts`], [`Decision`];
//!   [`Lifecycle`], which cannot be open without a deadline or closed while not terminal;
//!   [`StateClass`] and [`Shown`], how a state reads to a caller (`REQ-OP-7`); and
//!   [`past_deadline`], the one place that says what a passed deadline does in each state.
//! - [`keys`]: client keys, operation references, request digests, transition keys and
//!   notification identities (`REQ-OP-10`, `REQ-OP-11`, `REQ-OP-3`, `REQ-EV-3`).
//! - [`store`]: the operation store's records and rules, part of the service store
//!   ([`crate::state::State`]), behind [`crate::state::OperationStore`] (`INV-15`, `INV-18`,
//!   `INV-21`).
//! - [`LifecycleConfig`]: the deployment's deadlines and review routing.
//!
//! The engine that drives transitions with their postings, `tick`, the review queue, `resume` and
//! the commands' idempotency are methods of [`crate::Ledger`] (`ledger/lifecycle.rs`), because a
//! transition commits with the book.
//!
//! ## Amendment 0006
//!
//! Ruled on 2026-09-29 and followed here: no `Reversed` state and nothing leaves a terminal state
//! (items 1 and 4, `INV-21` with no exception); a reversal is an operation of its own
//! ([`KindClass::Reversal`], settled by approval); a proved rejection releases at once (item 3);
//! every non-terminal state moves to `Needs-Review` past its deadline (item 5, [`past_deadline`]);
//! and a release requested without proof moves to `Needs-Review`, releasing nothing (item 11).
//! `features/F-007-operation-lifecycle/NOTES.md` records each.

pub mod keys;
pub mod next;
pub mod store;
pub mod table;

#[cfg(test)]
mod model;

use alloc::collections::BTreeMap;

pub use next::{
    Decision, Facts, Lifecycle, PastDeadline, Reason, Resolution, Shown, StateClass, next,
    past_deadline,
};
pub use table::{ENTRY, Edge, KindClass, Origin, PostingRule, State, TRANSITIONS, Trigger};

use crate::account::Time;
use crate::authority::OperationKind;
use crate::custody::RecoveryConfig;

/// The deployment's lifecycle configuration (`F-007` §8).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LifecycleConfig {
    /// The deployment's identity, which operation references are derived under (`F-007` §7).
    pub deployment: [u8; 16],
    /// The bound within which a `Received` command must be decided (`REQ-CS-7`). It is
    /// `Received`'s deadline: past it the operation moves to `Needs-Review`, and it is an incident
    /// too (amendment 0006 item 5).
    pub decision_bound: Time,
    /// The deadline, as a duration from acceptance, of an operation of each kind whose command
    /// carries none. A kind with no entry and a command with no deadline is
    /// `ERR-DeadlineRequired`. Accepted proposal of `F-007` §8.
    pub default_deadlines: BTreeMap<OperationKind, Time>,
    /// The service level of the review queue: an operation entering `Needs-Review` must be
    /// resolved within this long, which is its deadline there (`REQ-OP-6`, `AC-C4.1`).
    pub review_service_level: Time,
    /// The most deadline transitions one `tick` makes. The rest wait for the next tick, so one
    /// invocation stays within its budget (`F-002`): [`DEFAULT_MAX_FIRES_PER_TICK`] by default.
    pub max_fires_per_tick: usize,
    /// The declared maximum number of legs in one transfer submission (`F-008` §9, `AC-B3.3`,
    /// `OQ-24`): a larger one is `ERR-BatchTooLarge`, before anything is looked up. Part of the
    /// deployment's declaration of limits, read at start-up.
    pub max_legs: usize,
    /// The deployment's recovery configuration (`F-012`, `REQ-AU-9`, `DEC-13`): whether owned
    /// custody is offered, the recovery authority's public key, and the delays. Checked at
    /// genesis; a deployment that fails the check does not start. [`LifecycleConfig::new`]
    /// offers delegated custody only.
    pub recovery: RecoveryConfig,
    /// The independent sources an opening load must reconcile against, per asset (`F-014`,
    /// `REQ-RC-4`, `NFR-9`). An asset with none cannot be loaded.
    pub opening_sources: crate::opening::RequiredSources,
    /// Anchoring: the interval between checkpoints and the assurance declaration disclosures
    /// carry (`F-017`, `REQ-PR-1`, `REQ-AR-5`).
    pub anchoring: crate::anchor::AnchoringConfig,
}

/// The default of [`LifecycleConfig::max_fires_per_tick`]: what one invocation of the core Agent
/// holds (finding F18 of `F-032`). Measured natively (`core-binding`'s `tests/heap.rs`), a tick
/// firing 8 deadlines peaks at about 96 KiB of the Agent's heap, 16 at about 194 KiB, and 256,
/// the default before, at about 1.5 MiB; the guest's heap is 256 KiB, of which the tests keep a
/// quarter free, and a tick may travel in a batch. The rest wait for the next tick.
pub const DEFAULT_MAX_FIRES_PER_TICK: usize = 8;

/// The default of [`LifecycleConfig::max_legs`]. `TODO(F-002)`: replace it with the measured
/// chain length that fits one invocation's budget. Research 07 §6 measured a chain of ten at
/// about 12.7 ms on a dev profile, which is not a contract (`F-008` §9).
pub const DEFAULT_MAX_LEGS: usize = 10;

impl LifecycleConfig {
    /// A configuration with no default deadline for any kind: every command must carry one.
    pub fn new(deployment: [u8; 16], decision_bound: Time, review_service_level: Time) -> Self {
        LifecycleConfig {
            deployment,
            decision_bound,
            default_deadlines: BTreeMap::new(),
            review_service_level,
            max_fires_per_tick: DEFAULT_MAX_FIRES_PER_TICK,
            max_legs: DEFAULT_MAX_LEGS,
            recovery: RecoveryConfig::not_offered(),
            opening_sources: BTreeMap::new(),
            anchoring: crate::anchor::AnchoringConfig::default(),
        }
    }

    /// The same, with `anchoring` as the deployment's anchoring configuration (`F-017`).
    pub fn with_anchoring(mut self, anchoring: crate::anchor::AnchoringConfig) -> Self {
        self.anchoring = anchoring;
        self
    }

    /// The same, with `recovery` as the deployment's recovery configuration (`F-012`).
    pub fn with_recovery(mut self, recovery: RecoveryConfig) -> Self {
        self.recovery = recovery;
        self
    }

    /// The same, requiring `sources` to agree on every opening balance in `asset`.
    pub fn with_opening_sources(
        mut self,
        asset: crate::asset::AssetId,
        sources: crate::set::NonEmptySet<crate::opening::OpeningSource>,
    ) -> Self {
        self.opening_sources.insert(asset, sources);
        self
    }

    /// The same, with at most `n` legs in one transfer submission.
    pub fn with_max_legs(mut self, n: usize) -> Self {
        self.max_legs = n;
        self
    }

    /// The same, with a default deadline for operations of `kind`.
    pub fn with_default_deadline(mut self, kind: OperationKind, after: Time) -> Self {
        self.default_deadlines.insert(kind, after);
        self
    }

    /// The same, with at most `n` deadline transitions per tick.
    pub fn with_max_fires_per_tick(mut self, n: usize) -> Self {
        self.max_fires_per_tick = n;
        self
    }
}
