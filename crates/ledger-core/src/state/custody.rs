// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The service store's half of [`CustodyWrite`] (`F-012`): the writes that change an account —
//! its custody mode, its owner — under the store's rules, and the rest handed to
//! [`crate::custody::store::CustodyState`].

use super::{State, StoreRefusal, Undo};
use crate::account::{AccountKind, AccountStatus, Custody, LedgerAccount};
use crate::custody::store::{CustodyRefusal, CustodyWrite};

impl State {
    pub(super) fn apply_custody(&mut self, w: &CustodyWrite) -> Result<Undo, StoreRefusal> {
        match w {
            CustodyWrite::SetCustody { account, custody } => {
                let before = self
                    .accounts
                    .get(account)
                    .ok_or(StoreRefusal::Custody(CustodyRefusal::Dangling))?
                    .clone();
                if before.custody() == *custody
                    || before.kind() == AccountKind::Issuance
                    || before.status() == AccountStatus::Closed
                {
                    return Err(StoreRefusal::Custody(CustodyRefusal::NoChange));
                }
                let after = before.clone().with_custody(*custody);
                self.index_custody(&after);
                self.accounts.insert(*account, after);
                let epoch = self.custody.bump(account);
                Ok(Undo::CustodySwitch(before, epoch))
            }
            CustodyWrite::SetOwner { account, owner } => {
                let before = self
                    .accounts
                    .get(account)
                    .ok_or(StoreRefusal::Custody(CustodyRefusal::Dangling))?
                    .clone();
                if !self.identities.contains_key(owner) {
                    return Err(StoreRefusal::Custody(CustodyRefusal::Dangling));
                }
                if before.owner() == *owner
                    || before.kind() == AccountKind::Issuance
                    || before.status() == AccountStatus::Closed
                {
                    return Err(StoreRefusal::Custody(CustodyRefusal::NoChange));
                }
                self.by_owner.remove_from(&before.owner(), account);
                self.by_owner.entry_or_default(*owner).insert(*account);
                let after = before.clone().with_owner(*owner);
                if before.custody() == Custody::Owned {
                    self.owned_custody.remove_from(&before.owner(), account);
                    self.index_custody(&after);
                }
                self.accounts.insert(*account, after);
                Ok(Undo::Owner(before))
            }
            other => {
                let accounts = &self.accounts;
                let identities = &self.identities;
                self.custody
                    .apply(
                        other,
                        |a| accounts.get(a).map(LedgerAccount::owner),
                        |i| identities.contains_key(i),
                    )
                    .map(Undo::Custody)
                    .map_err(StoreRefusal::Custody)
            }
        }
    }

    /// Undo a [`CustodyWrite::SetOwner`]: the account and both owners' indexes.
    pub(super) fn restore_owner(&mut self, before: LedgerAccount) {
        let id = before.id();
        if let Some(now) = self.accounts.get(&id) {
            let current = now.owner();
            self.by_owner.remove_from(&current, &id);
            if now.custody() == Custody::Owned {
                self.owned_custody.remove_from(&current, &id);
            }
        }
        self.by_owner.entry_or_default(before.owner()).insert(id);
        if before.custody() == Custody::Owned {
            self.index_custody(&before);
        }
        self.accounts.insert(id, before);
    }
}
