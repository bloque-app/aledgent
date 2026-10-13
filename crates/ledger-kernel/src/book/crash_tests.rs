// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Crash injection around the two commits of a unit (`F-001` §9, T-001-07).
//!
//! The vault commits sequence number `seq` first, then the leaf store. A
//! process can die before the vault's commit, between the two, or after
//! both. After a restart from the files alone, the book must be at the last
//! sequence number the store committed, with a vault that opens every stored
//! balance, and the unit must then either apply afresh or be a duplicate.

use super::*;
use crate::posting::PostingKind;
use crate::store::{FileStore, KeySet, Loaded, MemStore, SubRoots};
use crate::testkit::*;
use crate::vault::{FileVault, LayerSums, MemVault, Undo, VaultImage};
use alloc::vec;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CrashAt {
    /// The process dies before the vault writes.
    BeforeVaultCommit,
    /// The vault has committed; the process dies before the store commits.
    BetweenCommits,
    /// Both have committed; the process dies before `apply` returns.
    AfterStoreCommit,
}

struct CrashStore<S> {
    inner: S,
    at: CrashAt,
}

impl<S: LeafStore> LeafStore for CrashStore<S> {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        self.inner.subroots()
    }
    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        self.inner.load(keys)
    }
    fn commit(&mut self, seq: u64, d: &BookDelta) -> Result<SubRoots, StoreError> {
        if self.at == CrashAt::BetweenCommits {
            panic!("injected crash between the vault and the store commit");
        }
        let r = self.inner.commit(seq, d);
        if self.at == CrashAt::AfterStoreCommit {
            panic!("injected crash after the store commit");
        }
        r
    }
    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.inner.last_committed()
    }
    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        self.inner.next_posting_seq()
    }
    fn log_record(&self, seq: u64) -> Result<Option<crate::store::LogRecord>, StoreError> {
        self.inner.log_record(seq)
    }
}

struct CrashVault<V> {
    inner: V,
    at: CrashAt,
}

impl<V: VaultBackend> VaultBackend for CrashVault<V> {
    fn read(&self) -> Result<VaultImage, VaultError> {
        self.inner.read()
    }
    fn write(&mut self, rows: &[(RowKey, LayerSums)], undo: &Undo) -> Result<(), VaultError> {
        if self.at == CrashAt::BeforeVaultCommit {
            panic!("injected crash before the vault commit");
        }
        self.inner.write(rows, undo)
    }
    fn revert(&mut self, undo: &Undo) -> Result<(), VaultError> {
        self.inner.revert(undo)
    }
}

fn the_unit() -> Unit {
    unit(vec![chain(vec![mv(
        PostingKind::Transfer,
        0xC0,
        0,
        customer(1, 1),
        customer(2, 1),
        30,
    )])])
}

fn open_files(dir: &Path) -> Book<FileStore, FileVault> {
    Book::open(
        FileStore::open(dir.join("book.redb")).unwrap(),
        FileVault::open(dir.join("vault.redb")).unwrap(),
        KeyRing::from_root(ROOT),
    )
    .expect("the book reopens")
}

/// The roots the same history gives in memory: before and after the unit.
fn expected_roots() -> ([u8; 32], [u8; 32]) {
    let mut book = mem_book();
    fund(&mut book, 100);
    let before = book.root().unwrap();
    book.apply(&the_unit(), 7_000).unwrap();
    (before, book.root().unwrap())
}

