// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The read side of the service store, and the index contracts it serves (`F-011` §2, §4, §6, §7,
//! `DEC-4`, `INV-16`).
//!
//! [`QueryStore`] is the storage trait every query reads through. Each method that returns a fact
//! about an account takes the caller's [`AuthorisedScope`] and filters on it **inside the store**:
//! an account outside the scope is `ERR-UnknownAccount`, exactly as a missing one, so `INV-16`
//! holds at the layer SPEC §9 names whatever the code above does. [`State`] implements it in
//! memory for both in-process targets; `F-006` implements it over the side store.
//!
//! ## Index contracts for `F-006`
//!
//! Each method below states what it must serve and how fast. `N` is the number of postings in the
//! book (up to 150 million entries, `NFR-3`), `k` the number of postings returned or walked.
//!
//! | Method | Index | Cost |
//! |---|---|---|
//! | [`account_in`](QueryStore::account_in) | account by id | `O(log N)` |
//! | [`reserved_in`](QueryStore::reserved_in) | open reservations by (account, asset), summed | `O(log N)` with a maintained sum |
//! | [`postings_in`](QueryStore::postings_in) | `account_postings(account, from_seq, to_seq, limit)`, ordered by sequence | `O(log N + k)` |
//! | [`position_postings_in`](QueryStore::position_postings_in) | the same, per (account, asset) | `O(log N + k)` |
//! | [`balance_before_in`](QueryStore::balance_before_in) | a balance checkpoint every [`CHECKPOINT_EVERY`](super::journal::CHECKPOINT_EVERY) postings per (account, asset) | `O(log N + CHECKPOINT_EVERY)`, whatever the skew |
//! | [`balance_now_in`](QueryStore::balance_now_in) | the running entry-derived balance per (account, asset) | `O(log N)` |
//! | [`first_at_or_after`](QueryStore::first_at_or_after) | a sparse `(sequence, at)` index, `at` never decreasing along the sequence | `O(log N)` |
//!
//! Every read is as of one committed posting sequence (the view's, `REQ-QR-2`): postings at or
//! above it are never returned. No method writes (`REQ-QR-3`): every one takes `&self`.

use alloc::vec::Vec;

use super::journal::{PostingEntry, Sums};
use super::scope::AuthorisedScope;
use crate::account::{LedgerAccount, Time};
use crate::asset::{AssetId, Minor};
use crate::id::AccountId;
use crate::refusal::Refusal;
use crate::state::State;

/// The storage trait's read side. See the module documentation for the index contracts.
pub trait QueryStore {
    /// The account, if the scope reaches it; `ERR-UnknownAccount` otherwise, and for an account
    /// that does not exist.
    fn account_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
    ) -> Result<&LedgerAccount, Refusal>;

    /// The sum of the account's open reservations in `asset` (`REQ-BA-6`).
    fn reserved_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        asset: &AssetId,
    ) -> Result<Minor, Refusal>;

    /// The postings on `account` with `from <= seq < below`, in sequence order, at most `limit`
    /// (`REQ-QR-5`, `REQ-PO-2`).
    fn postings_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        from: u64,
        below: u64,
        limit: usize,
    ) -> Result<Vec<&PostingEntry>, Refusal>;

    /// The same, for one asset of the account.
    fn position_postings_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        asset: &AssetId,
        from: u64,
        below: u64,
        limit: usize,
    ) -> Result<Vec<&PostingEntry>, Refusal>;

    /// The account's balances in `asset` derived from every entry below `seq` (`INV-6`).
    fn balance_before_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        asset: &AssetId,
        seq: u64,
    ) -> Result<Sums, Refusal>;

    /// The account's balances in `asset` derived from every entry recorded (`INV-6`).
    fn balance_now_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        asset: &AssetId,
    ) -> Result<Sums, Refusal>;

    /// The first posting at or after `time`, below `below`: its sequence number, else `below`.
    /// A sequence number is not a fact about any account.
    fn first_at_or_after(&self, time: Time, below: u64) -> u64;

    /// The last posting recorded: its sequence number and time.
    fn last_posting(&self) -> Option<(u64, Time)>;
}

impl QueryStore for State {
    fn account_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
    ) -> Result<&LedgerAccount, Refusal> {
        scope.check(account)?;
        self.account(account).ok_or(Refusal::UnknownAccount)
    }

    fn reserved_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        asset: &AssetId,
    ) -> Result<Minor, Refusal> {
        self.account_in(scope, account)?;
        Ok(self.reservations().reserved(account, asset))
    }

    fn postings_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        from: u64,
        below: u64,
        limit: usize,
    ) -> Result<Vec<&PostingEntry>, Refusal> {
        self.account_in(scope, account)?;
        Ok(self.journal().account_postings(account, from, below, limit))
    }

    fn position_postings_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        asset: &AssetId,
        from: u64,
        below: u64,
        limit: usize,
    ) -> Result<Vec<&PostingEntry>, Refusal> {
        self.account_in(scope, account)?;
        Ok(self
            .journal()
            .position_postings(account, asset, from, below, limit))
    }

    fn balance_before_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        asset: &AssetId,
        seq: u64,
    ) -> Result<Sums, Refusal> {
        self.account_in(scope, account)?;
        Ok(self.journal().balance_before(account, asset, seq))
    }

    fn balance_now_in(
        &self,
        scope: &AuthorisedScope,
        account: &AccountId,
        asset: &AssetId,
    ) -> Result<Sums, Refusal> {
        self.account_in(scope, account)?;
        Ok(self.journal().balance_now(account, asset))
    }

    fn first_at_or_after(&self, time: Time, below: u64) -> u64 {
        self.journal().first_at_or_after(time, below)
    }

    fn last_posting(&self) -> Option<(u64, Time)> {
        self.journal().last()
    }
}
