// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `FileStore`: the book's leaves in one redb file (`F-001` §10).
//!
//! It is the reference for durability semantics until `F-006` chooses the
//! production engine. One file holds:
//!
//! - a leaf table per sub-tree, keyed by the 16-byte leaf key;
//! - a node table keyed by `(sub-tree, depth, prefix)`, updated along the
//!   path of every written leaf, so roots are read, not recomputed. It holds
//!   every leaf's hash and the nodes down to [`FILE_NODES_DEPTH`]; a deeper node
//!   is computed from the leaves under it (`T-031-19`, as `MemStore`'s table
//!   does since `T-031-14`). A file written before `T-031-19` holds every node
//!   and reads the same; a file written since cannot be read by an earlier
//!   build;
//! - the append-only delta log, keyed by commit sequence number;
//! - a small meta table (the posting sequence).
//!
//! Every commit is one redb write transaction, made durable (fsynced) before
//! `commit` returns. Inside it, each node of the commit's paths is written
//! once, at the end, however many of the commit's leaves share it. The write rules are checked first, against a read
//! snapshot, so a refused commit writes nothing.

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use std::path::Path;

use cipher_clerk::ids::ExternalId;
use cipher_clerk::merkle::{SMT_DEPTH, smt_leaf_hash};
use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::{
    account_leaf_content, external_id_key, external_id_leaf_content, journal_leaf_content,
    pending_leaf_content, transfer_leaf_content, voided_leaf_content,
};
use cipher_clerk::{Account, AccountId, TransferId};
use redb::{
    Database, Durability, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition,
};

use super::codec::{
    decode_account, decode_journal, decode_log_record, decode_pending, decode_transfer,
    encode_account, encode_journal, encode_log_record, encode_transfer,
};
use super::rules::{self, RuleView};
use super::smt::{self, EmptyChain, Neighbours, NodeAddr, NodeTable};
use super::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, StoreError, SubRoots, SubTree, composite_root,
};

type Blob = &'static [u8];

const ACCOUNTS: TableDefinition<Blob, Blob> = TableDefinition::new("leaves/accounts");
const TRANSFERS: TableDefinition<Blob, Blob> = TableDefinition::new("leaves/transfers");
const JOURNALS: TableDefinition<Blob, Blob> = TableDefinition::new("leaves/journals");
const EXTERNAL_IDS: TableDefinition<Blob, Blob> = TableDefinition::new("leaves/external-ids");
const VOIDED: TableDefinition<Blob, Blob> = TableDefinition::new("leaves/voided");
const PENDING: TableDefinition<Blob, Blob> = TableDefinition::new("leaves/pending");
const NODES: TableDefinition<Blob, Blob> = TableDefinition::new("nodes");
const LOG: TableDefinition<u64, Blob> = TableDefinition::new("delta-log");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const NEXT_POSTING_SEQ: &str = "next-posting-seq";

fn leaf_table(t: SubTree) -> TableDefinition<'static, Blob, Blob> {
    match t {
        SubTree::Accounts => ACCOUNTS,
        SubTree::Transfers => TRANSFERS,
        SubTree::Journals => JOURNALS,
        SubTree::ExternalIds => EXTERNAL_IDS,
        SubTree::Voided => VOIDED,
        SubTree::Pending => PENDING,
    }
}

fn io(e: impl core::fmt::Display) -> StoreError {
    StoreError::Io(e.to_string())
}

fn node_key(tree: SubTree, addr: &NodeAddr) -> [u8; 18] {
    let mut k = [0u8; 18];
    k[0] = tree.index() as u8;
    k[1..].copy_from_slice(&addr.to_bytes());
    k
}

fn array32(bytes: &[u8]) -> Result<[u8; 32], StoreError> {
    bytes
        .try_into()
        .map_err(|_| StoreError::Corrupt(format!("expected 32 bytes, got {}", bytes.len())))
}

/// The book's leaves in one redb file.
pub struct FileStore {
    db: Database,
    empty: EmptyChain,
    durability: Durability,
    profile: CommitProfile,
}