fn crash_and_recover(at: CrashAt) {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut book = Book::bootstrap(
            FileStore::open(dir.path().join("book.redb")).unwrap(),
            FileVault::open(dir.path().join("vault.redb")).unwrap(),
            KeyRing::from_root(ROOT),
        )
        .unwrap();
        fund(&mut book, 100);
    }
    let (before, after) = expected_roots();

    // Run the unit in a process that dies at `at`.
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        let mut book = Book::open(
            CrashStore {
                inner: FileStore::open(dir.path().join("book.redb")).unwrap(),
                at,
            },
            CrashVault {
                inner: FileVault::open(dir.path().join("vault.redb")).unwrap(),
                at,
            },
            KeyRing::from_root(ROOT),
        )
        .unwrap();
        let _ = book.apply(&the_unit(), 7_000);
    }));
    assert!(crashed.is_err(), "the injected crash must fire");

    // Restart from the files alone.
    let mut book = open_files(dir.path());
    let committed = book.last_committed().unwrap();
    assert_eq!(book.vault().last_committed(), committed);
    book.verify_openings().unwrap();
    let rec = book
        .store()
        .log_record(committed.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rec.root_after, book.root().unwrap());
    match at {
        CrashAt::BeforeVaultCommit | CrashAt::BetweenCommits => {
            assert_eq!(committed, Some(1));
            assert_eq!(book.root().unwrap(), before);
            assert_eq!(book.settled_net(&customer(1, 1).id()).unwrap(), Some(100));
            // The unit never landed, so it applies now.
            let applied = book.apply(&the_unit(), 7_000).unwrap();
            assert_eq!(applied.seq, Some(2));
            assert_eq!(applied.root, after);
        }
        CrashAt::AfterStoreCommit => {
            assert_eq!(committed, Some(2));
            assert_eq!(book.root().unwrap(), after);
            assert_eq!(book.settled_net(&customer(1, 1).id()).unwrap(), Some(70));
            // It landed, so a retry is a duplicate.
            let again = book.apply(&the_unit(), 8_000).unwrap();
            assert_eq!(again.steps[0].events, vec![EventClass::Duplicate]);
        }
    }
    book.verify_openings().unwrap();
}

#[test]
fn a_crash_before_the_vault_commit_recovers_to_the_last_seq() {
    crash_and_recover(CrashAt::BeforeVaultCommit);
}

#[test]
fn a_crash_between_the_vault_and_the_store_commit_rolls_the_vault_back() {
    crash_and_recover(CrashAt::BetweenCommits);
}

#[test]
fn a_crash_after_both_commits_keeps_the_unit() {
    crash_and_recover(CrashAt::AfterStoreCommit);
}

#[test]
fn req_ker_6_a_repeated_posting_is_refused_after_a_file_store_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut book = Book::bootstrap(
            FileStore::open(dir.path().join("book.redb")).unwrap(),
            FileVault::open(dir.path().join("vault.redb")).unwrap(),
            KeyRing::from_root(ROOT),
        )
        .unwrap();
        fund(&mut book, 100);
        assert!(book.apply(&the_unit(), 7_000).unwrap().steps[0].applied);
    }
    for seed in [8_000, 9_000] {
        let mut book = open_files(dir.path());
        let again = book.apply(&the_unit(), seed).unwrap();
        assert_eq!(again.steps[0].events, vec![EventClass::Duplicate]);
        assert_eq!(again.seq, None);
    }
    let book = open_files(dir.path());
    assert_eq!(book.settled_net(&customer(2, 1).id()).unwrap(), Some(30));
}

/// A store whose commit fails without a crash.
struct FailingStore(MemStore);

impl LeafStore for FailingStore {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        self.0.subroots()
    }
    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        self.0.load(keys)
    }
    fn commit(&mut self, _: u64, _: &BookDelta) -> Result<SubRoots, StoreError> {
        Err(StoreError::Io("disk full".into()))
    }
    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.0.last_committed()
    }
    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        self.0.next_posting_seq()
    }
    fn log_record(&self, seq: u64) -> Result<Option<crate::store::LogRecord>, StoreError> {
        self.0.log_record(seq)
    }
}

#[test]
fn a_failed_store_commit_reverts_the_vault_in_process() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let (store, vault, keys) = book.into_parts();
    let mut book = Book::open(FailingStore(store), vault, keys).unwrap();
    let r = book.apply(&the_unit(), 7_000);
    assert_eq!(
        r,
        Err(BindingFault::Store(StoreError::Io("disk full".into())))
    );
    assert_eq!(book.vault().last_committed(), Some(1));
    let (FailingStore(store), vault, keys) = book.into_parts();
    let mut book: Book<MemStore, MemVault> = Book::open(store, vault, keys).unwrap();
    book.verify_openings().unwrap();
    assert_eq!(book.apply(&the_unit(), 7_000).unwrap().seq, Some(2));
}

#[test]
fn a_vault_ahead_by_more_than_one_commit_is_refused() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let (store, mut vault, keys) = book.into_parts();
    // The vault commits twice more than the store ever saw.
    let mut v = crate::vault::OpeningVault::open(vault.clone()).unwrap();
    v.commit(2, &Default::default()).unwrap();
    v.commit(3, &Default::default()).unwrap();
    vault = v.into_backend();
    assert_eq!(
        Book::open(store, vault, keys).err(),
        Some(BindingFault::VaultDiverged {
            vault: Some(3),
            store: Some(1)
        })
    );
}
