// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The binding of a book-only asset, after its request (amendment 0011, `REQ-AS-8`, `DEC-20`,
//! `F-029`): the outstanding balance it records, the bind call in the call outbox, and its outcome.
//!
//! - **The amount** is the book's outstanding balance of the asset: everything the book holds in
//!   it outside the issuance counterparty and the opening positions, which is the negation of what
//!   those hold (`INV-5`). Opening balances count (question `k-03`, defaulted).
//! - **The call** is in the call outbox ([`crate::ledger::Ledger::call_outbox`]) from the request
//!   until the layer's record of its outcome is final, under the `bind_asset` command's operation
//!   as its reference. It has no expiry: its transaction is mortal, and the executor signs again
//!   one that provably did not execute; a second execution is refused by the layer as already
//!   bound, and changes nothing here.
//! - **The outcome** is decided by the canonical check that finds the layer's record final
//!   (`REQ-SL-3`): a `Bound` record of the asked amount binds the asset; the layer's refusal
//!   returns it to book-only and raises an incident with the record. Any other record under the
//!   reference (another amount, another asset) is an incident and decides nothing.

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::Ledger;
use crate::account::AccountKind;
use crate::asset::{Asset, AssetBinding, Minor};
use crate::id::AccountId;
use crate::refusal::Refusal;
use crate::settlement::assets::{AssetBind, BindStatus};
use crate::settlement::store::SettlementWrite;
use crate::settlement::{EventKind, Incident, Observation};
use crate::state::Write;

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// The book's outstanding balance of `asset`: the negation of everything the issuance
    /// counterparty and the opening positions hold in it (`INV-5`). A fault of the kernel is
    /// `ERR-Internal`, which the caller's refusal path raises as an incident; an outstanding balance below zero or beyond `u64` cannot be bound
    /// (`ERR-NotAuthorised`), and never occurs while `INV-5` holds.
    ///
    /// The issuance counterparty and the opening positions are the book's only `Issuance`
    /// accounts (`crate::state::State`'s rules), so they are read by key: the roles, then the
    /// positions, one per registered asset. Before, every ledger account of the book was read to
    /// find them (finding F18's class, `F-032`).
    pub(super) fn outstanding(&self, asset: &Asset) -> Result<Minor, Refusal> {
        let issuance = self.roles()?.issuance;
        let accounts: alloc::collections::BTreeSet<AccountId> = core::iter::once(issuance)
            .chain(self.state.openings().positions().map(|(_, a)| *a))
            .collect();
        let mut held: i128 = 0;
        for id in accounts {
            let a = self.state.account(&id).ok_or(Refusal::Internal)?;
            debug_assert_eq!(a.kind(), AccountKind::Issuance);
            match self.net(a, asset) {
                Ok(n) => held = held.checked_add(n).ok_or(Refusal::NotAuthorised)?,
                // The caller's refusal path counts it as its incident (`REQ-ER-2`).
                Err(_) => return Err(Refusal::Internal),
            }
        }
        let outstanding = held.checked_neg().ok_or(Refusal::NotAuthorised)?;
        u64::try_from(outstanding)
            .map(Minor::new)
            .map_err(|_| Refusal::NotAuthorised)
    }

    /// The bindings in the call outbox: every pending one, oldest request first. It reads every
    /// asset's binding: for the outbox, a query, never inside a recorded call.
    pub(super) fn pending_binds(&self) -> Vec<AssetBind> {
        let mut out: Vec<AssetBind> = self
            .state
            .settlement()
            .asset_binds()
            .filter(|b| b.status == BindStatus::Pending)
            .copied()
            .collect();
        out.sort_by_key(|b| (b.requested_at, b.asset));
        out
    }

    /// Whether an ingested record is one of a binding's: the canonical check calls
    /// [`Ledger::bind_final_writes`] for those.
    pub(super) fn is_bind_record(obs: &Observation) -> bool {
        matches!(obs.kind, EventKind::Bound | EventKind::BindRefused)
    }

    /// What a binding's record that the canonical check just found final means (`REQ-AS-8`,
    /// `REQ-SL-3`): the pending binding its reference names is bound, if the record is `Bound`
    /// with the binding's asset and amount; or returned to book-only, if the layer refused it.
    /// A record of a binding already decided changes nothing (a second execution, refused as
    /// already bound); any other record is an incident. Called in the check's commit.
    pub(super) fn bind_final_writes(&self, obs: &Observation) -> Vec<Write> {
        let coordinate = obs.coordinate;
        let incident = |asset| {
            vec![Write::Settlement(SettlementWrite::Incident(
                Incident::BindRefused { asset, coordinate },
            ))]
        };
        let Some(bind) = obs
            .operation_ref
            .and_then(|r| self.state.settlement().asset_bind_by_reference(&r))
            .copied()
        else {
            // A binding aledgent never asked for: recorded in the unassociated bucket only.
            return Vec::new();
        };
        if bind.status != BindStatus::Pending {
            return Vec::new();
        }
        match obs.kind {
            EventKind::Bound
                if obs
                    .movement
                    .is_some_and(|m| m.asset == bind.asset && m.amount == bind.amount) =>
            {
                vec![
                    Write::AssetBinding {
                        id: bind.asset,
                        binding: AssetBinding::Bound(bind.location),
                    },
                    Write::Settlement(SettlementWrite::AssetBindOutcome {
                        asset: bind.asset,
                        status: BindStatus::Bound,
                        coordinate,
                    }),
                ]
            }
            EventKind::BindRefused => {
                let mut w = vec![
                    Write::AssetBinding {
                        id: bind.asset,
                        binding: AssetBinding::BookOnly,
                    },
                    Write::Settlement(SettlementWrite::AssetBindOutcome {
                        asset: bind.asset,
                        status: BindStatus::Refused,
                        coordinate,
                    }),
                ];
                w.extend(incident(bind.asset));
                w
            }
            // A `Bound` record of another amount or asset under this binding's reference: the
            // layer and the book disagree. Nothing is decided; an operator looks.
            _ => incident(bind.asset),
        }
    }
}
