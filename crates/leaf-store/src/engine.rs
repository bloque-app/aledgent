// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The storage engine, behind one trait (`F-006` §Storage engine, T-006-02).
//!
//! The plan's Proposal is an embedded ordered key-value store with single-writer ACID
//! transactions and snapshots, such as `redb`; its Alternative is RocksDB. The choice is revisited
//! with `F-002`'s numbers before `M-3`, so everything above this module sees only [`Engine`]:
//!
//! - a [`Snapshot`] is a consistent, read-only view of the whole store, pinned when taken;
//! - a [`WriteBatch`] is a set of puts and deletes, applied atomically by [`Engine::write`], with
//!   a [`Durability`] that says whether the write is on disk when the call returns.
//!
//! The store is one ordered keyspace. Every key starts with a [`Space`] byte, so each kind of row
//! sorts together and a canonical dump is one scan in key order.
//!
//! [`Engine::stats`] counts point reads, range reads and written rows, so tests can show that the
//! work of one apply does not grow with the book (`F-006` T-006-10, issue #37).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

/// The kinds of row, each its own key range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Space {
    /// Leaves: `tree ‖ key` → `leaf hash ‖ stored bytes`.
    Leaf,
    /// Branch nodes of the compressed trees: `tree ‖ level ‖ prefix` → both children.
    Node,
    /// Each tree's top node and root: `tree` → `top ‖ root`.
    Top,
    /// Book-wide values: applied sequence, log head, next posting sequence.
    Meta,
    /// The global posting sequence: `posting seq` → transfer id (`REQ-PO-2`).
    Posting,
    /// Entries by account: `kernel account ‖ posting seq ‖ entry index` → side and layer.
    History,
    /// Ledger accounts by identity: `identity ‖ account` → nothing.
    Owned,
    /// Authority holders by account: `account ‖ holder ‖ grant` → nothing (`INV-16`).
    Holder,
    /// Reserved totals: `account ‖ asset` → minor units (`REQ-BA-6`).
    Reserved,
}

impl Space {
    /// Every space, in key order.
    pub const ALL: [Space; 9] = [
        Space::Leaf,
        Space::Node,
        Space::Top,
        Space::Meta,
        Space::Posting,
        Space::History,
        Space::Owned,
        Space::Holder,
        Space::Reserved,
    ];

    /// The key's first byte.
    pub const fn byte(self) -> u8 {
        match self {
            Space::Leaf => 1,
            Space::Node => 2,
            Space::Top => 3,
            Space::Meta => 4,
            Space::Posting => 5,
            Space::History => 6,
            Space::Owned => 7,
            Space::Holder => 8,
            Space::Reserved => 9,
        }
    }

    /// The space whose first byte is `b`.
    pub fn from_byte(b: u8) -> Option<Space> {
        Space::ALL.into_iter().find(|s| s.byte() == b)
    }

    /// Whether rows of this space are indexes, recomputable from leaves and postings.
    pub const fn is_index(self) -> bool {
        matches!(
            self,
            Space::History | Space::Owned | Space::Holder | Space::Reserved
        )
    }
}

/// The full key of a row: space byte, then the key.
pub fn full_key(space: Space, key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + key.len());
    k.push(space.byte());
    k.extend_from_slice(key);
    k
}

/// The engine failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineError(pub String);

impl EngineError {
    fn from(e: impl core::fmt::Display) -> Self {
        EngineError(e.to_string())
    }
}

/// Whether a write is on disk when [`Engine::write`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// Fsynced before the call returns.
    Durable,
    /// Visible to every snapshot taken after the call, and made durable by the next durable
    /// write. Lost on a crash: the store then catches up from the delta archive.
    Visible,
}

/// Puts and deletes, applied together or not at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteBatch {
    rows: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl WriteBatch {
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Put a row.
    pub fn put(&mut self, space: Space, key: &[u8], value: Vec<u8>) {
        self.rows.insert(full_key(space, key), Some(value));
    }

    /// Delete a row. The store only ever deletes index rows; it has no path that deletes a leaf
    /// (`INV-2`).
    pub fn delete(&mut self, space: Space, key: &[u8]) {
        self.rows.insert(full_key(space, key), None);
    }

    /// What the batch says about a row: `None` if it does not touch it, `Some(None)` if it
    /// deletes it.
    pub fn pending(&self, space: Space, key: &[u8]) -> Option<Option<&[u8]>> {
        self.rows.get(&full_key(space, key)).map(|v| v.as_deref())
    }

    /// The number of rows the batch writes.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the batch writes nothing.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Counters of engine work, for the flat-cost tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Point reads.
    pub gets: u64,
    /// Range reads.
    pub ranges: u64,
    /// Rows written or deleted.
    pub writes: u64,
}

