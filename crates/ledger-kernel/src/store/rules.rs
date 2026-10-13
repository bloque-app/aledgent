// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The write rules every store enforces before it writes (`INV-2`, `INV-25`,
//! `REQ-KER-5`, `REQ-KER-6`, `REQ-PO-2`). One implementation, shared by all
//! stores, so the rules cannot drift between them.

use alloc::collections::{BTreeMap, BTreeSet};

use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::external_id_key;
use cipher_clerk::{Account, AccountFlags, AccountId, TransferFlags, TransferId};

use super::{BookDelta, StoreError, WriteRefusal};

/// What a store must be able to answer for the rules to run.
pub trait RuleView {
    /// The commit sequence number the store expects next.
    fn expected_seq(&self) -> Result<u64, StoreError>;
    /// The posting sequence number the store expects next.
    fn expected_posting_seq(&self) -> Result<u64, StoreError>;
    /// The stored account, if any.
    fn stored_account(&self, id: &AccountId) -> Result<Option<Account>, StoreError>;
    /// Whether the transfer is stored.
    fn transfer_stored(&self, id: &TransferId) -> Result<bool, StoreError>;
    /// Whether any journal is stored.
    fn journal_stored(&self) -> Result<bool, StoreError>;
    /// The full external id occupying a slot, if any.
    fn external_id_in_slot(&self, slot: &[u8; 16]) -> Result<Option<[u8; 32]>, StoreError>;
    /// Whether the transfer is marked voided.
    fn voided_stored(&self, id: &TransferId) -> Result<bool, StoreError>;
    /// The stored pending status, if any.
    fn pending_stored(&self, id: &TransferId) -> Result<Option<PendingStatus>, StoreError>;
}

/// Check `delta`, as commit `seq`, against the rules. Nothing is written.
pub fn check(view: &impl RuleView, seq: u64, delta: &BookDelta) -> Result<(), StoreError> {
    let expected = view.expected_seq()?;
    if seq != expected {
        return Err(WriteRefusal::SeqOutOfOrder { expected, got: seq }.into());
    }

    // Journals: exactly one, at bootstrap (INV-25).
    if !delta.journals.is_empty() {
        if view.journal_stored()? {
            return Err(WriteRefusal::JournalExists.into());
        }
        if delta.journals.len() > 1 {
            return Err(WriteRefusal::MoreThanOneJournal.into());
        }
    }

    // Accounts: insert, then roll balances only (INV-2).
    for (id, new) in &delta.accounts {
        if let Some(old) = view.stored_account(id)?
            && !is_roll_of(&old, new)
        {
            return Err(WriteRefusal::AccountRewritten(*id).into());
        }
    }

    // Transfers: insert once (INV-2, REQ-KER-5).
    for id in delta.transfers.keys() {
        if view.transfer_stored(id)? {
            return Err(WriteRefusal::TransferExists(*id).into());
        }
    }

    // External ids: insert once (REQ-KER-6); a slot shared by two ids is fatal.
    let mut slots: BTreeMap<[u8; 16], [u8; 32]> = BTreeMap::new();
    for eid in &delta.external_ids {
        let slot = external_id_key(&eid.0);
        if let Some(other) = slots.insert(slot, eid.0)
            && other != eid.0
        {
            return Err(StoreError::ExternalIdCollision { slot });
        }
        match view.external_id_in_slot(&slot)? {
            None => {}
            Some(stored) if stored == eid.0 => {
                return Err(WriteRefusal::ExternalIdExists(*eid).into());
            }
            Some(_) => return Err(StoreError::ExternalIdCollision { slot }),
        }
    }

    // Voided markers: insert once.
    for id in &delta.voided {
        if view.voided_stored(id)? {
            return Err(WriteRefusal::VoidedExists(*id).into());
        }
    }

    // Pending statuses: Pending, then Posted or Voided only.
    for (id, to) in &delta.pending {
        let from = view.pending_stored(id)?;
        let allowed = match (from, *to) {
            (None, PendingStatus::Pending) => true,
            // A pending transfer and its finalisation in the same delta: the
            // delta holds only the final status, and the transfer itself.
            (None, PendingStatus::Posted | PendingStatus::Voided) => delta
                .transfers
                .get(id)
                .is_some_and(|t| t.flags.contains(TransferFlags::PENDING)),
            (Some(PendingStatus::Pending), PendingStatus::Posted | PendingStatus::Voided) => true,
            _ => false,
        };
        if !allowed {
            return Err(WriteRefusal::PendingTransition {
                id: *id,
                from,
                to: *to,
            }
            .into());
        }
    }

    // Postings: one record per transfer added, numbered without a gap (REQ-PO-2).
    let first = view.expected_posting_seq()?;
    let mut named: BTreeSet<TransferId> = BTreeSet::new();
    for (expected, p) in (first..).zip(&delta.postings) {
        if p.seq != expected {
            return Err(WriteRefusal::PostingSeqGap {
                expected,
                got: p.seq,
            }
            .into());
        }
        if !delta.transfers.contains_key(&p.transfer) || !named.insert(p.transfer) {
            return Err(WriteRefusal::PostingsMismatch.into());
        }
    }
    if named.len() != delta.transfers.len() {
        return Err(WriteRefusal::PostingsMismatch.into());
    }
    Ok(())
}

/// Whether `new` only rolls `old` forward: the same identity, asset, key,
/// code, normal balance and user data; flags unchanged or gaining `CLOSED`.
fn is_roll_of(old: &Account, new: &Account) -> bool {
    old.id == new.id
        && old.journal_id == new.journal_id
        && old.auth_key == new.auth_key
        && old.ledger == new.ledger
        && old.code == new.code
        && old.normal_balance == new.normal_balance
        && old.user_data == new.user_data
        && (new.flags == old.flags || new.flags == (old.flags | AccountFlags::CLOSED))
}
