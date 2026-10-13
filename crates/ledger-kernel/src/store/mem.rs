// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `MemStore`: the book's leaves in memory (`F-001` §10).
//!
//! It keeps each sub-tree as an incremental node table in memory ([`smt`]),
//! the same tree `FileStore` keeps on disk, so a commit costs the same
//! whatever the size of the book. [`MemStore::recomputed_root`] recomputes
//! the root with CipherClerk's own `composite_state_root`, the kernel's
//! reference, as an independent check (in v2, the same leaves under
//! `compact_smt::v2`). [`MemStore::with_version`] picks the root version, so a
//! differential test runs one book under both (`DEC-25`). It is the store for tests and for the
//! in-process conformance harness; it persists nothing.
//!
//! [`smt`]: super::smt

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use cipher_clerk::ids::ExternalId;
use cipher_clerk::merkle::{SMT_DEPTH, smt_leaf_hash};
use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::{
    account_leaf_content, external_id_key, external_id_leaf_content, journal_leaf_content,
    pending_leaf_content, transfer_leaf_content, voided_leaf_content,
};
use cipher_clerk::{Account, AccountId, Journal, JournalId, Transfer, TransferId};
use compact_smt::Version;

use super::rules::{self, RuleView};
use super::smt::{self, EmptyChain, MemNodes};
use super::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, StoreError, SubRoots, SubTree,
    composite_root_in, recomputed_composite_root,
};

/// The book's leaves, held in memory over six incremental sparse Merkle
/// trees.
#[derive(Clone, Debug)]
pub struct MemStore {
    version: Version,
    trees: [MemNodes; 6],
    empty: EmptyChain,
    subroots: SubRoots,
    accounts: BTreeMap<AccountId, Account>,
    transfers: BTreeMap<TransferId, Transfer>,
    journals: BTreeMap<JournalId, Journal>,
    /// External-id slot → the full id occupying it.
    external_ids: BTreeMap<[u8; 16], [u8; 32]>,
    voided: BTreeSet<TransferId>,
    pending: BTreeMap<TransferId, PendingStatus>,
    log: Vec<LogRecord>,
    next_posting_seq: u64,
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemStore {
    /// An empty store, in the build's root version.
    pub fn new() -> Self {
        Self::with_version(Version::ACTIVE)
    }

    /// An empty store whose trees are hashed in `version`.
    pub fn with_version(version: Version) -> Self {
        let empty = smt::empty_chain();
        let empty_root = match version {
            Version::V1 => empty[SMT_DEPTH],
            Version::V2 => compact_smt::v2::EMPTY,
        };
        Self {
            version,
            trees: core::array::from_fn(|_| MemNodes::with_version(version)),
            subroots: [empty_root; 6],
            empty,
            accounts: BTreeMap::new(),
            transfers: BTreeMap::new(),
            journals: BTreeMap::new(),
            external_ids: BTreeMap::new(),
            voided: BTreeSet::new(),
            pending: BTreeMap::new(),
            log: Vec::new(),
            next_posting_seq: 0,
        }
    }

    /// The store's root version.
    pub fn version(&self) -> Version {
        self.version
    }

    /// The composite root recomputed from scratch over every stored record
    /// with CipherClerk's `composite_state_root` (v1) or the same leaves under
    /// `compact_smt::v2` (v2). Equal to [`LeafStore::root`] whenever the store
    /// is consistent; tests use it as an independent check.
    pub fn recomputed_root(&self) -> [u8; 32] {
        let accounts: Vec<Account> = self.accounts.values().cloned().collect();
        let transfers: Vec<Transfer> = self.transfers.values().cloned().collect();
        let external_ids: Vec<[u8; 32]> = self.external_ids.values().copied().collect();
        let voided: Vec<[u8; 16]> = self.voided.iter().map(|id| id.0).collect();
        let pending: Vec<([u8; 16], u8)> = self
            .pending
            .iter()
            .map(|(id, s)| (id.0, s.code()))
            .collect();
        recomputed_composite_root(
            self.version,
            &accounts,
            &transfers,
            self.journals.values().next(),
            &external_ids,
            &voided,
            &pending,
        )
    }

    /// Every stored account, in id order.
    pub fn accounts(&self) -> impl Iterator<Item = &Account> {
        self.accounts.values()
    }

    /// Every stored transfer, in id order.
    pub fn transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers.values()
    }

    /// Set one leaf and its path; the sub-tree's root follows.
    fn set_leaf(&mut self, tree: SubTree, key: &[u8; 16], content: &[u8]) {
        let i = tree.index();
        let Ok(root) = smt::set_leaf(&mut self.trees[i], &self.empty, key, smt_leaf_hash(content));
        self.subroots[i] = root;
    }

