// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Read reach and the reads built so far (`F-003` §8, `F-004` §6, `INV-16`, `REQ-PV-2`). `F-011`
//! builds the query surface on [`Ledger::reach_account`] and [`Ledger::balances`].

use alloc::collections::BTreeSet;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::Ledger;
use crate::account::{AccountKind, LedgerAccount, Time};
use crate::asset::AssetId;
use crate::authority::{AuthorityKind, Subject, effective};
use crate::eval;
use crate::id::{AccountId, IdentityId};
use crate::principal::{Caller, resolve};
use crate::refusal::Refusal;
use crate::reservation::BalanceView;

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// Which live authority lets the caller read `account` (`INV-16`). `F-011` builds every query
    /// on this.
    pub fn reach_account(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<AuthorityKind, Refusal> {
        let p = resolve(&self.state, caller)?;
        eval::reach_account(&self.state, &p, account, now)
    }

    /// `account(account)` (SPEC §8.2, amendment 0028): the account as the book holds it — owner,
    /// kind, custody, status, when it was opened — so a service can check whose an account is
    /// before it adopts it. Read by whoever reaches the account (`INV-16`: its owner, a live grant
    /// naming it), by the operator's policy authority and the identities principal (`REQ-ID-6`),
    /// and by a principal holding a live grant over the account's owner (amendment 0025), with an
    /// unscoped credential. Anyone else, an unknown account and the issuance counterparty:
    /// `ERR-UnknownAccount`. A read; nothing is recorded.
    pub fn account_view(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<LedgerAccount, Refusal> {
        let p = resolve(&self.state, caller)?;
        let acct = self
            .state
            .account(account)
            .filter(|a| a.kind() != AccountKind::Issuance)
            .ok_or(Refusal::UnknownAccount)?;
        let reaches = eval::reach_account(&self.state, &p, account, now).is_ok()
            || self.identities_principal(&p, now).is_ok()
            || (p.unscoped()
                && self
                    .state
                    .held(&p.identity, Subject::Identity(acct.owner()))
                    .into_iter()
                    .any(|g| effective(g, now, |id| self.state.grant(id)).is_ok()));
        if !reaches {
            return Err(Refusal::UnknownAccount);
        }
        Ok(acct.clone())
    }

    /// The accounts of `identity` the caller can read (`REQ-PV-2`).
    pub fn reach_identity(
        &self,
        caller: &Caller,
        identity: &IdentityId,
        now: Time,
    ) -> Result<BTreeSet<AccountId>, Refusal> {
        let p = resolve(&self.state, caller)?;
        eval::reach_identity(&self.state, &p, identity, now)
    }

    /// An account's available balance in `asset`, for a caller that reaches it (`INV-16`). Signed:
    /// a Treasury account may be below zero. The same number as [`Ledger::balances`]' `available`.
    pub fn available(
        &self,
        caller: &Caller,
        account: &AccountId,
        asset: &AssetId,
        now: Time,
    ) -> Result<i128, Refusal> {
        self.reach_account(caller, account, now)?;
        let acct = self.state.account(account).ok_or(Refusal::UnknownAccount)?;
        let asset = self.state.registry().get(asset)?;
        self.net(acct, asset).map_err(|_| Refusal::NotAuthorised)
    }

    /// An account's balances in `asset` — available, reserved, encumbered and total — from one
    /// committed state version, for a caller that reaches it (`REQ-BA-2`, `REQ-BA-3`, `REQ-BA-6`,
    /// `INV-16`). `available` is the number the kernel checks a spend against; `reserved` is the
    /// sum of the account's open reservations. `F-011` serves it per identity (`REQ-BA-5`).
    pub fn balances(
        &self,
        caller: &Caller,
        account: &AccountId,
        asset: &AssetId,
        now: Time,
    ) -> Result<BalanceView, Refusal> {
        self.reach_account(caller, account, now)?;
        self.balance_view(account, asset)
    }

    /// The signed sum of every balance in `asset` across the book (`INV-5`): zero whenever every
    /// posting balances. Every kernel account is credit-normal, so this is the sum of the nets of
    /// every ledger account and every holding position (`F-004`).
    pub fn trial_balance(&self, asset: &AssetId) -> Result<i128, Refusal> {
        let asset = self.state.registry().get(asset)?;
        let mut sum: i128 = 0;
        for acct in self.state.all_accounts() {
            sum += self.net(acct, asset).map_err(|_| Refusal::NotAuthorised)?;
        }
        Ok(sum + self.held(asset)?)
    }
}
