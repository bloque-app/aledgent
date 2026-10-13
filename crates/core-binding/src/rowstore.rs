// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The kernel's leaf store over the Agent's own committed maps (`F-031` lever 2, `T-031-07`).
//!
//! [`RowStore`] is the `LeafStore` the library runs over inside the Agent once the kernel's six
//! trees are committed maps in its rows ([`crate::committed`]). It replaces the witnessed store
//! ([`crate::witness::WitnessStore`]): a load reads the leaves asked for, one row each, and
//! nothing else; a commit runs `F-001`'s write rules over the stored leaves (`INV-2`) and writes
//! each changed leaf and its path. No host builds a witness, and no proof is checked: the rows
//! are the Agent's, committed by its runtime.
//!
//! Every write goes to an [`Overlay`] over the Agent's rows, which the transition applies only
//! when it succeeds, so a refused invocation leaves the state as it was.
//!
//! Each leaf row stores the leaf's hash (CipherClerk's, over its leaf content) and the leaf
//! itself: the kernel's rkyv archive for accounts, transfers and journals, the full external id,
//! nothing for a voided marker, and the status code for a pending status.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use cipher_clerk::ids::ExternalId;
use cipher_clerk::merkle::smt_leaf_hash;
use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::{
    account_leaf_content, external_id_key, external_id_leaf_content, journal_leaf_content,
    pending_leaf_content, transfer_leaf_content, voided_leaf_content,
};
use cipher_clerk::{Account, AccountId, Journal, Transfer, TransferId};
use ledger_kernel::store::rules::{self, RuleView};
use ledger_kernel::store::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, StoreError, SubRoots, SubTree,
};

use crate::committed::{self, CommittedMap, Corrupt, EmptyChain};
use crate::transition::Rows;

/// The Agent's rows with this invocation's writes on top: reads see the writes, and nothing
/// reaches the rows until [`Overlay::into_writes`] is applied.
pub struct Overlay<'a> {
    base: &'a dyn Rows,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<'a> Overlay<'a> {
    /// No writes yet over `base`.
    pub fn new(base: &'a dyn Rows) -> Self {
        Self {
            base,
            writes: BTreeMap::new(),
        }
    }

    /// The writes, each a value or a removal.
    pub fn into_writes(self) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
        self.writes
    }

    /// How many rows were written.
    pub fn written(&self) -> usize {
        self.writes.len()
    }
}

impl Rows for Overlay<'_> {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self.writes.get(key) {
            Some(v) => v.clone(),
            None => self.base.get(key),
        }
    }
    fn put(&mut self, key: &[u8], value: Vec<u8>) {
        self.writes.insert(key.to_vec(), Some(value));
    }
    fn remove(&mut self, key: &[u8]) {
        self.writes.insert(key.to_vec(), None);
    }
}

fn corrupt(c: Corrupt) -> StoreError {
    StoreError::Corrupt(String::from(c.0))
}

fn archive<T>(v: &T) -> Vec<u8>
where
    T: for<'b> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'b>,
                rkyv::rancor::Error,
            >,
        >,
{
    rkyv::to_bytes::<rkyv::rancor::Error>(v)
        .expect("archiving a kernel leaf only fails on allocator exhaustion")
        .to_vec()
}

fn unarchive<T>(bytes: &[u8]) -> Result<T, StoreError>
where
    T: rkyv::Archive,
    T::Archived: for<'b> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'b, rkyv::rancor::Error>>
        + rkyv::Deserialize<T, rkyv::api::high::HighDeserializer<rkyv::rancor::Error>>,
{
    let mut aligned = rkyv::util::AlignedVec::<16>::with_capacity(bytes.len());
    aligned.extend_from_slice(bytes);
    rkyv::from_bytes::<T, rkyv::rancor::Error>(&aligned)
        .map_err(|e| StoreError::Corrupt(alloc::format!("a kernel leaf: {e}")))
}

/// The tree a sub-tree is kept in: its composite index.
fn tree(t: SubTree) -> u8 {
    t.index() as u8
}

/// An overlay the transition and the store it hands the library share: the library writes the
/// kernel's leaves through the store, and the transition the service's, into the same writes.
pub type Shared<'a> = Rc<RefCell<Overlay<'a>>>;

/// The kernel's leaf store over the Agent's committed maps (module documentation).
pub struct RowStore<'a> {
    rows: Shared<'a>,
    empty: &'a EmptyChain,
    last: Option<u64>,
    next_posting: u64,
}