/// Where the last [`FileStore`] commit spent its time (`T-031-19`): an observation for the
/// measurements (`notes/MEASUREMENTS-F031.md`), never an input to a decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitProfile {
    /// The write rules, against a read snapshot.
    pub rules_ns: u64,
    /// Encoding the leaf records and inserting them.
    pub leaves_ns: u64,
    /// The leaves' paths: sibling reads, hashing and node writes.
    pub nodes_ns: u64,
    /// The roots read back, the delta-log record encoded and inserted, the posting sequence.
    pub log_ns: u64,
    /// The transaction's commit: the fsync when the commit is durable.
    pub durable_ns: u64,
    /// Leaves written.
    pub leaves: u64,
    /// Node reads (siblings and range scans).
    pub node_reads: u64,
    /// Node writes.
    pub node_writes: u64,
    /// Bytes of the delta-log record.
    pub log_bytes: u64,
}

fn ns_since(t: std::time::Instant) -> u64 {
    u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// How often an unsynced store syncs a commit anyway: redb keeps an unsynced commit's freed-page
/// records in memory until a synced commit writes them out, so a store that never synced would
/// grow in memory with every commit (`T-031-14`).
const SYNC_EVERY: u64 = 1024;

/// How a [`FileStore`] opened with [`FileStore::open_with`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStoreOptions {
    /// Whether each commit is durable (fsynced) before `commit` returns. `false` only for a
    /// store that is rebuilt from a journal of its inputs on every start (the in-process core,
    /// `F-009`): a commit is visible to every later read at once, and nothing is promised after
    /// a crash. One commit in 1 024 is synced anyway, so that redb can let go of what the unsynced
    /// ones freed.
    pub durable: bool,
    /// The page cache redb keeps in memory, in bytes: what the store holds in memory at most,
    /// whatever the size of the book.
    pub cache_bytes: usize,
}

impl Default for FileStoreOptions {
    /// What [`FileStore::open`] uses: durable commits, redb's default cache (1 GiB).
    fn default() -> Self {
        Self {
            durable: true,
            cache_bytes: 1024 * 1024 * 1024,
        }
    }
}

impl core::fmt::Debug for FileStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("FileStore")
    }
}

impl FileStore {
    /// Open the store at `path`, creating the file if it does not exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::init(Database::create(path).map_err(io)?, Durability::Immediate)
    }

    /// Open the store at `path` with `options`, creating the file if it does not exist.
    pub fn open_with(
        path: impl AsRef<Path>,
        options: FileStoreOptions,
    ) -> Result<Self, StoreError> {
        let db = Database::builder()
            .set_cache_size(options.cache_bytes)
            .create(path)
            .map_err(io)?;
        let durability = if options.durable {
            Durability::Immediate
        } else {
            Durability::None
        };
        Self::init(db, durability)
    }

    /// A store on redb's in-memory backend: the same code and rules, no file.
    /// For tests that need many stores quickly.
    pub fn in_memory() -> Result<Self, StoreError> {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(io)?;
        Self::init(db, Durability::Immediate)
    }

    fn init(db: Database, durability: Durability) -> Result<Self, StoreError> {
        let txn = db.begin_write().map_err(io)?;
        for t in SubTree::ALL {
            txn.open_table(leaf_table(t)).map_err(io)?;
        }
        txn.open_table(NODES).map_err(io)?;
        txn.open_table(LOG).map_err(io)?;
        txn.open_table(META).map_err(io)?;
        txn.commit().map_err(io)?;
        Ok(Self {
            db,
            empty: smt::empty_chain(),
            durability,
            profile: CommitProfile::default(),
        })
    }

    /// Where the last commit spent its time (`T-031-19`).
    pub fn last_profile(&self) -> CommitProfile {
        self.profile
    }

    fn read(&self) -> Result<ReadTransaction, StoreError> {
        self.db.begin_read().map_err(io)
    }

    fn leaf(
        &self,
        txn: &ReadTransaction,
        t: SubTree,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let table = txn.open_table(leaf_table(t)).map_err(io)?;
        let got = table.get(key).map_err(io)?;
        Ok(got.map(|g| g.value().to_vec()))
    }

    /// The node hash stored at `(tree, depth 128, key)`: the leaf's hash.
    fn leaf_hash(
        &self,
        txn: &ReadTransaction,
        t: SubTree,
        key: &[u8; 16],
    ) -> Result<Option<[u8; 32]>, StoreError> {
        let table = txn.open_table(NODES).map_err(io)?;
        let k = node_key(t, &NodeAddr::above(key, SMT_DEPTH));
        match table.get(k.as_slice()).map_err(io)? {
            Some(g) => Ok(Some(array32(g.value())?)),
            None => Ok(None),
        }
    }

    /// Check that stored leaf content hashes to the node committed for it.
    fn verified(
        &self,
        txn: &ReadTransaction,
        t: SubTree,
        key: &[u8; 16],
        content: &[u8],
    ) -> Result<(), StoreError> {
        if self.leaf_hash(txn, t, key)? != Some(smt_leaf_hash(content)) {
            return Err(StoreError::Corrupt(format!(
                "{t:?} leaf {key:02x?} does not match its node"
            )));
        }
        Ok(())
    }

    fn meta(&self, txn: &ReadTransaction, name: &str) -> Result<Option<u64>, StoreError> {
        let table = txn.open_table(META).map_err(io)?;
        Ok(table.get(name).map_err(io)?.map(|g| g.value()))
    }

    fn last_seq(&self, txn: &ReadTransaction) -> Result<Option<u64>, StoreError> {
        let table = txn.open_table(LOG).map_err(io)?;
        Ok(table.last().map_err(io)?.map(|(k, _)| k.value()))
    }
}

