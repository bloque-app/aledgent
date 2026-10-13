// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The working set the kernel runs over (`F-001` §5, `DEC-4`).
//!
//! A unit never runs against the store itself. Its keys are discovered from
//! the batch, loaded from the store in one call, and wrapped in a
//! [`WorkingSet`]: an overlay that implements CipherClerk's `LedgerState`.
//! Reads come from the loaded leaves; writes go to a [`BookDelta`]; `root()`
//! is the root before the unit.
//!
//! A read of a key that was not loaded **poisons** the set. The unit must
//! then be discarded ([`WorkingSet::into_delta`] returns the miss). Answering
//! "not seen" for a key nobody loaded would let a duplicate through, so a
//! miss is never an answer.

use core::cell::Cell;

use cipher_clerk::ids::ExternalId;
use cipher_clerk::kernel::CreateAccount;
use cipher_clerk::state::{LedgerState, Oracle, PendingStatus};
use cipher_clerk::{Account, AccountId, Journal, JournalId, Transfer, TransferFlags, TransferId};

use crate::store::{BookDelta, KeySet, LeafStore, Loaded, StoreError};

/// A key the kernel read without it having been loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnloadedKey {
    /// An account.
    Account(AccountId),
    /// A transfer.
    Transfer(TransferId),
    /// A journal.
    Journal(JournalId),
    /// An external id.
    ExternalId(ExternalId),
    /// A voided marker.
    Voided(TransferId),
    /// A pending status.
    Pending(TransferId),
}

/// The keys the kernel will read for these events, as far as the events
/// alone tell: accounts created and their journal; each transfer's id and
/// external id; every entry's account; and the transfers named by `void_of`
/// and `pending_id` with their voided and pending markers.
///
/// The accounts of a referenced transfer are only known once that transfer is
/// loaded; [`referenced_accounts`] finds them.
pub fn discover(creations: &[CreateAccount], transfers: &[Transfer]) -> KeySet {
    let mut keys = KeySet::default();
    for c in creations {
        keys.accounts.insert(c.account.id);
        keys.journals.insert(c.account.journal_id);
    }
    for t in transfers {
        keys.transfers.insert(t.id);
        if let Some(eid) = t.external_id {
            keys.external_ids.insert(eid);
        }
        for e in &t.entries {
            keys.accounts.insert(e.account_id);
        }
        if let Some(orig) = t.void_of {
            keys.transfers.insert(orig);
            keys.voided.insert(orig);
        }
        if let Some(pending) = t.pending_id {
            keys.transfers.insert(pending);
            keys.pending.insert(pending);
        }
    }
    keys
}

/// The accounts of every transfer the batch references through `void_of` or
/// `pending_id`, where that transfer is in `loaded`.
pub fn referenced_accounts(transfers: &[Transfer], loaded: &Loaded) -> KeySet {
    let mut keys = KeySet::default();
    for t in transfers {
        for referenced in [t.void_of, t.pending_id].into_iter().flatten() {
            if let Some(Some(r)) = loaded.transfers.get(&referenced) {
                keys.accounts.extend(r.entries.iter().map(|e| e.account_id));
            }
        }
    }
    keys
}

/// A copy of a working set's writes, to return to if a step fails.
#[derive(Clone, Debug)]
pub struct Savepoint(BookDelta);

/// The overlay the kernel runs a unit over.
#[derive(Debug)]
pub struct WorkingSet {
    loaded: Loaded,
    root_before: [u8; 32],
    delta: BookDelta,
    miss: Cell<Option<UnloadedKey>>,
}

impl WorkingSet {
    /// A working set over leaves already loaded, with the root they belong to.
    pub fn new(loaded: Loaded, root_before: [u8; 32]) -> Self {
        Self {
            loaded,
            root_before,
            delta: BookDelta::default(),
            miss: Cell::new(None),
        }
    }

    /// Discover the keys of these events, load them from `store` (in two
    /// rounds, so referenced transfers' accounts are included), and wrap them.
    pub fn load_for<S: LeafStore + ?Sized>(
        store: &S,
        creations: &[CreateAccount],
        transfers: &[Transfer],
    ) -> Result<Self, StoreError> {
        let root = store.root()?;
        let keys = discover(creations, transfers);
        let mut loaded = store.load(&keys)?;
        let more = referenced_accounts(transfers, &loaded).missing_from(&loaded);
        if !more.is_empty() {
            loaded.merge(store.load(&more)?);
        }
        Ok(Self::new(loaded, root))
    }