impl core::ops::Sub for Stats {
    type Output = Stats;
    fn sub(self, o: Stats) -> Stats {
        Stats {
            gets: self.gets - o.gets,
            ranges: self.ranges - o.ranges,
            writes: self.writes - o.writes,
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    gets: AtomicU64,
    ranges: AtomicU64,
    writes: AtomicU64,
}

/// Rows as `(key, value)` pairs.
pub type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// A visitor of rows, with their full keys.
pub type Visit<'a> = dyn FnMut(&[u8], &[u8]) -> Result<(), EngineError> + 'a;

/// A consistent read-only view of the whole store.
pub trait Snapshot {
    /// One row.
    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, EngineError>;

    /// Up to `limit` rows of `space` whose key starts with `prefix`, in key order, starting
    /// strictly after `after` when given.
    fn scan(
        &self,
        space: Space,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Rows, EngineError>;

    /// Visit every row of the store in key order, with its full key.
    fn for_each(&self, visit: &mut Visit<'_>) -> Result<(), EngineError>;
}

/// A storage engine (module documentation).
pub trait Engine {
    /// Its snapshots.
    type Snapshot: Snapshot;

    /// A snapshot of everything written so far.
    fn snapshot(&self) -> Result<Self::Snapshot, EngineError>;

    /// Apply `batch` atomically.
    fn write(&self, batch: WriteBatch, durability: Durability) -> Result<(), EngineError>;

    /// Make every earlier [`Durability::Visible`] write durable.
    fn flush(&self) -> Result<(), EngineError> {
        self.write(WriteBatch::new(), Durability::Durable)
    }

    /// The work counters so far.
    fn stats(&self) -> Stats;
}

type Blob = &'static [u8];
const KV: TableDefinition<Blob, Blob> = TableDefinition::new("kv");

/// The Proposal engine: one redb table holding every row.
pub struct RedbEngine {
    db: Database,
    counters: Arc<Counters>,
    /// Every this many [`Durability::Visible`] writes, one is made durable instead (0: never).
    /// redb keeps an unsynced commit's freed-page records in memory until a synced commit writes
    /// them out (`T-031-14`).
    sync_every: u64,
    writes: std::sync::atomic::AtomicU64,
    /// Whether a synced write saves redb's allocator state with it (quick repair, `T-031-26`): a
    /// file killed after one then reopens without a walk of the whole file. Each synced write
    /// costs a second sync; only engines whose synced writes are rare set it.
    quick_repair: bool,
}

impl core::fmt::Debug for RedbEngine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RedbEngine")
    }
}

impl RedbEngine {
    /// Open the engine's file at `path`, creating it if absent.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, EngineError> {
        Self::init(Database::create(path).map_err(EngineError::from)?)
    }

    /// Open the engine's file at `path`, creating it if absent, with at most `cache_bytes` of it
    /// cached in memory (redb's default is 1 GiB): a replica that must not grow with the book
    /// (`T-031-14`). One [`Durability::Visible`] write in 1 024 is made durable, so that redb lets
    /// go of what the others freed. Synced writes are rare here, so each saves the allocator state
    /// (quick repair, `T-031-26`): the replica's file, killed, reopens without a repair of the
    /// whole file.
    pub fn open_cached(path: impl AsRef<Path>, cache_bytes: usize) -> Result<Self, EngineError> {
        let mut engine = Self::init(
            Database::builder()
                .set_cache_size(cache_bytes)
                .create(path)
                .map_err(EngineError::from)?,
        )?;
        engine.sync_every = 1024;
        engine.quick_repair = true;
        Ok(engine)
    }

    /// An engine on redb's in-memory backend: the same code, no file.
    pub fn in_memory() -> Result<Self, EngineError> {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(EngineError::from)?;
        Self::init(db)
    }

    /// An engine over any redb storage backend.
    pub fn with_backend(backend: impl redb::StorageBackend) -> Result<Self, EngineError> {
        let db = Database::builder()
            .create_with_backend(backend)
            .map_err(EngineError::from)?;
        Self::init(db)
    }

    fn init(db: Database) -> Result<Self, EngineError> {
        let txn = db.begin_write().map_err(EngineError::from)?;
        txn.open_table(KV).map_err(EngineError::from)?;
        txn.commit().map_err(EngineError::from)?;
        Ok(Self {
            db,
            counters: Arc::new(Counters::default()),
            sync_every: 0,
            writes: std::sync::atomic::AtomicU64::new(0),
            quick_repair: false,
        })
    }
}

impl RedbEngine {
    /// Make every write so far durable and record a savepoint of it, deleting every other
    /// savepoint but those in `keep` ([`crate::savepoint::take`], `T-031-26`).
    pub fn savepoint(&self, keep: &[u64]) -> Result<u64, EngineError> {
        crate::savepoint::take(&self.db, keep).map_err(EngineError)
    }

