// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `IndexStore`: a spike-only in-memory `LeafStore` whose roots come from the
//! compact index of [`crate::witness`], for books too large for `FileStore`
//! on the reference host (S3 at 100 000 and 1 000 000 accounts).
//!
//! It is **not** a candidate store. It does not enforce `INV-2`'s write rules
//! (`FileStore` and `MemStore` do, through `ledger-kernel`'s private rules
//! module), keeps only the last few delta-log records, and persists nothing.
//! Its roots are the kernel's: the index is checked against CipherClerk's
//! `SparseMerkleTree` in `witness::tests` and against `FileStore` at 10 000
//! accounts in S3.

use std::collections::{BTreeMap, HashMap, VecDeque};

use cipher_clerk::merkle::{SMT_DEPTH, build_empty_chain};
use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::external_id_key;
use cipher_clerk::{Account, AccountId, Journal, JournalId, Transfer, TransferId};
use ledger_kernel::store::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, StoreError, SubRoots, composite_root,
};

use crate::witness::Model;

/// How many delta-log records are kept.
const LOG_KEPT: usize = 16;

/// The book's leaves in hash maps, rooted by the compact index.
pub struct IndexStore {
    model: std::cell::RefCell<Model>,
    empty: [[u8; 32]; SMT_DEPTH + 1],
    accounts: HashMap<AccountId, Account>,
    transfers: HashMap<TransferId, Transfer>,
    journals: BTreeMap<JournalId, Journal>,
    external_ids: HashMap<[u8; 16], [u8; 32]>,
    voided: HashMap<TransferId, ()>,
    pending: HashMap<TransferId, PendingStatus>,
    log: VecDeque<LogRecord>,
    last: Option<u64>,
    next_posting_seq: u64,
}

impl Default for IndexStore {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexStore {
    /// An empty store.
    pub fn new() -> Self {
        Self {
            model: Default::default(),
            empty: build_empty_chain(),
            accounts: HashMap::new(),
            transfers: HashMap::new(),
            journals: BTreeMap::new(),
            external_ids: HashMap::new(),
            voided: HashMap::new(),
            pending: HashMap::new(),
            log: VecDeque::new(),
            last: None,
            next_posting_seq: 0,
        }
    }

    /// A copy of the index, for the witness builder.
    pub fn model(&self) -> Model {
        self.model.borrow().clone()
    }

    /// Every account leaf, for building a leaf table.
    pub fn accounts(&self) -> impl Iterator<Item = &Account> {
        self.accounts.values()
    }
}

impl LeafStore for IndexStore {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        Ok(self.model.borrow_mut().subroots(&self.empty))
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
            let seen = match self.external_ids.get(&external_id_key(&eid.0)) {
                None => false,
                Some(full) if full == &eid.0 => true,
                Some(_) => {
                    return Err(StoreError::ExternalIdCollision {
                        slot: external_id_key(&eid.0),
                    });
                }
            };
            out.external_ids.insert(*eid, seen);
        }
        for id in &keys.voided {
            out.voided.insert(*id, self.voided.contains_key(id));
        }
        for id in &keys.pending {
            out.pending.insert(*id, self.pending.get(id).copied());
        }
        Ok(out)
    }

    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        let expected = self.last.map_or(0, |n| n + 1);
        assert_eq!(seq, expected, "IndexStore: commits are gapless");
        for (id, a) in &delta.accounts {
            self.accounts.insert(*id, a.clone());
        }
        for (id, t) in &delta.transfers {
            self.transfers.insert(*id, t.clone());
        }
        for (id, j) in &delta.journals {
            self.journals.insert(*id, j.clone());
        }
        for eid in &delta.external_ids {
            self.external_ids.insert(external_id_key(&eid.0), eid.0);
        }
        for id in &delta.voided {
            self.voided.insert(*id, ());
        }
        for (id, s) in &delta.pending {
            self.pending.insert(*id, *s);
        }
        if let Some(p) = delta.postings.last() {
            self.next_posting_seq = p.seq + 1;
        }
        self.model.borrow_mut().apply(delta);
        let sub = self.subroots()?;
        self.log.push_back(LogRecord {
            seq,
            delta: delta.clone(),
            root_after: composite_root(&sub),
        });
        if self.log.len() > LOG_KEPT {
            self.log.pop_front();
        }
        self.last = Some(seq);
        Ok(sub)
    }

    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        Ok(self.last)
    }

    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.next_posting_seq)
    }

    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        Ok(self.log.iter().find(|r| r.seq == seq).cloned())
    }
}
