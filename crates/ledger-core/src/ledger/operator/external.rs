// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Reconciliation against an external source (`F-018` T-018-06, `NFR-9` axis 1(c), `DEC-14`).
//!
//! During shadow and cutover the book's balances are compared with the legacy book's. That needs
//! plaintext balances, so the comparison runs **inside the core** as a privileged read
//! (`REQ-PV-4`): the operator needs the `PrivilegedRead` operator kind and a justification, and
//! the read's record reaches the audit store before anything is compared. It returns **only the
//! mismatches** — which (account, asset, balance kind) differ, and which pairs one side lacks —
//! never a figure: the operator learns where to look, not the book's balances. It changes nothing
//! (`REQ-AT-3`).
//!
//! The source itself is a plug-in ([`ExternalSource`]): the legacy book's extract is `DEC-14`
//! Phase 1 work, outside `F-018`.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::super::{Ctx, Ledger};
use crate::asset::AssetId;
use crate::audit::{AuditSink, ReadKind};
use crate::id::AccountId;
use crate::refusal::Refusal;

/// A balance kind an external source states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExternalKind {
    /// `available`.
    Available,
    /// `reserved`.
    Reserved,
}

/// An independent source of balances (`NFR-9` axis 1): during shadow and cutover, the legacy
/// book (`DEC-14`).
pub trait ExternalSource {
    /// Every (account, asset) the source holds a balance for.
    fn pairs(&self) -> Vec<(AccountId, AssetId)>;
    /// The source's figure for `kind` of (account, asset), in exact minor units; `None` if it
    /// holds none.
    fn figure(&self, account: &AccountId, asset: &AssetId, kind: ExternalKind) -> Option<i128>;
}

/// Why a pair does not reconcile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MismatchReason {
    /// Both hold the pair, and the figures differ.
    Differs(ExternalKind),
    /// The source holds a pair the book does not.
    MissingInBook,
    /// The book holds a pair, with a balance, the source does not.
    MissingInSource,
}

/// One pair that does not reconcile. No figure: see the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExternalMismatch {
    /// The account.
    pub account: AccountId,
    /// The asset.
    pub asset: AssetId,
    /// Why.
    pub reason: MismatchReason,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// Compare every (account, asset) of the book with `source`, at one minor unit and zero
    /// tolerance, through the privileged-read mechanism (module documentation). Returns the
    /// mismatches only.
    pub fn reconcile_external(
        &mut self,
        ctx: &Ctx,
        source: &dyn ExternalSource,
        justification: &[u8],
        sink: &mut dyn AuditSink,
    ) -> Result<Vec<ExternalMismatch>, Refusal> {
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Vec<ExternalMismatch>, Refusal> = (|| {
            self.privileged_read(
                ctx,
                ReadKind::Reconciliation,
                Vec::new(),
                justification,
                sink,
            )?;
            let assets: Vec<AssetId> = self.state.registry().iter().map(|a| a.id()).collect();
            let accounts: Vec<AccountId> = self.state.all_accounts().map(|a| a.id()).collect();
            let theirs: BTreeSet<(AccountId, AssetId)> = source.pairs().into_iter().collect();
            let mut out = Vec::new();
            for account in &accounts {
                for asset in &assets {
                    let view = self.balance_view(account, asset)?;
                    let pair = (*account, *asset);
                    if !theirs.contains(&pair) {
                        if view.available != 0 || view.reserved != 0 {
                            out.push(ExternalMismatch {
                                account: *account,
                                asset: *asset,
                                reason: MismatchReason::MissingInSource,
                            });
                        }
                        continue;
                    }
                    for (kind, ours) in [
                        (ExternalKind::Available, view.available),
                        (ExternalKind::Reserved, view.reserved),
                    ] {
                        if source.figure(account, asset, kind).unwrap_or(0) != ours {
                            out.push(ExternalMismatch {
                                account: *account,
                                asset: *asset,
                                reason: MismatchReason::Differs(kind),
                            });
                        }
                    }
                }
            }
            for (account, asset) in theirs {
                if self.state.account(&account).is_none() || !assets.contains(&asset) {
                    out.push(ExternalMismatch {
                        account,
                        asset,
                        reason: MismatchReason::MissingInBook,
                    });
                }
            }
            Ok(out)
        })();
        r
    }
}
