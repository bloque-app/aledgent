// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! [`UnitBuilder`]: one all-or-nothing piece of an apply unit, as reservation postings are added
//! to it (`F-004` §3).
//!
//! It collects three things: the kernel accounts a posting needs created first (a holding
//! position under Option A), one **chain** of postings, and the [`IndexChange`]s that become true
//! if, and only if, that chain applies. A chain is all-or-nothing in the kernel (`REQ-KER-10`),
//! so a builder is too: its changes are committed exactly when its postings are.
//!
//! `F-007` and `F-010` put more than one posting in a builder (a capture and a transfer, a
//! settlement and a fee); the builder answers [`UnitBuilder::lookup`] with its own changes
//! applied, so a later posting sees the reservation an earlier one opened or closed.
//!
//! **One operation per unit.** Two builders applied as two steps of one unit are each built
//! against the index as committed before the unit, and neither knows whether the other will apply.
//! A composer must therefore not put the same operation in two builders of one unit: the second
//! could create a holding the first already created, which the kernel refuses as a binding fault
//! and which discards the whole unit. Put both in one builder (one chain) instead.

use alloc::vec::Vec;

use cipher_clerk::AccountId as KernelAccountId;
use ledger_kernel::account::KernelAccountSpec;
use ledger_kernel::book::Step;
use ledger_kernel::posting::PostingDraft;

use super::index::{IndexChange, Reservation, ReservationIndex, Status};
use crate::id::OperationId;

/// Where a reservation stands as a builder sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// No reservation has this operation reference.
    Unknown,
    /// Open.
    Open(Reservation),
    /// Closed: captured, released or settled.
    Closed(Reservation),
}

/// A step under construction, and the index changes that commit with it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitBuilder {
    creations: Vec<KernelAccountSpec>,
    chain: Vec<PostingDraft>,
    changes: Vec<IndexChange>,
}

/// A finished builder: the step to apply, and the index changes to commit if it applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    /// Creations, then one chain.
    pub step: Step,
    /// True exactly when the step applies.
    pub changes: Vec<IndexChange>,
}

/// Why a builder could not be finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishError<E> {
    /// A kernel account the builder must create already exists: its operation has been used.
    Exists(KernelAccountSpec),
    /// Reading the kernel failed.
    Kernel(E),
}

impl UnitBuilder {
    /// An empty builder.
    pub fn new() -> UnitBuilder {
        UnitBuilder::default()
    }

    /// Whether nothing has been added.
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty() && self.creations.is_empty() && self.changes.is_empty()
    }

    /// The postings, in chain order.
    pub fn chain(&self) -> &[PostingDraft] {
        &self.chain
    }

    /// The index changes, in order.
    pub fn changes(&self) -> &[IndexChange] {
        &self.changes
    }

    /// The accounts this builder creates before its chain.
    pub fn creations(&self) -> &[KernelAccountSpec] {
        &self.creations
    }

    /// Reservation `op`, as this builder sees it: the index, then this builder's own changes.
    pub fn lookup(&self, index: &ReservationIndex, op: &OperationId) -> Lookup {
        let mut seen = match index.status(op) {
            None => Lookup::Unknown,
            Some(Status::Open(r)) => Lookup::Open(r),
            Some(Status::Closed(r, _)) => Lookup::Closed(r),
        };
        for c in &self.changes {
            match c {
                IndexChange::Open(r) if r.op == *op => seen = Lookup::Open(*r),
                IndexChange::Close { op: o, .. } if o == op => {
                    if let Lookup::Open(r) = seen {
                        seen = Lookup::Closed(r);
                    }
                }
                _ => {}
            }
        }
        seen
    }

    /// Create `spec` before the chain runs. It must not exist yet.
    pub(crate) fn create(&mut self, spec: KernelAccountSpec) {
        if !self.creations.contains(&spec) {
            self.creations.push(spec);
        }
    }

    /// Append a posting to the chain: a reservation posting, or any other balanced posting that
    /// must apply with it or not at all (a transfer, a fee; `F-007`, `F-008`, `F-010`).
    pub fn post(&mut self, draft: PostingDraft) {
        self.chain.push(draft);
    }

    /// Record an index change that commits with the chain.
    pub(crate) fn change(&mut self, change: IndexChange) {
        self.changes.push(change);
    }

    /// Finish the builder. `exists` reads whether a kernel account exists.
    ///
    /// Every other kernel account the chain touches and that does not exist yet is created in
    /// the same step, before the chain: a ledger account can be credited at once in an asset it
    /// never held (`INV-9`). If the chain is refused, the kernel drops those creations with it.
    /// An account the builder must create itself (a holding position) that already exists means
    /// its operation reference was used before: [`FinishError::Exists`], and nothing is applied.
    pub fn finish<E>(
        self,
        mut exists: impl FnMut(&KernelAccountId) -> Result<bool, E>,
    ) -> Result<Built, FinishError<E>> {
        let mut creations = self.creations;
        for spec in &creations {
            if exists(&spec.id()).map_err(FinishError::Kernel)? {
                return Err(FinishError::Exists(*spec));
            }
        }
        for draft in &self.chain {
            for e in draft.entries() {
                if creations.contains(&e.account) {
                    continue;
                }
                if !exists(&e.account.id()).map_err(FinishError::Kernel)? {
                    creations.push(e.account);
                }
            }
        }
        Ok(Built {
            step: Step {
                creations,
                chain: self.chain,
            },
            changes: self.changes,
        })
    }
}
