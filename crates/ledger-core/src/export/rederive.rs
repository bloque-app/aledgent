// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Re-derivation after a restore (`F-014` T-014-05, `AC-K5.1`, `INV-6`, `NFR-4`).
//!
//! Three checks, none needing an entry's own opening:
//!
//! 1. **Entries against balances.** For each kernel account and layer, the sum of the commitments
//!    of every entry against it equals the stored debit and credit commitments. Pedersen
//!    commitments add, so the check is exact (`INV-6`).
//! 2. **Openings against commitments.** Each stored plaintext balance, re-committed with its
//!    stored blinding, equals the stored commitment, which check 1 tied to the entries; and every
//!    commitment has its opening. So the openings store is checked against the entries too.
//! 3. **Acknowledged operations present.** Every operation reference the ingress acknowledged
//!    exists, in a state at least as far as the acknowledgement said (`NFR-4`). In process, the
//!    caller keeps the acknowledgement log.
//!
//! [`Ledger::rederive`] runs all three on a book as it stands, from its own delta log, its own
//! vault and its own service store, and adds the service store's own comparison of every
//! entry-derived balance with the stored one (`F-011`'s `reconcile`).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use cipher_clerk::{AccountId, Direction, TransferId};
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::traits::Identity;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::{RowKey, VaultBackend};

use super::kernel::{Leaves, OpeningFault, Openings};
use crate::Ledger;
use crate::id::OperationId;
use crate::lifecycle::State as OpState;
use crate::query::Incident;
use crate::state::State;

/// What the ingress told a caller about an operation (`NFR-4`): its reference, and the state its
/// reply showed, for an operation with a lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acknowledgement {
    /// The operation.
    pub op: OperationId,
    /// The state the acknowledgement showed; `None` for a command decided at once.
    pub state: Option<OpState>,
}

/// One thing re-derivation found wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finding {
    /// Check 1: a stored balance commitment is not the sum of its entries'.
    Balance {
        /// The kernel account.
        account: AccountId,
        /// The layer.
        layer: u8,
    },
    /// Check 1: an entry names an account the book does not hold, or its amount is not a valid
    /// commitment.
    Entry {
        /// The transfer.
        transfer: TransferId,
    },
    /// Check 1: the book holds a pending-transfer lifecycle, which no posting of aledgent makes
    /// (`DEC-2`), and which this check does not model.
    Unsupported,
    /// Check 2: an opening does not open its commitment, names an account the book does not hold,
    /// or is missing.
    Opening(RowKey),
    /// Check 3: an acknowledged operation is missing.
    Missing(OperationId),
    /// Check 3: an acknowledged operation is behind the state it was acknowledged in.
    Behind(OperationId),
    /// The service store's entry-derived balance differs from the stored one (`F-011`).
    Drift(Incident),
    /// The book could not be read.
    Unreadable,
}

/// Check 1: every stored balance against the sum of its entries.
pub fn check_entries(leaves: &Leaves) -> Vec<Finding> {
    let mut found = Vec::new();
    if !leaves.pending.is_empty() {
        found.push(Finding::Unsupported);
    }
    let zero = (RistrettoPoint::identity(), RistrettoPoint::identity());
    let mut sums: BTreeMap<(AccountId, usize), (RistrettoPoint, RistrettoPoint)> = BTreeMap::new();
    for t in leaves.transfers.values() {
        for e in &t.entries {
            let point = e.amount.to_point();
            let (true, Some(p)) = (leaves.accounts.contains_key(&e.account_id), point) else {
                found.push(Finding::Entry { transfer: t.id });
                continue;
            };
            let row = sums
                .entry((e.account_id, e.layer.as_index()))
                .or_insert(zero);
            match e.direction {
                Direction::Debit => row.0 += p,
                Direction::Credit => row.1 += p,
            }
        }
    }
    for (id, a) in &leaves.accounts {
        for (layer, pair) in a.balances.iter().enumerate() {
            let (dr, cr) = sums.get(&(*id, layer)).copied().unwrap_or(zero);
            if pair.dr.0 != dr.compress().to_bytes() || pair.cr.0 != cr.compress().to_bytes() {
                found.push(Finding::Balance {
                    account: *id,
                    layer: layer as u8,
                });
            }
        }
    }
    found
}

/// Check 2: the openings against the stored commitments.
pub fn check_openings(leaves: &Leaves, openings: &Openings) -> Vec<Finding> {
    match openings.check(leaves) {
        Ok(()) => Vec::new(),
        Err(OpeningFault::Unknown(k) | OpeningFault::DoesNotOpen(k) | OpeningFault::Missing(k)) => {
            alloc::vec![Finding::Opening(k)]
        }
    }
}

/// Check 3: every acknowledged operation is present, at least as far as it was acknowledged.
pub fn check_acknowledged(state: &State, acks: &[Acknowledgement]) -> Vec<Finding> {
    let mut found = Vec::new();
    for a in acks {
        let op = state.operations().get(&a.op);
        if op.is_none() && state.record(&a.op).is_none() {
            found.push(Finding::Missing(a.op));
            continue;
        }
        let Some(want) = a.state else {
            continue;
        };
        // States only move forward: the operation has been in `want` if it is there now or a
        // transition left or entered it.
        let reached = op.is_some_and(|o| {
            o.state() == want
                || o.transitions()
                    .iter()
                    .any(|t| t.from == want || t.to == want)
        });
        if !reached {
            found.push(Finding::Behind(a.op));
        }
    }
    found
}

/// All three checks over leaves, openings and a service store.
pub fn rederive(
    leaves: &Leaves,
    openings: &Openings,
    state: &State,
    acks: &[Acknowledgement],
) -> Result<(), Vec<Finding>> {
    let mut found = check_entries(leaves);
    found.extend(check_openings(leaves, openings));
    found.extend(check_acknowledged(state, acks));
    if found.is_empty() { Ok(()) } else { Err(found) }
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// Re-derive every balance of this book from its entries (module documentation), from its own
    /// delta log, vault and service store. `acks` is the acknowledgement log.
    pub fn rederive(&self, acks: &[Acknowledgement]) -> Result<(), Vec<Finding>> {
        let store = self.book().store();
        let leaves = store
            .last_committed()
            .ok()
            .flatten()
            .and_then(|last| Leaves::from_log(store, last).ok())
            .ok_or_else(|| alloc::vec![Finding::Unreadable])?;
        if store.root().ok() != Some(leaves.root()) {
            return Err(alloc::vec![Finding::Unreadable]);
        }
        let openings = Openings::from_vault(self.book().vault());
        let mut found = match rederive(&leaves, &openings, self.state(), acks) {
            Ok(()) => Vec::new(),
            Err(f) => f,
        };
        match self.reconcile() {
            Ok(drifts) => found.extend(drifts.into_iter().map(Finding::Drift)),
            Err(_) => found.push(Finding::Unreadable),
        }
        if found.is_empty() { Ok(()) } else { Err(found) }
    }
}