/// The deepest level a [`FileStore`] stores; deeper nodes are computed from the leaves under
/// them (`T-031-19`). The same cut as `MemStore`'s ([`smt::MEM_NODES_DEPTH`]): below it a
/// subtree of a book of fewer than 2³² leaves almost always holds one leaf, and a path's nodes
/// there are a chain that leaf alone determines.
pub const FILE_NODES_DEPTH: usize = smt::MEM_NODES_DEPTH;

/// Leaves' keys and hashes, in key order.
type LeafHashes = Vec<([u8; 16], [u8; 32])>;

/// The node table inside one commit's write transaction (`T-031-19`). Leaves' hashes are
/// written through, so range reads see them; the nodes down to [`FILE_NODES_DEPTH`] are kept in
/// memory and written once each by [`TxnNodes::flush`]; deeper nodes are not stored.
struct TxnNodes<'t> {
    table: redb::Table<'t, Blob, Blob>,
    empty: &'t EmptyChain,
    tree: SubTree,
    dirty: alloc::collections::BTreeMap<[u8; 18], [u8; 32]>,
    reads: core::cell::Cell<u64>,
    writes: u64,
}

impl TxnNodes<'_> {
    fn bump(&self) {
        self.reads.set(self.reads.get() + 1);
    }

    /// The leaves of the current tree whose keys lie in `lo ..= hi`, with their hashes.
    fn leaves_in(&self, lo: &[u8; 16], hi: &[u8; 16]) -> Result<LeafHashes, StoreError> {
        self.bump();
        let lo = node_key(self.tree, &NodeAddr::above(lo, SMT_DEPTH));
        let hi = node_key(self.tree, &NodeAddr::above(hi, SMT_DEPTH));
        let mut out = Vec::new();
        for row in self
            .table
            .range::<&[u8]>(lo.as_slice()..=hi.as_slice())
            .map_err(io)?
        {
            let (k, v) = row.map_err(io)?;
            let mut key = [0u8; 16];
            key.copy_from_slice(&k.value()[2..]);
            out.push((key, array32(v.value())?));
        }
        Ok(out)
    }

    /// Write every kept node once.
    fn flush(&mut self) -> Result<(), StoreError> {
        for (k, h) in core::mem::take(&mut self.dirty) {
            self.table.insert(k.as_slice(), h.as_slice()).map_err(io)?;
            self.writes += 1;
        }
        Ok(())
    }
}