impl<'a> RowStore<'a> {
    /// The store over `rows`; its last commit and next posting number are the Agent's.
    pub fn new(
        rows: Shared<'a>,
        empty: &'a EmptyChain,
        last: Option<u64>,
        next_posting: u64,
    ) -> Self {
        Self {
            rows,
            empty,
            last,
            next_posting,
        }
    }

    fn leaf(&self, t: SubTree, key: &[u8; 16]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(committed::get(&*self.rows.borrow(), tree(t), key)
            .map_err(corrupt)?
            .map(|(_, bytes)| bytes))
    }

    fn set(
        &mut self,
        t: SubTree,
        key: &[u8; 16],
        content: &[u8],
        stored: &[u8],
    ) -> Result<(), StoreError> {
        let mut rows = self.rows.borrow_mut();
        CommittedMap::new(&mut *rows, tree(t), self.empty)
            .put(key, Some((smt_leaf_hash(content), stored)))
            .map_err(corrupt)?;
        Ok(())
    }

    fn account(&self, id: &AccountId) -> Result<Option<Account>, StoreError> {
        self.leaf(SubTree::Accounts, &id.0)?
            .map(|b| unarchive::<Account>(&b))
            .transpose()
    }

    fn slot(&self, slot: &[u8; 16]) -> Result<Option<[u8; 32]>, StoreError> {
        match self.leaf(SubTree::ExternalIds, slot)? {
            None => Ok(None),
            Some(b) if b.len() == 32 => {
                let mut id = [0u8; 32];
                id.copy_from_slice(&b);
                Ok(Some(id))
            }
            Some(_) => Err(StoreError::Corrupt(String::from("an external-id leaf"))),
        }
    }
}

impl RuleView for RowStore<'_> {
    fn expected_seq(&self) -> Result<u64, StoreError> {
        Ok(self.last.map_or(0, |n| n + 1))
    }
    fn expected_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.next_posting)
    }
    fn stored_account(&self, id: &AccountId) -> Result<Option<Account>, StoreError> {
        self.account(id)
    }
    fn transfer_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        Ok(self.leaf(SubTree::Transfers, &id.0)?.is_some())
    }
    fn journal_stored(&self) -> Result<bool, StoreError> {
        let empty = ledger_kernel::compact_smt::Version::ACTIVE.empty_root();
        let root = committed::root(&*self.rows.borrow(), tree(SubTree::Journals), self.empty);
        Ok(root.map_err(corrupt)? != empty)
    }
    fn external_id_in_slot(&self, slot: &[u8; 16]) -> Result<Option<[u8; 32]>, StoreError> {
        self.slot(slot)
    }
    fn voided_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        Ok(self.leaf(SubTree::Voided, &id.0)?.is_some())
    }
    fn pending_stored(&self, id: &TransferId) -> Result<Option<PendingStatus>, StoreError> {
        match self.leaf(SubTree::Pending, &id.0)? {
            None => Ok(None),
            Some(b) if b.len() == 1 => PendingStatus::from_code(b[0])
                .map(Some)
                .ok_or_else(|| StoreError::Corrupt(String::from("a pending status"))),
            Some(_) => Err(StoreError::Corrupt(String::from("a pending leaf"))),
        }
    }
}