    /// Put the file back to savepoint `id` ([`crate::savepoint::restore`]). A store built over
    /// this engine afterwards reads the restored state; one built before must not be used.
    pub fn restore_savepoint(&self, id: u64) -> Result<(), EngineError> {
        crate::savepoint::restore(&self.db, id).map_err(EngineError)
    }

    /// The savepoints the file holds, oldest first.
    pub fn savepoints(&self) -> Result<Vec<u64>, EngineError> {
        crate::savepoint::list(&self.db).map_err(EngineError)
    }

    /// Delete every savepoint, durably ([`crate::savepoint::clear`]).
    pub fn clear_savepoints(&self) -> Result<(), EngineError> {
        crate::savepoint::clear(&self.db).map_err(EngineError)
    }
}

/// A redb read transaction's view of the table.
pub struct RedbSnapshot {
    table: redb::ReadOnlyTable<Blob, Blob>,
    counters: Arc<Counters>,
}

impl core::fmt::Debug for RedbSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RedbSnapshot")
    }
}

/// The smallest key greater than every key starting with `prefix`, if there is one.
fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while let Some(last) = p.pop() {
        if last != 0xFF {
            p.push(last + 1);
            return Some(p);
        }
    }
    None
}

impl Snapshot for RedbSnapshot {
    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, EngineError> {
        self.counters.gets.fetch_add(1, Ordering::Relaxed);
        let k = full_key(space, key);
        Ok(self
            .table
            .get(k.as_slice())
            .map_err(EngineError::from)?
            .map(|g| g.value().to_vec()))
    }

    fn scan(
        &self,
        space: Space,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Rows, EngineError> {
        self.counters.ranges.fetch_add(1, Ordering::Relaxed);
        let start = full_key(space, prefix);
        let end = successor(&start);
        let from = match after {
            Some(a) => {
                let mut k = full_key(space, a);
                // Strictly after `a`: the smallest key greater than it.
                k.push(0);
                if k < start { start.clone() } else { k }
            }
            None => start.clone(),
        };
        let mut out = Vec::new();
        let iter = match &end {
            Some(e) => self
                .table
                .range::<&[u8]>(from.as_slice()..e.as_slice())
                .map_err(EngineError::from)?,
            None => self
                .table
                .range::<&[u8]>(from.as_slice()..)
                .map_err(EngineError::from)?,
        };
        for item in iter {
            if out.len() >= limit {
                break;
            }
            let (k, v) = item.map_err(EngineError::from)?;
            out.push((k.value()[1..].to_vec(), v.value().to_vec()));
        }
        Ok(out)
    }

    fn for_each(&self, visit: &mut Visit<'_>) -> Result<(), EngineError> {
        self.counters.ranges.fetch_add(1, Ordering::Relaxed);
        for item in self.table.iter().map_err(EngineError::from)? {
            let (k, v) = item.map_err(EngineError::from)?;
            visit(k.value(), v.value())?;
        }
        Ok(())
    }
}

impl Engine for RedbEngine {
    type Snapshot = RedbSnapshot;

    fn snapshot(&self) -> Result<RedbSnapshot, EngineError> {
        let txn = self.db.begin_read().map_err(EngineError::from)?;
        Ok(RedbSnapshot {
            table: txn.open_table(KV).map_err(EngineError::from)?,
            counters: self.counters.clone(),
        })
    }

