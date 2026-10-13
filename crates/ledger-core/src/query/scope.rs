// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The authorised read scope (`F-011` §2, `INV-16`, `REQ-PV-1`, `REQ-PV-2`).
//!
//! A query first asks `F-003`'s reach function which of the accounts it would read the calling
//! principal holds live authority over, and with which kind of authority. The answer is an
//! [`AuthorisedScope`]. Every read method of the query store ([`super::QueryStore`]) takes the
//! scope as an argument and filters on it **inside the store**, so an account outside it is
//! `ERR-UnknownAccount` — the same answer as an account that does not exist — whatever the code
//! above the store does. Delegated reach is only as wide as the grant (`REQ-PV-2`).
//!
//! A scope is made only by `AuthorisedScope::reached`, from the evaluator's reach. The
//! conformance suite may build a narrowed one (`AuthorisedScope::narrowed`, feature
//! `conformance-ports`) to call the store directly, as the store case of `INV-16` does.

use alloc::collections::BTreeMap;

use crate::account::Time;
use crate::authority::AuthorityKind;
use crate::eval;
use crate::id::AccountId;
use crate::principal::Principal;
use crate::refusal::Refusal;
use crate::state::State;

/// The accounts a principal may read in one query, and the authority it reaches each with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorisedScope {
    accounts: BTreeMap<AccountId, AuthorityKind>,
}

impl AuthorisedScope {
    /// The scope of `p` over `candidates` at `now`: every candidate it reaches with live
    /// authority (`F-003`'s reach, `eval::reach_account`). The others are left out.
    pub(crate) fn reached<'a>(
        state: &State,
        p: &Principal,
        candidates: impl IntoIterator<Item = &'a AccountId>,
        now: Time,
    ) -> AuthorisedScope {
        let accounts = candidates
            .into_iter()
            .filter_map(|a| {
                eval::reach_account(state, p, a, now)
                    .ok()
                    .map(|kind| (*a, kind))
            })
            .collect();
        AuthorisedScope { accounts }
    }

    /// A scope of exactly `accounts`, each reached as owner. For the conformance suite's store
    /// cases only (feature `conformance-ports`, `REQ-CS-5`): a production caller gets its scope
    /// from reach and nowhere else.
    #[cfg(any(test, feature = "conformance-ports"))]
    pub fn narrowed(accounts: impl IntoIterator<Item = AccountId>) -> AuthorisedScope {
        AuthorisedScope {
            accounts: accounts
                .into_iter()
                .map(|a| (a, AuthorityKind::Owner))
                .collect(),
        }
    }

    /// The authority `account` is reached with, or `ERR-UnknownAccount`.
    pub fn check(&self, account: &AccountId) -> Result<AuthorityKind, Refusal> {
        self.accounts
            .get(account)
            .copied()
            .ok_or(Refusal::UnknownAccount)
    }

    /// Whether `account` is in the scope.
    pub fn covers(&self, account: &AccountId) -> bool {
        self.accounts.contains_key(account)
    }

    /// The accounts in the scope, in order.
    pub fn accounts(&self) -> impl Iterator<Item = &AccountId> {
        self.accounts.keys()
    }

    /// Whether the scope reaches nothing.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }
}