impl LeafStore for RowStore<'_> {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        let rows = self.rows.borrow();
        let mut out = [[0u8; 32]; 6];
        for t in SubTree::ALL {
            out[t.index()] = committed::root(&*rows, tree(t), self.empty).map_err(corrupt)?;
        }
        Ok(out)
    }

    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        let mut out = Loaded::default();
        for k in &keys.accounts {
            out.accounts.insert(*k, self.account(k)?);
        }
        for k in &keys.transfers {
            let t = self
                .leaf(SubTree::Transfers, &k.0)?
                .map(|b| unarchive::<Transfer>(&b))
                .transpose()?;
            out.transfers.insert(*k, t);
        }
        for k in &keys.journals {
            let j = self
                .leaf(SubTree::Journals, &k.0)?
                .map(|b| unarchive::<Journal>(&b))
                .transpose()?;
            out.journals.insert(*k, j);
        }
        for e in &keys.external_ids {
            let slot = external_id_key(&e.0);
            let seen = match self.slot(&slot)? {
                None => false,
                Some(stored) if stored == e.0 => true,
                Some(_) => return Err(StoreError::ExternalIdCollision { slot }),
            };
            out.external_ids.insert(ExternalId(e.0), seen);
        }
        for k in &keys.voided {
            out.voided.insert(*k, self.voided_stored(k)?);
        }
        for k in &keys.pending {
            out.pending.insert(*k, self.pending_stored(k)?);
        }
        Ok(out)
    }

    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        rules::check(self, seq, delta)?;
        for a in delta.accounts.values() {
            self.set(
                SubTree::Accounts,
                &a.id.0,
                &account_leaf_content(a),
                &archive(a),
            )?;
        }
        for t in delta.transfers.values() {
            self.set(
                SubTree::Transfers,
                &t.id.0,
                &transfer_leaf_content(t),
                &archive(t),
            )?;
        }
        for j in delta.journals.values() {
            self.set(
                SubTree::Journals,
                &j.id.0,
                &journal_leaf_content(j),
                &archive(j),
            )?;
        }
        for e in &delta.external_ids {
            let slot = external_id_key(&e.0);
            self.set(
                SubTree::ExternalIds,
                &slot,
                &external_id_leaf_content(&e.0),
                &e.0,
            )?;
        }
        for v in &delta.voided {
            self.set(SubTree::Voided, &v.0, &voided_leaf_content(&v.0), &[])?;
        }
        for (id, s) in &delta.pending {
            self.set(
                SubTree::Pending,
                &id.0,
                &pending_leaf_content(&id.0, s.code()),
                &[s.code()],
            )?;
        }
        self.last = Some(seq);
        self.next_posting += delta.postings.len() as u64;
        self.subroots()
    }

    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        Ok(self.last)
    }

    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.next_posting)
    }

    fn log_record(&self, _seq: u64) -> Result<Option<LogRecord>, StoreError> {
        // The Agent keeps no delta log: its runtime's journal is the history. No command reads it.
        Err(StoreError::Io(String::from("the Agent keeps no delta log")))
    }
}

#[cfg(test)]
mod tests {
    use ledger_kernel::store::MemStore;

    use super::*;
    use crate::transition::MemRows;

    /// The kernel's write-rule suite and its reference store agree with the rows: same roots
    /// after the same commits (`REQ-RT-3`).
    #[test]
    fn req_rt_3_the_row_store_reaches_the_memory_stores_roots() {
        use ledger_kernel::store::suite;
        let empty = committed::empty_chain();
        let mut rows = MemRows::default();
        let mut mem = MemStore::new();
        suite::bootstrap(&mut mem);
        let boot = mem.log_record(0).unwrap().unwrap().delta;
        let mut d1 = suite::transfer_delta(&mem, suite::transfer(1));
        d1.accounts.insert(AccountId([1; 16]), suite::account(1));
        d1.accounts.insert(AccountId([2; 16]), suite::account(2));
        d1.external_ids.insert(ExternalId([5; 32]));
        d1.voided.insert(TransferId([9; 16]));
        d1.pending
            .insert(TransferId([7; 16]), PendingStatus::Pending);
        mem.commit(1, &d1).unwrap();
        let writes = {
            let shared = Rc::new(RefCell::new(Overlay::new(&rows)));
            let mut store = RowStore::new(Rc::clone(&shared), &empty, None, 0);
            store.commit(0, &boot).unwrap();
            store.commit(1, &d1).unwrap();
            assert_eq!(store.subroots().unwrap(), mem.subroots().unwrap());
            let mut keys = KeySet::default();
            keys.accounts.insert(AccountId([1; 16]));
            keys.external_ids.insert(ExternalId([5; 32]));
            keys.pending.insert(TransferId([7; 16]));
            assert_eq!(store.load(&keys).unwrap(), mem.load(&keys).unwrap());
            drop(store);
            Rc::try_unwrap(shared)
                .ok()
                .unwrap()
                .into_inner()
                .into_writes()
        };
        for (k, v) in writes {
            match v {
                Some(v) => rows.put(&k, v),
                None => rows.remove(&k),
            }
        }
        let shared = Rc::new(RefCell::new(Overlay::new(&rows)));
        let again = RowStore::new(shared, &empty, Some(1), mem.next_posting_seq().unwrap());
        assert_eq!(again.subroots().unwrap(), mem.subroots().unwrap());
    }
}