impl NodeTable for TxnNodes<'_> {
    type Error = StoreError;
    fn get(&self, addr: &NodeAddr) -> Result<Option<[u8; 32]>, StoreError> {
        let depth = usize::from(addr.depth);
        let k = node_key(self.tree, addr);
        if depth <= FILE_NODES_DEPTH
            && let Some(h) = self.dirty.get(&k)
        {
            return Ok(Some(*h));
        }
        if depth <= FILE_NODES_DEPTH || depth == SMT_DEPTH {
            self.bump();
            return match self.table.get(k.as_slice()).map_err(io)? {
                Some(g) => Ok(Some(array32(g.value())?)),
                None => Ok(None),
            };
        }
        let under = self.leaves_in(&addr.prefix, &smt::last_under(&addr.prefix, depth))?;
        Ok((!under.is_empty())
            .then(|| smt::subtree_hash(compact_smt::Version::ACTIVE, self.empty, depth, &under)))
    }
    fn put(&mut self, addr: &NodeAddr, hash: [u8; 32]) -> Result<(), StoreError> {
        let depth = usize::from(addr.depth);
        let k = node_key(self.tree, addr);
        if depth == SMT_DEPTH {
            self.table
                .insert(k.as_slice(), hash.as_slice())
                .map_err(io)?;
            self.writes += 1;
        } else if depth <= FILE_NODES_DEPTH {
            self.dirty.insert(k, hash);
        }
        Ok(())
    }
    fn neighbours(&self, key: &[u8; 16]) -> Result<Neighbours, StoreError> {
        self.bump();
        let lo = node_key(self.tree, &NodeAddr::above(&[0; 16], SMT_DEPTH));
        let at = node_key(self.tree, &NodeAddr::above(key, SMT_DEPTH));
        let hi = node_key(self.tree, &NodeAddr::above(&[0xFF; 16], SMT_DEPTH));
        let leaf_key = |k: &[u8]| {
            let mut out = [0u8; 16];
            out.copy_from_slice(&k[2..]);
            out
        };
        let before = match self
            .table
            .range::<&[u8]>(lo.as_slice()..at.as_slice())
            .map_err(io)?
            .next_back()
        {
            Some(row) => Some(leaf_key(row.map_err(io)?.0.value())),
            None => None,
        };
        let after = match self
            .table
            .range::<&[u8]>((
                core::ops::Bound::Excluded(at.as_slice()),
                core::ops::Bound::Included(hi.as_slice()),
            ))
            .map_err(io)?
            .next()
        {
            Some(row) => Some(leaf_key(row.map_err(io)?.0.value())),
            None => None,
        };
        Ok(smt::neighbours_of(key, before.as_ref(), after.as_ref()))
    }
    fn present_under(&self, prefix: &[u8; 16], depth: usize) -> Result<Option<usize>, StoreError> {
        let under = self.leaves_in(prefix, &smt::last_under(prefix, depth))?;
        Ok(Some(
            under
                .iter()
                .filter(|(_, h)| *h != cipher_clerk::merkle::SMT_EMPTY_LEAF)
                .take(2)
                .count(),
        ))
    }
}

/// Write one leaf: its record, and its path in the node table.
fn write_leaf(
    leaves: &mut redb::Table<'_, Blob, Blob>,
    nodes: &mut TxnNodes<'_>,
    profile: &mut CommitProfile,
    key: &[u8; 16],
    stored: &[u8],
    content: &[u8],
) -> Result<(), StoreError> {
    let t = std::time::Instant::now();
    leaves.insert(key.as_slice(), stored).map_err(io)?;
    profile.leaves_ns += ns_since(t);
    profile.leaves += 1;
    let t = std::time::Instant::now();
    let empty = nodes.empty;
    smt::set_leaf(nodes, empty, key, smt_leaf_hash(content))?;
    profile.nodes_ns += ns_since(t);
    Ok(())
}

/// The rules' view of the store: a read snapshot taken before the write.
struct Snapshot<'a> {
    store: &'a FileStore,
    txn: ReadTransaction,
}

impl RuleView for Snapshot<'_> {
    fn expected_seq(&self) -> Result<u64, StoreError> {
        Ok(self.store.last_seq(&self.txn)?.map_or(0, |n| n + 1))
    }
    fn expected_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.store.meta(&self.txn, NEXT_POSTING_SEQ)?.unwrap_or(0))
    }
    fn stored_account(&self, id: &AccountId) -> Result<Option<Account>, StoreError> {
        match self.store.leaf(&self.txn, SubTree::Accounts, &id.0)? {
            Some(b) => Ok(Some(decode_account(&b)?)),
            None => Ok(None),
        }
    }
    fn transfer_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        Ok(self
            .store
            .leaf(&self.txn, SubTree::Transfers, &id.0)?
            .is_some())
    }
    fn journal_stored(&self) -> Result<bool, StoreError> {
        let table = self.txn.open_table(JOURNALS).map_err(io)?;
        Ok(table.first().map_err(io)?.is_some())
    }
    fn external_id_in_slot(&self, slot: &[u8; 16]) -> Result<Option<[u8; 32]>, StoreError> {
        match self.store.leaf(&self.txn, SubTree::ExternalIds, slot)? {
            Some(b) => Ok(Some(array32(&b)?)),
            None => Ok(None),
        }
    }
    fn voided_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        Ok(self
            .store
            .leaf(&self.txn, SubTree::Voided, &id.0)?
            .is_some())
    }
    fn pending_stored(&self, id: &TransferId) -> Result<Option<PendingStatus>, StoreError> {
        match self.store.leaf(&self.txn, SubTree::Pending, &id.0)? {
            Some(b) => match b.as_slice() {
                [code] => Ok(Some(decode_pending(*code)?)),
                _ => Err(StoreError::Corrupt("pending leaf length".to_string())),
            },
            None => Ok(None),
        }
    }
}