    fn write(&self, batch: WriteBatch, durability: Durability) -> Result<(), EngineError> {
        let mut txn = self.db.begin_write().map_err(EngineError::from)?;
        let n = self
            .writes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let synced = self.sync_every > 0 && n % self.sync_every == 0;
        let immediate = durability == Durability::Durable || synced;
        txn.set_durability(if immediate {
            redb::Durability::Immediate
        } else {
            redb::Durability::None
        })
        .map_err(EngineError::from)?;
        if immediate && self.quick_repair {
            txn.set_quick_repair(true);
        }
        {
            let mut table = txn.open_table(KV).map_err(EngineError::from)?;
            for (k, v) in &batch.rows {
                match v {
                    Some(v) => {
                        table
                            .insert(k.as_slice(), v.as_slice())
                            .map_err(EngineError::from)?;
                    }
                    None => {
                        table.remove(k.as_slice()).map_err(EngineError::from)?;
                    }
                }
            }
        }
        txn.commit().map_err(EngineError::from)?;
        self.counters
            .writes
            .fetch_add(batch.rows.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn stats(&self) -> Stats {
        Stats {
            gets: self.counters.gets.load(Ordering::Relaxed),
            ranges: self.counters.ranges.load(Ordering::Relaxed),
            writes: self.counters.writes.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_atomic_and_snapshots_are_pinned() {
        let e = RedbEngine::in_memory().unwrap();
        let mut b = WriteBatch::new();
        b.put(Space::Meta, b"a", b"1".to_vec());
        b.put(Space::History, b"x1", b"".to_vec());
        b.put(Space::History, b"x2", b"".to_vec());
        b.put(Space::History, b"y1", b"".to_vec());
        e.write(b, Durability::Visible).unwrap();
        let before = e.snapshot().unwrap();
        let mut b = WriteBatch::new();
        b.put(Space::Meta, b"a", b"2".to_vec());
        b.delete(Space::History, b"x1");
        e.write(b, Durability::Durable).unwrap();
        let after = e.snapshot().unwrap();
        assert_eq!(before.get(Space::Meta, b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(after.get(Space::Meta, b"a").unwrap(), Some(b"2".to_vec()));
        let keys = |s: &RedbSnapshot, after: Option<&[u8]>| -> Vec<Vec<u8>> {
            s.scan(Space::History, b"x", after, 10)
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        assert_eq!(keys(&before, None), vec![b"x1".to_vec(), b"x2".to_vec()]);
        assert_eq!(keys(&after, None), vec![b"x2".to_vec()]);
        assert_eq!(keys(&before, Some(b"x1")), vec![b"x2".to_vec()]);
        assert_eq!(keys(&before, Some(b"x2")), Vec::<Vec<u8>>::new());
        assert!(e.stats().writes >= 5);
    }

    /// T-031-26: a savepoint taken between unsynced writes is where a copy of the file, as a kill
    /// leaves it, is put back to; the savepoints a file holds stay bounded.
    #[test]
    fn t031_26_a_file_left_by_a_kill_is_put_back_to_its_savepoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leaves.redb");
        let e = RedbEngine::open_cached(&path, 1 << 20).unwrap();
        let put = |e: &RedbEngine, k: &[u8], v: &[u8], d: Durability| {
            let mut b = WriteBatch::new();
            b.put(Space::Meta, k, v.to_vec());
            e.write(b, d).unwrap();
        };
        put(&e, b"a", b"1", Durability::Visible);
        let first = e.savepoint(&[]).unwrap();
        put(&e, b"a", b"2", Durability::Visible);
        let second = e.savepoint(&[first]).unwrap();
        assert_eq!(e.savepoints().unwrap(), vec![first, second]);
        // The next one keeps only the one named.
        put(&e, b"a", b"3", Durability::Visible);
        let third = e.savepoint(&[second]).unwrap();
        assert_eq!(e.savepoints().unwrap(), vec![second, third]);
        // Past it: unsynced writes, and a synced one.
        put(&e, b"a", b"4", Durability::Visible);
        put(&e, b"b", b"x", Durability::Durable);
        put(&e, b"a", b"5", Durability::Visible);
        // The kill: the file as the process left it, still open.
        let copy = dir.path().join("killed.redb");
        std::fs::copy(&path, &copy).unwrap();
        let k = RedbEngine::open_cached(&copy, 1 << 20).unwrap();
        let read =
            |e: &RedbEngine, key: &[u8]| e.snapshot().unwrap().get(Space::Meta, key).unwrap();
        assert_eq!(
            read(&k, b"b"),
            Some(b"x".to_vec()),
            "the synced write survived"
        );
        k.restore_savepoint(third).unwrap();
        assert_eq!(read(&k, b"a"), Some(b"3".to_vec()));
        assert_eq!(read(&k, b"b"), None);
        // Back further, and no way forward again.
        k.restore_savepoint(second).unwrap();
        assert_eq!(read(&k, b"a"), Some(b"2".to_vec()));
        assert!(k.restore_savepoint(third).is_err());
        assert!(k.restore_savepoint(99).is_err());
        k.clear_savepoints().unwrap();
        assert!(k.savepoints().unwrap().is_empty());
        drop(k);
        // Reopened, the restore holds.
        let k = RedbEngine::open_cached(&copy, 1 << 20).unwrap();
        assert_eq!(read(&k, b"a"), Some(b"2".to_vec()));
    }

    #[test]
    fn successor_bounds_a_prefix() {
        assert_eq!(successor(&[1, 2]), Some(vec![1, 3]));
        assert_eq!(successor(&[1, 0xFF]), Some(vec![2]));
        assert_eq!(successor(&[0xFF]), None);
    }
}