    /// Load any keys of these further events that are not loaded yet. The
    /// unit's writes so far are kept.
    pub fn extend_for<S: LeafStore + ?Sized>(
        &mut self,
        store: &S,
        creations: &[CreateAccount],
        transfers: &[Transfer],
    ) -> Result<(), StoreError> {
        let keys = discover(creations, transfers).missing_from(&self.loaded);
        if !keys.is_empty() {
            let got = store.load(&keys)?;
            self.loaded.merge(got);
        }
        let more = referenced_accounts(transfers, &self.loaded).missing_from(&self.loaded);
        if !more.is_empty() {
            let got = store.load(&more)?;
            self.loaded.merge(got);
        }
        Ok(())
    }

    /// The first key read without having been loaded, if any.
    pub fn miss(&self) -> Option<UnloadedKey> {
        self.miss.get()
    }

    /// Remember the current writes.
    pub fn savepoint(&self) -> Savepoint {
        Savepoint(self.delta.clone())
    }

    /// Return to a savepoint, dropping every write made since.
    pub fn rollback(&mut self, sp: Savepoint) {
        self.delta = sp.0;
    }

    /// The writes so far.
    pub fn delta(&self) -> &BookDelta {
        &self.delta
    }

    /// The account as the unit currently sees it, without poisoning the set:
    /// `None` if it is neither written nor loaded, or loaded as absent.
    pub fn peek_account(&self, id: &AccountId) -> Option<&Account> {
        self.delta
            .accounts
            .get(id)
            .or_else(|| self.loaded.accounts.get(id).and_then(Option::as_ref))
    }

    /// The transfer as the unit currently sees it, without poisoning the set.
    pub fn peek_transfer(&self, id: &TransferId) -> Option<&Transfer> {
        self.delta
            .transfers
            .get(id)
            .or_else(|| self.loaded.transfers.get(id).and_then(Option::as_ref))
    }

    /// The unit's writes, or the key whose miss poisoned the set.
    pub fn into_delta(self) -> Result<BookDelta, UnloadedKey> {
        match self.miss.get() {
            Some(miss) => Err(miss),
            None => Ok(self.delta),
        }
    }

    fn poison(&self, key: UnloadedKey) {
        if self.miss.get().is_none() {
            self.miss.set(Some(key));
        }
    }
}

impl LedgerState for WorkingSet {
    fn root(&self) -> [u8; 32] {
        self.root_before
    }

    fn get_account(&self, id: &AccountId, _: &mut dyn Oracle) -> Option<Account> {
        if let Some(a) = self.delta.accounts.get(id) {
            return Some(a.clone());
        }
        match self.loaded.accounts.get(id) {
            Some(a) => a.clone(),
            None => {
                self.poison(UnloadedKey::Account(*id));
                None
            }
        }
    }

    fn put_account(&mut self, a: Account, _: &mut dyn Oracle) {
        self.delta.accounts.insert(a.id, a);
    }

    fn get_transfer(&self, id: &TransferId, _: &mut dyn Oracle) -> Option<Transfer> {
        if let Some(t) = self.delta.transfers.get(id) {
            return Some(t.clone());
        }
        match self.loaded.transfers.get(id) {
            Some(t) => t.clone(),
            None => {
                self.poison(UnloadedKey::Transfer(*id));
                None
            }
        }
    }

    fn put_transfer(&mut self, t: Transfer, _: &mut dyn Oracle) {
        self.delta.transfers.insert(t.id, t);
    }

    fn external_id_seen(&self, eid: &ExternalId, _: &mut dyn Oracle) -> bool {
        if self.delta.external_ids.contains(eid) {
            return true;
        }
        match self.loaded.external_ids.get(eid) {
            Some(seen) => *seen,
            None => {
                self.poison(UnloadedKey::ExternalId(*eid));
                false
            }
        }
    }

    fn mark_external_id(&mut self, eid: &ExternalId, _: &mut dyn Oracle) {
        self.delta.external_ids.insert(*eid);
    }

    fn transfer_voided(&self, id: &TransferId, _: &mut dyn Oracle) -> bool {
        if self.delta.voided.contains(id) {
            return true;
        }
        match self.loaded.voided.get(id) {
            Some(v) => *v,
            None => {
                self.poison(UnloadedKey::Voided(*id));
                false
            }
        }
    }

    fn mark_transfer_voided(&mut self, id: &TransferId, _: &mut dyn Oracle) {
        self.delta.voided.insert(*id);
    }

    fn get_journal(&self, id: &JournalId, _: &mut dyn Oracle) -> Option<Journal> {
        if let Some(j) = self.delta.journals.get(id) {
            return Some(j.clone());
        }
        match self.loaded.journals.get(id) {
            Some(j) => j.clone(),
            None => {
                self.poison(UnloadedKey::Journal(*id));
                None
            }
        }
    }

    fn put_journal(&mut self, j: Journal, _: &mut dyn Oracle) {
        self.delta.journals.insert(j.id, j);
    }