impl LeafStore for FileStore {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        let txn = self.read()?;
        let table = txn.open_table(NODES).map_err(io)?;
        let mut out = [compact_smt::Version::ACTIVE.empty_root(); 6];
        for t in SubTree::ALL {
            let k = node_key(
                t,
                &NodeAddr {
                    depth: 0,
                    prefix: [0; 16],
                },
            );
            if let Some(g) = table.get(k.as_slice()).map_err(io)? {
                out[t.index()] = array32(g.value())?;
            }
        }
        Ok(out)
    }

    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        let txn = self.read()?;
        let mut out = Loaded::default();
        for id in &keys.accounts {
            let a = match self.leaf(&txn, SubTree::Accounts, &id.0)? {
                Some(b) => {
                    let a = decode_account(&b)?;
                    self.verified(&txn, SubTree::Accounts, &id.0, &account_leaf_content(&a))?;
                    Some(a)
                }
                None => None,
            };
            out.accounts.insert(*id, a);
        }
        for id in &keys.transfers {
            let t = match self.leaf(&txn, SubTree::Transfers, &id.0)? {
                Some(b) => {
                    let t = decode_transfer(&b)?;
                    self.verified(&txn, SubTree::Transfers, &id.0, &transfer_leaf_content(&t))?;
                    Some(t)
                }
                None => None,
            };
            out.transfers.insert(*id, t);
        }
        for id in &keys.journals {
            let j = match self.leaf(&txn, SubTree::Journals, &id.0)? {
                Some(b) => {
                    let j = decode_journal(&b)?;
                    self.verified(&txn, SubTree::Journals, &id.0, &journal_leaf_content(&j))?;
                    Some(j)
                }
                None => None,
            };
            out.journals.insert(*id, j);
        }
        for eid in &keys.external_ids {
            let slot = external_id_key(&eid.0);
            let seen = match self.leaf(&txn, SubTree::ExternalIds, &slot)? {
                None => false,
                Some(b) if b.as_slice() == eid.0.as_slice() => true,
                Some(_) => return Err(StoreError::ExternalIdCollision { slot }),
            };
            out.external_ids.insert(ExternalId(eid.0), seen);
        }
        for id in &keys.voided {
            let v = self.leaf(&txn, SubTree::Voided, &id.0)?.is_some();
            out.voided.insert(*id, v);
        }
        for id in &keys.pending {
            let p = match self.leaf(&txn, SubTree::Pending, &id.0)? {
                Some(b) => match b.as_slice() {
                    [code] => Some(decode_pending(*code)?),
                    _ => return Err(StoreError::Corrupt("pending leaf length".to_string())),
                },
                None => None,
            };
            out.pending.insert(*id, p);
        }
        Ok(out)
    }

    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        let mut profile = CommitProfile::default();
        let t = std::time::Instant::now();
        let next_posting = {
            let snapshot = Snapshot {
                store: self,
                txn: self.read()?,
            };
            rules::check(&snapshot, seq, delta)?;
            snapshot.expected_posting_seq()?
        };
        profile.rules_ns = ns_since(t);

        let mut txn = self.db.begin_write().map_err(io)?;
        let durability = if seq % SYNC_EVERY == 0 {
            Durability::Immediate
        } else {
            self.durability
        };
        txn.set_durability(durability).map_err(io)?;
        let p = &mut profile;
        let mut nodes = TxnNodes {
            table: txn.open_table(NODES).map_err(io)?,
            empty: &self.empty,
            tree: SubTree::Accounts,
            dirty: alloc::collections::BTreeMap::new(),
            reads: core::cell::Cell::new(0),
            writes: 0,
        };
        // One sub-tree at a time: its leaf table opened once.
        for tree in SubTree::ALL {
            nodes.tree = tree;
            let mut leaves = txn.open_table(leaf_table(tree)).map_err(io)?;
            let (l, n) = (&mut leaves, &mut nodes);
            match tree {
                SubTree::Accounts => {
                    for a in delta.accounts.values() {
                        let t = std::time::Instant::now();
                        let (stored, content) = (encode_account(a), account_leaf_content(a));
                        p.leaves_ns += ns_since(t);
                        write_leaf(l, n, p, &a.id.0, &stored, &content)?;
                    }
                }
                SubTree::Transfers => {
                    for x in delta.transfers.values() {
                        let t = std::time::Instant::now();
                        let (stored, content) = (encode_transfer(x), transfer_leaf_content(x));
                        p.leaves_ns += ns_since(t);
                        write_leaf(l, n, p, &x.id.0, &stored, &content)?;
                    }
                }
                SubTree::Journals => {
                    for j in delta.journals.values() {
                        let t = std::time::Instant::now();
                        let (stored, content) = (encode_journal(j), journal_leaf_content(j));
                        p.leaves_ns += ns_since(t);
                        write_leaf(l, n, p, &j.id.0, &stored, &content)?;
                    }
                }
                SubTree::ExternalIds => {
                    for x in &delta.external_ids {
                        let slot = external_id_key(&x.0);
                        let content = external_id_leaf_content(&x.0);
                        write_leaf(l, n, p, &slot, &x.0, &content)?;
                    }
                }
                SubTree::Voided => {
                    for v in &delta.voided {
                        write_leaf(l, n, p, &v.0, &[], &voided_leaf_content(&v.0))?;
                    }
                }
                SubTree::Pending => {
                    for (id, st) in &delta.pending {
                        let content = pending_leaf_content(&id.0, st.code());
                        write_leaf(l, n, p, &id.0, &[st.code()], &content)?;
                    }
                }
            }
        }
        let t = std::time::Instant::now();
        nodes.flush()?;
        p.nodes_ns += ns_since(t);
        p.node_reads = nodes.reads.get();
        p.node_writes = nodes.writes;
        drop(nodes);

        // Read the new roots inside the transaction, then log the commit.
        let t = std::time::Instant::now();
        let mut sub = [compact_smt::Version::ACTIVE.empty_root(); 6];
        {
            let nodes = txn.open_table(NODES).map_err(io)?;
            for t in SubTree::ALL {
                let k = node_key(
                    t,
                    &NodeAddr {
                        depth: 0,
                        prefix: [0; 16],
                    },
                );
                if let Some(g) = nodes.get(k.as_slice()).map_err(io)? {
                    sub[t.index()] = array32(g.value())?;
                }
            }
        }
        let record = LogRecord {
            seq,
            delta: delta.clone(),
            root_after: composite_root(&sub),
        };
        let bytes = encode_log_record(&record);
        p.log_bytes = bytes.len() as u64;
        txn.open_table(LOG)
            .map_err(io)?
            .insert(seq, bytes.as_slice())
            .map_err(io)?;
        txn.open_table(META)
            .map_err(io)?
            .insert(NEXT_POSTING_SEQ, next_posting + delta.postings.len() as u64)
            .map_err(io)?;
        p.log_ns = ns_since(t);
        let t = std::time::Instant::now();
        txn.commit().map_err(io)?;
        profile.durable_ns = ns_since(t);
        self.profile = profile;
        Ok(sub)
    }

    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.last_seq(&self.read()?)
    }

    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.meta(&self.read()?, NEXT_POSTING_SEQ)?.unwrap_or(0))
    }

    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        let txn = self.read()?;
        let table = txn.open_table(LOG).map_err(io)?;
        match table.get(seq).map_err(io)? {
            Some(g) => Ok(Some(decode_log_record(g.value())?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;
    use crate::store::suite::{self, store_rule_tests};
    use cipher_clerk::state::PendingStatus;

    store_rule_tests!(FileStore::in_memory().unwrap());

    /// The same deltas give the same roots in `FileStore` and `MemStore`, and
    /// both survive a reopen of the file.
    #[test]
    fn roots_match_the_kernels_tree_and_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.redb");
        let mut file = FileStore::open(&path).unwrap();
        let mut mem = MemStore::new();
        suite::bootstrap(&mut file);
        suite::bootstrap(&mut mem);
        let mut d = suite::transfer_delta(&mem, suite::transfer(1));
        d.accounts.insert(AccountId([1; 16]), suite::account(1));
        d.accounts.insert(AccountId([2; 16]), suite::account(2));
        d.external_ids.insert(ExternalId([5; 32]));
        d.voided.insert(TransferId([9; 16]));
        d.pending
            .insert(TransferId([7; 16]), PendingStatus::Pending);
        let a = file.commit(1, &d).unwrap();
        let b = mem.commit(1, &d).unwrap();
        assert_eq!(a, b);
        assert_eq!(composite_root(&a), mem.recomputed_root());
        drop(file);

        let file = FileStore::open(&path).unwrap();
        assert_eq!(file.subroots().unwrap(), b);
        assert_eq!(file.last_committed().unwrap(), Some(1));
        assert_eq!(file.next_posting_seq().unwrap(), 1);
        assert_eq!(file.log_record(1).unwrap(), mem.log_record(1).unwrap());
    }

    /// `T-031-14`: a store opened without synced commits and with a small cache reads every
    /// commit back at once and reaches the same roots as `MemStore`, many commits in.
    #[test]
    fn an_unsynced_store_with_a_small_cache_reads_its_commits_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = FileStore::open_with(
            dir.path().join("book.redb"),
            FileStoreOptions {
                durable: false,
                cache_bytes: 64 * 1024,
            },
        )
        .unwrap();
        let mut mem = MemStore::new();
        suite::bootstrap(&mut file);
        suite::bootstrap(&mut mem);
        for seq in 1..=50u8 {
            let d = suite::transfer_delta(&mem, suite::transfer(seq));
            let a = file.commit(u64::from(seq), &d).unwrap();
            let b = mem.commit(u64::from(seq), &d).unwrap();
            assert_eq!(a, b);
            assert_eq!(file.last_committed().unwrap(), Some(u64::from(seq)));
            assert_eq!(
                file.log_record(u64::from(seq)).unwrap(),
                mem.log_record(u64::from(seq)).unwrap()
            );
        }
        assert_eq!(
            composite_root(&file.subroots().unwrap()),
            mem.recomputed_root()
        );
    }

    /// `T-031-19`: the pruned node table, written once per commit, reaches CipherClerk's root
    /// (in a v2 build, `compact_smt::v2`'s)
    /// for keys that share long prefixes (deep subtrees of several leaves), many leaves per
    /// commit and one per commit, and reads its roots and leaves back after a reopen.
    #[test]
    fn t_031_19_the_pruned_table_reaches_the_kernels_roots() {
        use cipher_clerk::crypto::blake2b_256;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.redb");
        let mut file = FileStore::open(&path).unwrap();
        suite::bootstrap(&mut file);
        let mut reference = crate::testkit::BTreeMapLeaves::new();
        let mut keys: Vec<[u8; 16]> = Vec::new();
        let mut seq = 1u64;
        let mut pending = BookDelta::default();
        for i in 0u32..600 {
            let digest = blake2b_256(b"file-prune", &[&i.to_be_bytes()]);
            let mut key = [0u8; 16];
            key.copy_from_slice(&digest[..16]);
            if i % 3 == 0 && !keys.is_empty() {
                let base = keys[(i as usize / 3) % keys.len()];
                let share = [20usize, 40, 64, 120][(i as usize / 3) % 4];
                for b in 0..share {
                    let (byte, mask) = (b / 8, 0x80u8 >> (b % 8));
                    key[byte] = (key[byte] & !mask) | (base[byte] & mask);
                }
            }
            if keys.contains(&key) {
                continue;
            }
            keys.push(key);
            pending.voided.insert(TransferId(key));
            reference.insert(key, smt_leaf_hash(&voided_leaf_content(&key)));
            // Commits of 1, 7 and 40 leaves in turn.
            if pending.voided.len() >= [1usize, 7, 40][(seq % 3) as usize] {
                let sub = file.commit(seq, &core::mem::take(&mut pending)).unwrap();
                assert_eq!(
                    sub[SubTree::Voided.index()],
                    crate::testkit::tree_reference_root(&reference),
                    "commit {seq}"
                );
                seq += 1;
            }
        }
        if !pending.voided.is_empty() {
            file.commit(seq, &pending).unwrap();
        }
        drop(file);
        let file = FileStore::open(&path).unwrap();
        assert_eq!(
            file.subroots().unwrap()[SubTree::Voided.index()],
            crate::testkit::tree_reference_root(&reference)
        );
        let mut want = KeySet::default();
        want.voided.extend(keys.iter().map(|k| TransferId(*k)));
        assert!(file.load(&want).unwrap().voided.values().all(|v| *v));
    }

    /// INV-2 at the store, after a reopen: the write rules hold on what the
    /// file holds, not on what a process remembers.
    #[test]
    fn inv_2_overwrites_stay_refused_after_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.redb");
        let mut s = FileStore::open(&path).unwrap();
        suite::bootstrap(&mut s);
        let mut d = suite::transfer_delta(&s, suite::transfer(1));
        d.external_ids.insert(ExternalId([5; 32]));
        d.voided.insert(TransferId([6; 16]));
        s.commit(1, &d).unwrap();
        drop(s);

        let mut s = FileStore::open(&path).unwrap();
        let before = s.subroots().unwrap();
        let mut again = suite::transfer_delta(&s, suite::transfer(1));
        again.external_ids.insert(ExternalId([5; 32]));
        assert!(matches!(
            s.commit(2, &again),
            Err(StoreError::Refused(
                super::super::WriteRefusal::TransferExists(_)
            ))
        ));
        let mut eid = BookDelta::default();
        eid.external_ids.insert(ExternalId([5; 32]));
        assert!(matches!(
            s.commit(2, &eid),
            Err(StoreError::Refused(
                super::super::WriteRefusal::ExternalIdExists(_)
            ))
        ));
        let mut voided = BookDelta::default();
        voided.voided.insert(TransferId([6; 16]));
        assert!(matches!(
            s.commit(2, &voided),
            Err(StoreError::Refused(
                super::super::WriteRefusal::VoidedExists(_)
            ))
        ));
        assert_eq!(s.subroots().unwrap(), before);
        assert_eq!(s.last_committed().unwrap(), Some(1));
    }
    /// Overwrite the stored leaf at `key` of `tree`, behind the store's back: the node table is
    /// left as committed, as a corrupted file would be.
    fn overwrite_leaf(s: &FileStore, tree: SubTree, key: &[u8], bytes: &[u8]) {
        let txn = s.db.begin_write().unwrap();
        {
            let mut table = txn.open_table(leaf_table(tree)).unwrap();
            table.insert(key, bytes).unwrap();
        }
        txn.commit().unwrap();
    }

    /// `REQ-KER-6`: a slot holding another external id than the one asked about is fatal on a
    /// file, as on `MemStore`; the id the slot does hold reads as seen. The slot is written
    /// directly: a 64-bit birthday collision cannot be found in a test.
    #[test]
    fn req_ker_6_a_file_slot_holding_another_external_id_is_fatal() {
        let mut s = FileStore::in_memory().unwrap();
        suite::bootstrap(&mut s);
        let eid = ExternalId([5; 32]);
        let slot = external_id_key(&eid.0);
        let mut d = BookDelta::default();
        d.external_ids.insert(eid);
        s.commit(1, &d).unwrap();
        let mut keys = KeySet::default();
        keys.external_ids.insert(eid);
        assert!(
            s.load(&keys).unwrap().external_ids[&eid],
            "the stored id is seen"
        );
        let absent = ExternalId([6; 32]);
        let mut other = KeySet::default();
        other.external_ids.insert(absent);
        assert!(!s.load(&other).unwrap().external_ids[&absent]);

        // Another id is now in the slot.
        overwrite_leaf(&s, SubTree::ExternalIds, &slot, &[6; 32]);
        assert_eq!(s.load(&keys), Err(StoreError::ExternalIdCollision { slot }));
        let mut again = BookDelta::default();
        again.external_ids.insert(eid);
        assert_eq!(
            s.commit(2, &again),
            Err(StoreError::ExternalIdCollision { slot })
        );
    }

    /// `INV-2`, `REQ-KER-5`: a leaf whose bytes no longer hash to the node committed for it is
    /// refused on load as corrupt, for every kind of leaf that is verified (an account here). The
    /// same bytes, untouched, load.
    #[test]
    fn inv_2_a_file_leaf_that_does_not_match_its_node_is_refused_on_load() {
        let mut s = FileStore::in_memory().unwrap();
        suite::bootstrap(&mut s);
        let account = suite::account(1);
        let mut d = BookDelta::default();
        d.accounts.insert(account.id, account.clone());
        s.commit(1, &d).unwrap();
        let mut keys = KeySet::default();
        keys.accounts.insert(account.id);
        assert_eq!(
            s.load(&keys).unwrap().accounts[&account.id].as_ref(),
            Some(&account),
            "the committed leaf loads"
        );

        // A different, well-formed account in its place: it decodes, and does not match.
        let mut forged = account.clone();
        forged.code = account.code + 1;
        overwrite_leaf(
            &s,
            SubTree::Accounts,
            &account.id.0,
            &encode_account(&forged),
        );
        assert!(matches!(
            s.load(&keys),
            Err(StoreError::Corrupt(m)) if m.contains("does not match its node")
        ));
    }
}