    fn apply(&mut self, delta: &BookDelta) {
        for a in delta.accounts.values() {
            self.set_leaf(SubTree::Accounts, &a.id.0, &account_leaf_content(a));
            self.accounts.insert(a.id, a.clone());
        }
        for t in delta.transfers.values() {
            self.set_leaf(SubTree::Transfers, &t.id.0, &transfer_leaf_content(t));
            self.transfers.insert(t.id, t.clone());
        }
        for j in delta.journals.values() {
            self.set_leaf(SubTree::Journals, &j.id.0, &journal_leaf_content(j));
            self.journals.insert(j.id, j.clone());
        }
        for e in &delta.external_ids {
            let slot = external_id_key(&e.0);
            self.set_leaf(SubTree::ExternalIds, &slot, &external_id_leaf_content(&e.0));
            self.external_ids.insert(slot, e.0);
        }
        for v in &delta.voided {
            self.set_leaf(SubTree::Voided, &v.0, &voided_leaf_content(&v.0));
            self.voided.insert(*v);
        }
        for (id, s) in &delta.pending {
            self.set_leaf(
                SubTree::Pending,
                &id.0,
                &pending_leaf_content(&id.0, s.code()),
            );
            self.pending.insert(*id, *s);
        }
        self.next_posting_seq += delta.postings.len() as u64;
    }
}

impl RuleView for MemStore {
    fn expected_seq(&self) -> Result<u64, StoreError> {
        Ok(self.log.len() as u64)
    }
    fn expected_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.next_posting_seq)
    }
    fn stored_account(&self, id: &AccountId) -> Result<Option<Account>, StoreError> {
        Ok(self.accounts.get(id).cloned())
    }
    fn transfer_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        Ok(self.transfers.contains_key(id))
    }
    fn journal_stored(&self) -> Result<bool, StoreError> {
        Ok(!self.journals.is_empty())
    }
    fn external_id_in_slot(&self, slot: &[u8; 16]) -> Result<Option<[u8; 32]>, StoreError> {
        Ok(self.external_ids.get(slot).copied())
    }
    fn voided_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        Ok(self.voided.contains(id))
    }
    fn pending_stored(&self, id: &TransferId) -> Result<Option<PendingStatus>, StoreError> {
        Ok(self.pending.get(id).copied())
    }
}

impl LeafStore for MemStore {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        Ok(self.subroots)
    }

    fn root(&self) -> Result<[u8; 32], StoreError> {
        Ok(composite_root_in(self.version, &self.subroots))
    }

    fn version(&self) -> Version {
        self.version
    }

    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        let mut out = Loaded::default();
        for id in &keys.accounts {
            out.accounts.insert(*id, self.accounts.get(id).cloned());
        }
        for id in &keys.transfers {
            out.transfers.insert(*id, self.transfers.get(id).cloned());
        }
        for id in &keys.journals {
            out.journals.insert(*id, self.journals.get(id).cloned());
        }
        for eid in &keys.external_ids {
            let slot = external_id_key(&eid.0);
            let seen = match self.external_ids.get(&slot) {
                None => false,
                Some(stored) if *stored == eid.0 => true,
                Some(_) => return Err(StoreError::ExternalIdCollision { slot }),
            };
            out.external_ids.insert(ExternalId(eid.0), seen);
        }
        for id in &keys.voided {
            out.voided.insert(*id, self.voided.contains(id));
        }
        for id in &keys.pending {
            out.pending.insert(*id, self.pending.get(id).copied());
        }
        Ok(out)
    }

    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        rules::check(self, seq, delta)?;
        self.apply(delta);
        self.log.push(LogRecord {
            seq,
            delta: delta.clone(),
            root_after: composite_root_in(self.version, &self.subroots),
        });
        Ok(self.subroots)
    }

    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        Ok(self.log.last().map(|r| r.seq))
    }

    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.next_posting_seq)
    }

    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        Ok(usize::try_from(seq)
            .ok()
            .and_then(|i| self.log.get(i))
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::suite::{self, store_rule_tests};
    use cipher_clerk::state_root::external_id_key;

    store_rule_tests!(MemStore::new());

    #[test]
    fn subroots_fold_to_the_kernels_composite_root() {
        let mut s = MemStore::new();
        suite::bootstrap(&mut s);
        let mut d = suite::transfer_delta(&s, suite::transfer(1));
        d.accounts.insert(AccountId([1; 16]), suite::account(1));
        d.accounts.insert(AccountId([2; 16]), suite::account(2));
        d.external_ids.insert(ExternalId([5; 32]));
        d.voided.insert(TransferId([9; 16]));
        d.pending
            .insert(TransferId([7; 16]), PendingStatus::Pending);
        s.commit(1, &d).unwrap();
        assert_eq!(s.root().unwrap(), s.recomputed_root());
        assert_eq!(
            s.log_record(1).unwrap().unwrap().root_after,
            s.recomputed_root()
        );
    }

    #[test]
    fn an_external_id_slot_holding_another_id_is_fatal() {
        let mut s = MemStore::new();
        suite::bootstrap(&mut s);
        // Plant a different id in the slot of `eid`: a 64-bit birthday
        // collision cannot be found in a test, so it is written directly.
        let eid = ExternalId([5; 32]);
        let slot = external_id_key(&eid.0);
        s.external_ids.insert(slot, [6; 32]);
        let mut keys = KeySet::default();
        keys.external_ids.insert(eid);
        assert_eq!(s.load(&keys), Err(StoreError::ExternalIdCollision { slot }));
        let mut d = BookDelta::default();
        d.external_ids.insert(eid);
        assert_eq!(
            s.commit(1, &d),
            Err(StoreError::ExternalIdCollision { slot })
        );
    }
}