    fn pending_status(&self, id: &TransferId, _: &mut dyn Oracle) -> Option<PendingStatus> {
        if let Some(s) = self.delta.pending.get(id) {
            return Some(*s);
        }
        match self.loaded.pending.get(id) {
            Some(s) => *s,
            None => {
                self.poison(UnloadedKey::Pending(*id));
                None
            }
        }
    }

    fn mark_pending_status(&mut self, id: &TransferId, status: PendingStatus, _: &mut dyn Oracle) {
        self.delta.pending.insert(*id, status);
    }
}

/// Whether a transfer finalises a pending one (`POST` or `VOID` pending).
pub fn is_finalize(t: &Transfer) -> bool {
    t.flags.contains(TransferFlags::POST_PENDING_TRANSFER)
        || t.flags.contains(TransferFlags::VOID_PENDING_TRANSFER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemStore, PostingRecord};
    use alloc::vec;
    use alloc::vec::Vec;
    use cipher_clerk::crypto::Keypair;
    use cipher_clerk::helpers::{MemLedger, MemOracle};
    use cipher_clerk::{EventStatus, Layer, apply_account_creations, apply_batch};

    /// A book with a journal and two funded-able accounts, built identically
    /// in a `MemStore` (through a working set) and in the kernel's `MemLedger`.
    struct Fixture {
        store: MemStore,
        reference: MemLedger,
        oracle: MemOracle,
        registrar: Keypair,
        journal: JournalId,
        alice: (Account, Keypair),
        bob: (Account, Keypair),
    }

    fn commit(store: &mut MemStore, mut delta: BookDelta) {
        let next = store.next_posting_seq().unwrap();
        delta.postings = delta
            .transfers
            .keys()
            .enumerate()
            .map(|(i, id)| PostingRecord {
                seq: next + i as u64,
                transfer: *id,
            })
            .collect();
        let seq = store.last_committed().unwrap().map_or(0, |n| n + 1);
        store.commit(seq, &delta).unwrap();
    }

    fn fixture() -> Fixture {
        let registrar = Keypair::generate();
        let journal = JournalId([0x10; 16]);
        let j = Journal::new(journal, registrar.public, 0);
        let mut store = MemStore::new();
        let mut boot = BookDelta::default();
        boot.journals.insert(journal, j.clone());
        store.commit(0, &boot).unwrap();
        let mut reference = MemLedger::new();
        let mut oracle = MemOracle::new();
        reference.put_journal(j, &mut oracle);

        let mk = |n: u8| {
            let kp = Keypair::generate();
            let a = Account::new(
                AccountId([n; 16]),
                journal,
                kp.public,
                1u32,
                1u16,
                cipher_clerk::Direction::Credit,
            );
            (a, kp)
        };
        let alice = mk(1);
        let bob = mk(2);
        let creations = vec![
            CreateAccount::signed(alice.0.clone(), &registrar.secret),
            CreateAccount::signed(bob.0.clone(), &registrar.secret),
        ];
        let mut ws = WorkingSet::load_for(&store, &creations, &[]).unwrap();
        let r1 = apply_account_creations(&mut ws, &creations, &mut oracle, 10);
        let r2 = apply_account_creations(&mut reference, &creations, &mut oracle, 10);
        assert_eq!(r1, r2);
        commit(&mut store, ws.into_delta().unwrap());
        assert_eq!(
            store.root().unwrap(),
            crate::testkit::kernel_reference_root(&reference)
        );
        Fixture {
            store,
            reference,
            oracle,
            registrar,
            journal,
            alice,
            bob,
        }
    }

    fn pay(f: &mut Fixture, id: u8, value: u64, flags: TransferFlags) -> Transfer {
        let amount = f.oracle.commit(value);
        Transfer::builder(f.journal)
            .id(TransferId([id; 16]))
            .external_id(ExternalId([id; 32]))
            .flags(flags)
            .debit(&f.alice.0, Layer::Settled, amount)
            .credit(&f.bob.0, Layer::Settled, amount)
            .signed_with(&[(&f.alice.0, &f.alice.1.secret)])
    }

    #[test]
    fn a_batch_over_the_working_set_matches_the_kernels_reference() {
        let mut f = fixture();
        let batch: Vec<Transfer> = vec![
            pay(&mut f, 0x21, 5, TransferFlags::LINKED),
            pay(&mut f, 0x22, 7, TransferFlags::NONE),
            pay(&mut f, 0x23, 9, TransferFlags::NONE),
        ];
        let mut ws = WorkingSet::load_for(&f.store, &[], &batch).unwrap();
        let mut o = f.oracle.clone();
        let ours = apply_batch(&mut ws, &batch, &mut o, 100);
        let theirs = apply_batch(&mut f.reference, &batch, &mut f.oracle, 100);
        assert_eq!(ours, theirs);
        assert!(ours.iter().all(|r| r.status == EventStatus::Created));
        commit(&mut f.store, ws.into_delta().unwrap());
        assert_eq!(
            f.store.root().unwrap(),
            crate::testkit::kernel_reference_root(&f.reference)
        );

        // The same external ids again: refused as duplicates, from loaded leaves.
        let mut ws = WorkingSet::load_for(&f.store, &[], &batch[1..]).unwrap();
        let again = apply_batch(&mut ws, &batch[1..], &mut o, 200);
        assert!(
            again
                .iter()
                .all(|r| r.status == EventStatus::TransferIdAlreadyExists)
        );
        assert!(ws.into_delta().unwrap().is_empty());
    }

    #[test]
    fn a_withheld_account_poisons_the_set() {
        let mut f = fixture();
        let batch = vec![pay(&mut f, 0x31, 5, TransferFlags::NONE)];
        let mut keys = discover(&[], &batch);
        keys.accounts.remove(&f.bob.0.id);
        let loaded = f.store.load(&keys).unwrap();
        let mut ws = WorkingSet::new(loaded, f.store.root().unwrap());
        let _ = apply_batch(&mut ws, &batch, &mut f.oracle, 100);
        assert_eq!(ws.miss(), Some(UnloadedKey::Account(f.bob.0.id)));
        assert_eq!(ws.into_delta(), Err(UnloadedKey::Account(f.bob.0.id)));
    }

    #[test]
    fn a_withheld_external_id_poisons_instead_of_answering_not_seen() {
        let mut f = fixture();
        let batch = vec![pay(&mut f, 0x32, 5, TransferFlags::NONE)];
        let mut keys = discover(&[], &batch);
        keys.external_ids.clear();
        let loaded = f.store.load(&keys).unwrap();
        let mut ws = WorkingSet::new(loaded, f.store.root().unwrap());
        let _ = apply_batch(&mut ws, &batch, &mut f.oracle, 100);
        assert_eq!(
            ws.into_delta(),
            Err(UnloadedKey::ExternalId(ExternalId([0x32; 32])))
        );
    }

    #[test]
    fn req_ker_10_rollback_to_a_savepoint_drops_creations() {
        let f = fixture();
        let kp = Keypair::generate();
        let carol = Account::new(
            AccountId([3; 16]),
            f.journal,
            kp.public,
            1u32,
            1u16,
            cipher_clerk::Direction::Credit,
        );
        let creations = vec![CreateAccount::signed(carol, &f.registrar.secret)];
        let mut ws = WorkingSet::load_for(&f.store, &creations, &[]).unwrap();
        let sp = ws.savepoint();
        let mut o = MemOracle::new();
        let r = apply_account_creations(&mut ws, &creations, &mut o, 10);
        assert_eq!(r[0].status, EventStatus::Created);
        assert!(!ws.delta().accounts.is_empty());
        ws.rollback(sp);
        assert!(ws.into_delta().unwrap().is_empty());
    }

    #[test]
    fn discovery_reaches_the_accounts_of_a_referenced_pending_transfer() {
        let mut f = fixture();
        // A pending transfer, committed, then its finalisation, which carries
        // no entries: its accounts are only known from the pending transfer.
        let amount = f.oracle.commit(4);
        let pending = Transfer::builder(f.journal)
            .id(TransferId([0x41; 16]))
            .flags(TransferFlags::PENDING)
            .debit(&f.alice.0, Layer::Pending, amount)
            .credit(&f.bob.0, Layer::Pending, amount)
            .signed_with(&[(&f.alice.0, &f.alice.1.secret)]);
        let mut ws = WorkingSet::load_for(&f.store, &[], core::slice::from_ref(&pending)).unwrap();
        let mut o = f.oracle.clone();
        let r = apply_batch(&mut ws, core::slice::from_ref(&pending), &mut o, 50);
        assert_eq!(r[0].status, EventStatus::Created);
        commit(&mut f.store, ws.into_delta().unwrap());

        let post = Transfer::draft(TransferId([0x42; 16]), f.journal, Vec::new())
            .with_flags(TransferFlags::POST_PENDING_TRANSFER)
            .finalizing(pending.id)
            .signed_by(&[&f.alice.1.secret]);
        assert!(is_finalize(&post));
        let first = discover(&[], core::slice::from_ref(&post));
        assert!(first.accounts.is_empty());
        let ws = WorkingSet::load_for(&f.store, &[], core::slice::from_ref(&post)).unwrap();
        assert!(ws.peek_account(&f.alice.0.id).is_some());
        assert!(ws.peek_account(&f.bob.0.id).is_some());
        let mut ws = ws;
        let r = apply_batch(&mut ws, core::slice::from_ref(&post), &mut o, 60);
        assert_eq!(r[0].status, EventStatus::Created);
        assert_eq!(ws.miss(), None);
    }
}
