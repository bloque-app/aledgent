// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! [`BookStore`]: the book's leaves, history and indexes, outside the core (`F-006`, `DEC-4`).
//!
//! **One store, either core.** The store serves a core that runs in its own process and a core
//! that runs inside an Agent, with the same code and the same committed delta log:
//!
//! - a **native core** commits through `F-001`'s [`LeafStore`] trait: `Book<BookStore, _>` works
//!   unchanged. Each commit becomes a [`DeltaRecord`] that the store builds itself, with the
//!   service tree's puts staged beforehand by [`BookStore::stage_service`];
//! - an **Agent core** commits a record inside the Agent and hands it to the host, which calls
//!   [`BookStore::apply_record`]. The store checks it instead of building it.
//!
//! Both paths end in one pipeline:
//!
//! 1. take the next record by sequence, refusing a gap; a record already applied is a no-op if it
//!    is the same record, and a conflict otherwise;
//! 2. check `prev_log_hash` against the head and `roots_before` against the store's roots;
//! 3. check the write rules of `INV-2` and `INV-25` (`F-001`'s rules, shared by every store) and
//!    that no service key is taken by another record;
//! 4. write the leaves, recompute the seven roots, and compare them with `roots_after`. A
//!    mismatch **stops the store** ([`Incident`]): it refuses every later record until an operator
//!    reopens it;
//! 5. update history and indexes, append the record to the archive and fsync it, then commit the
//!    store, recording the applied sequence and the head.
//!
//! The archive is written before the store, so a crash between the two leaves the archive ahead;
//! [`BookStore::open`] catches the store up from it. The store therefore needs no fsync of its own
//! per commit ([`StoreConfig::durability`]): the archive is what makes a commit durable.
//!
//! **Reads.** Every read runs on a [`ReadView`], a snapshot pinned to one applied sequence
//! (`REQ-QR-2`, `REQ-BA-3`). A record is visible to views as soon as `apply_record` returns, so a
//! driver that applies before it acknowledges never shows a holder a number older than the one
//! the spend was checked against (**read after acknowledge**).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use cipher_clerk::crypto::blake2b_256;
use cipher_clerk::ids::ExternalId;
use cipher_clerk::merkle::{SMT_DEPTH, SMT_EMPTY_LEAF, SmtProof, build_empty_chain, smt_leaf_hash};
use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::{
    account_leaf_content, external_id_key, external_id_leaf_content, journal_leaf_content,
    pending_leaf_content, transfer_leaf_content, voided_leaf_content,
};
use cipher_clerk::{Account, AccountId, Direction, Journal, Layer, Transfer, TransferId};
use ledger_core::delta::{
    DeltaRecord, Fact, GENESIS_LOG_HASH, RecordError, RecordParts, Roots, ServiceDelta,
    ServiceKind, ServiceLeaf, service_key,
};
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::codec::{
    decode_account, decode_journal, decode_pending, decode_transfer, encode_account,
    encode_journal, encode_transfer,
};
use ledger_kernel::store::rules::{self, RuleView};
use ledger_kernel::store::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, StoreError, SubRoots, SubTree, WriteRefusal,
};

use crate::archive::{Archive, ArchiveConfig, ArchiveError, TailRepair};
use crate::engine::{Durability, Engine, EngineError, RedbEngine, Snapshot, Space};
use crate::kv::{Kv, Txn};
use crate::trie::{self, EmptyChain, Frontier, TreeId, leaf_key};

/// A leaf row: the leaf hash and the stored bytes.
type LeafRow = ([u8; 32], Vec<u8>);

/// A holder of authority over an account, and the grant it holds it by.
pub type HolderGrant = ([u8; 16], [u8; 16]);

/// A visitor of leaves: key and stored bytes.
pub type LeafVisit<'a> = dyn FnMut(&[u8; 16], &[u8]) -> Result<(), StoreError> + 'a;

/// A visitor of rows: key and value.
pub type RowVisit<'a> = dyn FnMut(&[u8], &[u8]) -> Result<(), StoreError> + 'a;

/// The empty-subtree hashes, computed once.
pub fn empty_chain() -> &'static EmptyChain {
    static EMPTY: OnceLock<EmptyChain> = OnceLock::new();
    EMPTY.get_or_init(build_empty_chain)
}

/// The tree of a kernel sub-tree.
pub const fn tree_of(t: SubTree) -> TreeId {
    TreeId(t.index() as u8)
}

const ACCOUNTS: TreeId = tree_of(SubTree::Accounts);
const TRANSFERS: TreeId = tree_of(SubTree::Transfers);
const JOURNALS: TreeId = tree_of(SubTree::Journals);
const EXTERNAL_IDS: TreeId = tree_of(SubTree::ExternalIds);
const VOIDED: TreeId = tree_of(SubTree::Voided);
const PENDING: TreeId = tree_of(SubTree::Pending);

const META_APPLIED: &[u8] = b"applied";
const META_HEAD: &[u8] = b"head";
const META_POSTING: &[u8] = b"next-posting";

/// How the store runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreConfig {
    /// Durability of each store commit. [`Durability::Visible`] is safe whenever the store has an
    /// archive, which is synced before the store commits and replayed on open.
    pub durability: Durability,
    /// Re-hash every leaf read and compare it with its stored hash (`F-001` NOTES 5).
    pub verify_reads: bool,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            durability: Durability::Durable,
            verify_reads: true,
        }
    }
}

/// The store stopped: a record's `roots_after` is not what applying it gives. An operator must
/// look before anything else is applied (`F-018` pages on it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incident {
    /// The record's sequence number.
    pub seq: u64,
    /// The roots the record claims.
    pub expected: Roots,
    /// The roots the store computed.
    pub computed: Roots,
}

/// Why a record was not applied. Nothing was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The store refused or failed (the write rules of `INV-2`, `INV-25`, `REQ-PO-2`, or the
    /// engine).
    Store(StoreError),
    /// The archive failed.
    Archive(ArchiveError),
    /// The record is not the next one.
    Gap {
        /// The sequence number the store expects.
        expected: u64,
        /// The one it was given.
        got: u64,
    },
    /// Another record was already applied at this sequence number.
    Conflict {
        /// The sequence number.
        seq: u64,
    },
    /// The record does not follow the store's head.
    ChainBroken {
        /// The record's sequence number.
        seq: u64,
    },
    /// The record's `roots_before` are not the store's roots.
    RootsBefore {
        /// The record's sequence number.
        seq: u64,
    },
    /// Applying the record did not give its `roots_after`. The store is stopped.
    RootsAfter(Box<Incident>),
    /// The store is stopped by an earlier incident.
    Halted(Box<Incident>),
    /// A service key is taken by another record (a 64-bit collision; fatal).
    ServiceCollision {
        /// The key.
        key: [u8; 16],
    },
    /// An account's reserved total would pass `u64::MAX` minor units, or go below zero.
    ReservedOutOfRange {
        /// The ledger account.
        account: [u8; 16],
        /// The asset tag.
        asset: u32,
    },
    /// The record could not be built.
    Record(RecordError),
    /// The operation needs an archive and the store has none.
    NoArchive,
    /// The store failed to write a record the archive already holds. The record is committed;
    /// the store serves nothing until it is reopened, which replays it.
    Behind {
        /// The record's sequence number.
        seq: u64,
    },
}

impl From<StoreError> for ApplyError {
    fn from(e: StoreError) -> Self {
        ApplyError::Store(e)
    }
}

impl From<EngineError> for ApplyError {
    fn from(e: EngineError) -> Self {
        ApplyError::Store(StoreError::Io(e.0))
    }
}

impl From<ArchiveError> for ApplyError {
    fn from(e: ArchiveError) -> Self {
        ApplyError::Archive(e)
    }
}

impl From<ApplyError> for StoreError {
    fn from(e: ApplyError) -> Self {
        match e {
            ApplyError::Store(e) => e,
            ApplyError::Record(RecordError::TooLarge { bytes }) => {
                StoreError::Refused(WriteRefusal::RecordTooLarge {
                    bytes: bytes as u64,
                    limit: ledger_core::delta::MAX_RECORD_BYTES as u64,
                })
            }
            other => StoreError::Io(format!("{other:?}")),
        }
    }
}

fn eng(e: EngineError) -> StoreError {
    StoreError::Io(e.0)
}

/// What an applied record did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The record was applied now.
    Applied {
        /// Its sequence number.
        seq: u64,
        /// The new head.
        log_hash: [u8; 32],
    },
    /// The same record was already applied: nothing changed (`StateDelta::apply` is idempotent,
    /// and so is this).
    AlreadyApplied {
        /// Its sequence number.
        seq: u64,
    },
}

/// Where the store stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Head {
    applied: Option<u64>,
    log_head: [u8; 32],
    next_posting: u64,
    roots: Roots,
}

/// The service tree's puts and the operations staged for the next native commit.
#[derive(Clone, Debug, Default)]
struct Staged {
    service: ServiceDelta,
    ops: BTreeSet<OperationRef>,
}

/// Where a test may make the store crash in the middle of a commit.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrashPoint {
    /// After the checks, before the archive.
    BeforeArchive,
    /// After the archive is synced, before the store commits.
    AfterArchive,
    /// After the store commits, before the caller hears of it.
    AfterApply,
}

/// What [`BookStore::open`] did to bring the store to the archive's head.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    /// An incomplete final archive entry that was cut off.
    pub tail: TailRepair,
    /// Records replayed from the archive into the store.
    pub replayed: u64,
}

/// The book's host-side store (module documentation).
pub struct BookStore<E: Engine = RedbEngine> {
    engine: E,
    archive: Option<Archive>,
    config: StoreConfig,
    head: Head,
    staged: Mutex<Staged>,
    halted: Option<Incident>,
    behind: Option<u64>,
    #[cfg(test)]
    crash: Mutex<Option<CrashPoint>>,
}

impl<E: Engine> core::fmt::Debug for BookStore<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BookStore")
            .field("applied", &self.head.applied)
            .field("halted", &self.halted.is_some())
            .finish_non_exhaustive()
    }
}

impl BookStore<RedbEngine> {
    /// Open (or create) a store in `dir`: `dir/leaves.redb` and the archive in `dir/archive/`.
    /// An incomplete final archive entry is cut off, and the store is caught up from the archive.
    pub fn open(
        dir: impl AsRef<Path>,
        config: StoreConfig,
        archive: ArchiveConfig,
    ) -> Result<(Self, Recovery), ApplyError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
        let engine = RedbEngine::open(dir.join("leaves.redb"))?;
        let (archive, tail) = Archive::open_repairing(dir.join("archive"), archive)?;
        let mut store = Self::with_engine(engine, Some(archive), config)?;
        let replayed = store.catch_up()?;
        Ok((store, Recovery { tail, replayed }))
    }

    /// A store on the in-memory engine with an in-memory archive: the same code and rules, no
    /// files. For tests.
    pub fn in_memory() -> Result<Self, ApplyError> {
        Self::with_engine(
            RedbEngine::in_memory()?,
            Some(Archive::in_memory()),
            StoreConfig {
                durability: Durability::Visible,
                verify_reads: true,
            },
        )
    }

    /// A projection without an archive of its own, on the in-memory engine: what a rebuild
    /// applies records into.
    pub fn projection_in_memory() -> Result<Self, ApplyError> {
        Self::with_engine(
            RedbEngine::in_memory()?,
            None,
            StoreConfig {
                durability: Durability::Visible,
                verify_reads: true,
            },
        )
    }
}

impl<E: Engine> BookStore<E> {
    /// A store over `engine`, with `archive` if it keeps one. Reads where the engine stands; call
    /// [`BookStore::catch_up`] to replay records the archive holds beyond it.
    pub fn with_engine(
        engine: E,
        archive: Option<Archive>,
        config: StoreConfig,
    ) -> Result<Self, ApplyError> {
        let snap = engine.snapshot()?;
        let head = read_head(&snap)?;
        drop(snap);
        Ok(Self {
            engine,
            archive,
            config,
            head,
            staged: Mutex::new(Staged::default()),
            halted: None,
            behind: None,
            #[cfg(test)]
            crash: Mutex::new(None),
        })
    }

    /// Replay every record the archive holds beyond the store, verifying each. Returns how many.
    /// The archive's record at the store's own sequence must be the one the store applied.
    pub fn catch_up(&mut self) -> Result<u64, ApplyError> {
        let Some(archive) = self.archive.as_ref() else {
            return Ok(0);
        };
        let Some((last, _)) = archive.last() else {
            return match self.head.applied {
                None => Ok(0),
                Some(_) => Err(ApplyError::Archive(ArchiveError::HeadMismatch {
                    archive: None,
                    expected: self.head.applied.map(|s| (s, self.head.log_head)),
                })),
            };
        };
        if let Some(applied) = self.head.applied {
            if applied > last {
                return Err(ApplyError::Archive(ArchiveError::HeadMismatch {
                    archive: archive.last(),
                    expected: Some((applied, self.head.log_head)),
                }));
            }
            let own = archive
                .get(applied)?
                .ok_or(ApplyError::Conflict { seq: applied })?;
            if own.log_hash() != self.head.log_head {
                return Err(ApplyError::Conflict { seq: applied });
            }
        }
        let from = self.head.applied.map_or(0, |a| a + 1);
        let mut replayed = 0;
        for seq in from..=last {
            let rec = self
                .archive
                .as_ref()
                .expect("checked above")
                .get(seq)?
                .ok_or(ApplyError::Gap {
                    expected: seq,
                    got: seq + 1,
                })?;
            self.apply_checked(&rec, false)?;
            replayed += 1;
        }
        if replayed > 0 {
            self.engine.flush()?;
        }
        Ok(replayed)
    }

    /// The engine.
    pub fn engine(&self) -> &E {
        &self.engine
    }

    /// The archive, if the store keeps one.
    pub fn archive(&self) -> Option<&Archive> {
        self.archive.as_ref()
    }

    /// The archive, mutably (reads load segment offsets).
    pub fn archive_mut(&mut self) -> Option<&mut Archive> {
        self.archive.as_mut()
    }

    /// The last applied sequence number.
    pub fn applied_sequence(&self) -> Option<u64> {
        self.head.applied
    }

    /// The head of the committed delta log: the last applied record's log hash.
    pub fn log_head(&self) -> [u8; 32] {
        self.head.log_head
    }

    /// The seven roots.
    pub fn roots(&self) -> Roots {
        self.head.roots
    }

    /// The incident that stopped the store, if any.
    pub fn incident(&self) -> Option<&Incident> {
        self.halted.as_ref()
    }

    /// Make every earlier store commit durable.
    pub fn flush(&self) -> Result<(), ApplyError> {
        Ok(self.engine.flush()?)
    }

    /// Stage service-tree puts and operations for the next native commit through
    /// [`LeafStore::commit`]. Staged puts of the same record merge, the later winning.
    pub fn stage_service(
        &self,
        service: &ServiceDelta,
        ops: &BTreeSet<OperationRef>,
    ) -> Result<(), ApplyError> {
        let mut staged = self.staged.lock().expect("staging lock");
        for (_, leaf) in service.puts() {
            staged
                .service
                .put(leaf.clone())
                .map_err(ApplyError::Record)?;
        }
        for (_, (kind, id)) in service.removes() {
            staged
                .service
                .remove(*kind, *id)
                .map_err(ApplyError::Record)?;
        }
        staged.ops.extend(ops.iter().copied());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn crash_at(&self, point: Option<CrashPoint>) {
        *self.crash.lock().expect("crash lock") = point;
    }

    /// A simulated crash: the process stops here. Tests catch the unwind and reopen the store
    /// from what reached its files.
    #[cfg(test)]
    fn crash_here(&mut self, point: CrashPoint) -> Result<(), ApplyError> {
        let hit = {
            let mut c = self.crash.lock().expect("crash lock");
            let hit = *c == Some(point);
            if hit {
                *c = None;
            }
            hit
        };
        if hit {
            panic!("simulated crash at {point:?}");
        }
        Ok(())
    }

    fn check_serving(&self) -> Result<(), ApplyError> {
        if let Some(i) = &self.halted {
            return Err(ApplyError::Halted(Box::new(i.clone())));
        }
        if let Some(seq) = self.behind {
            return Err(ApplyError::Behind { seq });
        }
        Ok(())
    }

    /// Apply a record the core committed (the Agent path; module documentation).
    pub fn apply_record(&mut self, rec: &DeltaRecord) -> Result<Applied, ApplyError> {
        self.check_serving()?;
        let expected = self.head.applied.map_or(0, |a| a + 1);
        if rec.sequence() < expected {
            let same = if Some(rec.sequence()) == self.head.applied {
                rec.log_hash() == self.head.log_head
            } else {
                let archive = self.archive.as_ref().ok_or(ApplyError::NoArchive)?;
                archive
                    .get(rec.sequence())?
                    .is_some_and(|a| a.log_hash() == rec.log_hash())
            };
            return if same {
                Ok(Applied::AlreadyApplied {
                    seq: rec.sequence(),
                })
            } else {
                Err(ApplyError::Conflict {
                    seq: rec.sequence(),
                })
            };
        }
        let append = self
            .archive
            .as_ref()
            .is_some_and(|a| a.last().is_none_or(|(s, _)| s < rec.sequence()));
        self.apply_checked(rec, append)
    }

    fn apply_checked(&mut self, rec: &DeltaRecord, append: bool) -> Result<Applied, ApplyError> {
        let expected = self.head.applied.map_or(0, |a| a + 1);
        if rec.sequence() != expected {
            return Err(ApplyError::Gap {
                expected,
                got: rec.sequence(),
            });
        }
        if !rec.follows(&self.head.log_head) {
            return Err(ApplyError::ChainBroken {
                seq: rec.sequence(),
            });
        }
        if rec.roots_before() != &self.head.roots {
            return Err(ApplyError::RootsBefore {
                seq: rec.sequence(),
            });
        }
        let snap = self.engine.snapshot()?;
        let (txn, roots) = self.stage(
            &snap,
            rec.sequence(),
            rec.kernel_delta(),
            rec.service_delta(),
        )?;
        if &roots != rec.roots_after() {
            let incident = Incident {
                seq: rec.sequence(),
                expected: *rec.roots_after(),
                computed: roots,
            };
            self.halted = Some(incident.clone());
            return Err(ApplyError::RootsAfter(Box::new(incident)));
        }
        let batch = txn.into_batch();
        drop(snap);
        self.finish(batch, rec, roots, append)
    }

    /// Check a delta against the rules and build its writes over `snap`. Returns the batch and
    /// the roots after.
    fn stage<'s>(
        &self,
        snap: &'s E::Snapshot,
        seq: u64,
        kernel: &BookDelta,
        service: &ServiceDelta,
    ) -> Result<(Txn<'s, E::Snapshot>, Roots), ApplyError> {
        let view = RuleSnap {
            snap,
            applied: self.head.applied,
            next_posting: self.head.next_posting,
        };
        rules::check(&view, seq, kernel)?;
        let old_service = old_service_leaves(snap, service)?;

        let empty = empty_chain();
        let mut txn = Txn::new(snap);
        for a in kernel.accounts.values() {
            put_leaf(
                &mut txn,
                ACCOUNTS,
                &a.id.0,
                &encode_account(a),
                &account_leaf_content(a),
            )?;
        }
        for t in kernel.transfers.values() {
            put_leaf(
                &mut txn,
                TRANSFERS,
                &t.id.0,
                &encode_transfer(t),
                &transfer_leaf_content(t),
            )?;
        }
        for j in kernel.journals.values() {
            put_leaf(
                &mut txn,
                JOURNALS,
                &j.id.0,
                &encode_journal(j),
                &journal_leaf_content(j),
            )?;
        }
        for e in &kernel.external_ids {
            put_leaf(
                &mut txn,
                EXTERNAL_IDS,
                &external_id_key(&e.0),
                &e.0,
                &external_id_leaf_content(&e.0),
            )?;
        }
        for v in &kernel.voided {
            put_leaf(&mut txn, VOIDED, &v.0, &[], &voided_leaf_content(&v.0))?;
        }
        for (id, s) in &kernel.pending {
            put_leaf(
                &mut txn,
                PENDING,
                &id.0,
                &[s.code()],
                &pending_leaf_content(&id.0, s.code()),
            )?;
        }
        write_service(&mut txn, service, &old_service)?;

        // History and the global posting sequence.
        for p in &kernel.postings {
            let t = kernel
                .transfers
                .get(&p.transfer)
                .ok_or(StoreError::Refused(WriteRefusal::PostingsMismatch))?;
            txn.put(Space::Posting, &p.seq.to_be_bytes(), p.transfer.0.to_vec());
            for (i, e) in t.entries.iter().enumerate() {
                let (k, v) =
                    history_row(&e.account_id, p.seq, i as u16, &t.id, e.direction, e.layer);
                txn.put(Space::History, &k, v);
            }
        }

        let next_posting = self.head.next_posting + kernel.postings.len() as u64;
        txn.put(Space::Meta, META_APPLIED, seq.to_be_bytes().to_vec());
        txn.put(
            Space::Meta,
            META_POSTING,
            next_posting.to_be_bytes().to_vec(),
        );
        let roots = roots_of(&txn, empty)?;
        Ok((txn, roots))
    }

    /// Archive the record, then commit the store's writes.
    fn finish(
        &mut self,
        mut batch: crate::engine::WriteBatch,
        rec: &DeltaRecord,
        roots: Roots,
        append: bool,
    ) -> Result<Applied, ApplyError> {
        batch.put(Space::Meta, META_HEAD, rec.log_hash().to_vec());
        #[cfg(test)]
        self.crash_here(CrashPoint::BeforeArchive)?;
        if append {
            let archive = self.archive.as_mut().ok_or(ApplyError::NoArchive)?;
            archive.append(rec)?;
            archive.sync()?;
        }
        #[cfg(test)]
        self.crash_here(CrashPoint::AfterArchive)?;
        if let Err(e) = self.engine.write(batch, self.config.durability) {
            if self.archive.is_none() {
                return Err(e.into());
            }
            // The record is in the archive: it is committed, and reopening the store replays
            // it. Until then the store is behind what it acknowledged and serves nothing.
            self.behind = Some(rec.sequence());
        }
        self.head = Head {
            applied: Some(rec.sequence()),
            log_head: rec.log_hash(),
            next_posting: self.head.next_posting + rec.kernel_delta().postings.len() as u64,
            roots,
        };
        #[cfg(test)]
        self.crash_here(CrashPoint::AfterApply)?;
        Ok(Applied::Applied {
            seq: rec.sequence(),
            log_hash: rec.log_hash(),
        })
    }

    /// The native path: build the record for this commit, then apply it.
    fn commit_native(&mut self, seq: u64, kernel: &BookDelta) -> Result<SubRoots, ApplyError> {
        self.check_serving()?;
        let staged = core::mem::take(&mut *self.staged.lock().expect("staging lock"));
        let result = self.commit_staged(seq, kernel, &staged);
        if result.is_err() {
            // Nothing was committed: keep what was staged for the retry.
            let mut s = self.staged.lock().expect("staging lock");
            if s.service.is_empty() && s.ops.is_empty() {
                *s = staged;
            }
        }
        result
    }

    fn commit_staged(
        &mut self,
        seq: u64,
        kernel: &BookDelta,
        staged: &Staged,
    ) -> Result<SubRoots, ApplyError> {
        let snap = self.engine.snapshot()?;
        let (txn, roots) = self.stage(&snap, seq, kernel, &staged.service)?;
        let rec = DeltaRecord::new(RecordParts {
            sequence: seq,
            prev_log_hash: self.head.log_head,
            roots_before: self.head.roots,
            roots_after: roots,
            kernel_delta: kernel.clone(),
            service_delta: staged.service.clone(),
            ops: staged.ops.clone(),
        })
        .map_err(ApplyError::Record)?;
        let batch = txn.into_batch();
        drop(snap);
        let append = self.archive.is_some();
        self.finish(batch, &rec, roots, append)?;
        Ok(roots.kernel)
    }

    /// Apply service-tree changes alone, with no delta record: the service tree of a core that
    /// runs inside an Agent (`F-005`), kept by the host in a store of its own. The Agent's delta
    /// ring and the host's archive of its inputs are that core's committed transitions, and the
    /// store is rebuilt by replaying them (`INV-6`), so no record of these changes is kept here;
    /// a store that commits kernel records must not take them. Returns the service root after.
    pub fn apply_service(&mut self, service: &ServiceDelta) -> Result<[u8; 32], ApplyError> {
        self.check_serving()?;
        if self.head.applied.is_some() {
            return Err(ApplyError::Record(RecordError::Codec(
                "service changes without a record, in a store that holds records".into(),
            )));
        }
        let snap = self.engine.snapshot()?;
        let old = old_service_leaves(&snap, service)?;
        let mut txn = Txn::new(&snap);
        write_service(&mut txn, service, &old)?;
        let roots = roots_of(&txn, empty_chain())?;
        let batch = txn.into_batch();
        drop(snap);
        self.engine.write(batch, self.config.durability)?;
        self.head.roots = roots;
        Ok(roots.service)
    }

    /// Take the store apart: its engine and archive, to reopen them (a restart).
    pub fn into_parts(self) -> (E, Option<Archive>) {
        (self.engine, self.archive)
    }

    /// A read view pinned to the current applied sequence.
    pub fn view(&self) -> Result<ReadView<E::Snapshot>, StoreError> {
        if let Some(seq) = self.behind {
            return Err(ApplyError::Behind { seq }.into());
        }
        let snap = self.engine.snapshot().map_err(eng)?;
        ReadView::new(snap, self.config.verify_reads)
    }

    /// Digest of a canonical dump of the whole store: every row, leaves, nodes, indexes and
    /// meta, in key order (`F-006` exit criterion).
    pub fn digest(&self) -> Result<[u8; 32], StoreError> {
        let snap = self.engine.snapshot().map_err(eng)?;
        let mut acc = [0u8; 32];
        snap.for_each(&mut |k, v| {
            acc = blake2b_256(
                b"aledgent/store-dump/v1",
                &[
                    &acc,
                    &(k.len() as u32).to_be_bytes(),
                    k,
                    &(v.len() as u32).to_be_bytes(),
                    v,
                ],
            );
            Ok(())
        })
        .map_err(eng)?;
        Ok(acc)
    }

    /// Every row of the store, in key order, with its full key: for diagnosing a digest mismatch.
    pub fn dump(&self) -> Result<crate::engine::Rows, StoreError> {
        let snap = self.engine.snapshot().map_err(eng)?;
        let mut out = Vec::new();
        snap.for_each(&mut |k, v| {
            out.push((k.to_vec(), v.to_vec()));
            Ok(())
        })
        .map_err(eng)?;
        Ok(out)
    }
}

fn read_head<S: Snapshot>(snap: &S) -> Result<Head, ApplyError> {
    let u64_of = |v: Option<Vec<u8>>| -> Result<Option<u64>, ApplyError> {
        match v {
            None => Ok(None),
            Some(b) if b.len() == 8 => {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                Ok(Some(u64::from_be_bytes(a)))
            }
            Some(_) => Err(StoreError::Corrupt("meta row".into()).into()),
        }
    };
    let applied = u64_of(snap.get(Space::Meta, META_APPLIED)?)?;
    let next_posting = u64_of(snap.get(Space::Meta, META_POSTING)?)?.unwrap_or(0);
    let log_head = match snap.get(Space::Meta, META_HEAD)? {
        None => GENESIS_LOG_HASH,
        Some(b) if b.len() == 32 => array32(&b),
        Some(_) => return Err(StoreError::Corrupt("head row".into()).into()),
    };
    Ok(Head {
        applied,
        log_head,
        next_posting,
        roots: roots_of(snap, empty_chain())?,
    })
}

fn roots_of<K: Kv + ?Sized>(kv: &K, empty: &EmptyChain) -> Result<Roots, EngineError> {
    let mut kernel = [[0u8; 32]; 6];
    for (i, r) in kernel.iter_mut().enumerate() {
        *r = trie::root(kv, TreeId(i as u8), empty)?;
    }
    Ok(Roots {
        kernel,
        service: trie::root(kv, TreeId::SERVICE, empty)?,
    })
}

fn array32(b: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(b);
    out
}

/// The service leaves `service` changes, as they stand: each must be of the same record as the
/// change (a key taken by another record is a collision, fatal).
fn old_service_leaves<K: Kv + ?Sized>(
    kv: &K,
    service: &ServiceDelta,
) -> Result<BTreeMap<[u8; 16], ServiceLeaf>, ApplyError> {
    let mut old_service: BTreeMap<[u8; 16], ServiceLeaf> = BTreeMap::new();
    let changes = service
        .puts()
        .map(|(k, l)| (k, (l.kind(), l.id())))
        .chain(service.removes().map(|(k, r)| (k, *r)));
    for (key, (kind, id)) in changes {
        if let Some((_, bytes)) = read_leaf(kv, TreeId::SERVICE, key)? {
            let old = ServiceLeaf::decode(&bytes)
                .map_err(|e| StoreError::Corrupt(format!("service leaf: {e:?}")))?;
            if (old.kind(), old.id()) != (kind, id) {
                return Err(ApplyError::ServiceCollision { key: *key });
            }
            old_service.insert(*key, old);
        }
    }
    Ok(old_service)
}

/// Write `service`'s puts and removals, and their facts' index rows. A removed leaf keeps an
/// emptied row: the empty sentinel as its hash and nothing stored, so the tree's root is the one
/// it would have without the leaf. A removal of an absent leaf changes nothing.
fn write_service<S: Snapshot>(
    txn: &mut Txn<'_, S>,
    service: &ServiceDelta,
    old_service: &BTreeMap<[u8; 16], ServiceLeaf>,
) -> Result<(), ApplyError> {
    for (key, leaf) in service.puts() {
        put_leaf(
            txn,
            TreeId::SERVICE,
            key,
            &leaf.encode(),
            &leaf.leaf_content(),
        )?;
        let old: &[Fact] = old_service.get(key).map_or(&[], |l| l.facts());
        index_facts(txn, &leaf.id(), old, leaf.facts())?;
    }
    for (key, (_, id)) in service.removes() {
        if let Some(old) = old_service.get(key) {
            txn.put(
                Space::Leaf,
                &leaf_key(TreeId::SERVICE, key),
                SMT_EMPTY_LEAF.to_vec(),
            );
            trie::upsert(txn, TreeId::SERVICE, empty_chain(), key, SMT_EMPTY_LEAF)?;
            index_facts(txn, id, old.facts(), &[])?;
        }
    }
    Ok(())
}

/// A leaf row: its hash, then its stored bytes.
fn read_leaf<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    key: &[u8; 16],
) -> Result<Option<LeafRow>, EngineError> {
    match kv.get(Space::Leaf, &leaf_key(tree, key))? {
        None => Ok(None),
        // An emptied service leaf (`BookStore::stage`'s removals) reads as absent.
        Some(v) if v.len() == 32 && array32(&v) == SMT_EMPTY_LEAF => Ok(None),
        Some(v) if v.len() >= 32 => Ok(Some((array32(&v[..32]), v[32..].to_vec()))),
        Some(_) => Err(EngineError("corrupt leaf row".into())),
    }
}

fn put_leaf<S: Snapshot>(
    txn: &mut Txn<'_, S>,
    tree: TreeId,
    key: &[u8; 16],
    stored: &[u8],
    content: &[u8],
) -> Result<(), EngineError> {
    let h = smt_leaf_hash(content);
    let mut row = Vec::with_capacity(32 + stored.len());
    row.extend_from_slice(&h);
    row.extend_from_slice(stored);
    txn.put(Space::Leaf, &leaf_key(tree, key), row);
    trie::upsert(txn, tree, empty_chain(), key, h)?;
    Ok(())
}

/// The history row of one entry: `account ‖ posting seq ‖ entry index` → `transfer ‖ side ‖
/// layer`.
fn history_row(
    account: &AccountId,
    seq: u64,
    index: u16,
    transfer: &TransferId,
    direction: Direction,
    layer: Layer,
) -> ([u8; 26], Vec<u8>) {
    let mut k = [0u8; 26];
    k[..16].copy_from_slice(&account.0);
    k[16..24].copy_from_slice(&seq.to_be_bytes());
    k[24..].copy_from_slice(&index.to_be_bytes());
    let mut v = Vec::with_capacity(18);
    v.extend_from_slice(&transfer.0);
    v.push(match direction {
        Direction::Debit => 0,
        Direction::Credit => 1,
    });
    v.push(layer.as_byte());
    (k, v)
}

fn layer_of(b: u8) -> Option<Layer> {
    [Layer::Settled, Layer::Pending, Layer::Encumbrance]
        .into_iter()
        .find(|l| l.as_byte() == b)
}

fn reserved_key(account: &[u8; 16], asset: u32) -> [u8; 20] {
    let mut k = [0u8; 20];
    k[..16].copy_from_slice(account);
    k[16..].copy_from_slice(&asset.to_be_bytes());
    k
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Move the index rows a service leaf states from `old` facts to `new` ones.
fn index_facts<S: Snapshot>(
    txn: &mut Txn<'_, S>,
    leaf_id: &[u8; 16],
    old: &[Fact],
    new: &[Fact],
) -> Result<(), ApplyError> {
    let old_set: BTreeSet<&Fact> = old.iter().collect();
    let new_set: BTreeSet<&Fact> = new.iter().collect();
    for f in old_set.difference(&new_set) {
        fact_row(txn, leaf_id, f, false)?;
    }
    for f in new_set.difference(&old_set) {
        fact_row(txn, leaf_id, f, true)?;
    }
    Ok(())
}

fn fact_row<S: Snapshot>(
    txn: &mut Txn<'_, S>,
    leaf_id: &[u8; 16],
    fact: &Fact,
    add: bool,
) -> Result<(), ApplyError> {
    match fact {
        Fact::OwnedBy { identity } => {
            let k = cat(&[identity, leaf_id]);
            if add {
                txn.put(Space::Owned, &k, Vec::new());
            } else {
                txn.delete(Space::Owned, &k);
            }
        }
        Fact::Holder { account, holder } => {
            let k = cat(&[account, holder, leaf_id]);
            if add {
                txn.put(Space::Holder, &k, Vec::new());
            } else {
                txn.delete(Space::Holder, &k);
            }
        }
        Fact::Reserves {
            account,
            asset,
            amount,
        } => {
            let k = reserved_key(account, *asset);
            let now = match txn.get(Space::Reserved, &k)? {
                None => 0u64,
                Some(b) if b.len() == 8 => {
                    let mut a = [0u8; 8];
                    a.copy_from_slice(&b);
                    u64::from_be_bytes(a)
                }
                Some(_) => return Err(StoreError::Corrupt("reserved row".into()).into()),
            };
            let out_of_range = || ApplyError::ReservedOutOfRange {
                account: *account,
                asset: *asset,
            };
            let next = if add {
                now.checked_add(amount.get()).ok_or_else(out_of_range)?
            } else {
                now.checked_sub(amount.get()).ok_or_else(out_of_range)?
            };
            if next == 0 {
                txn.delete(Space::Reserved, &k);
            } else {
                txn.put(Space::Reserved, &k, next.to_be_bytes().to_vec());
            }
        }
    }
    Ok(())
}

/// The rules' view of the store before the write: `F-001`'s rules, run on this store.
struct RuleSnap<'a, S: Snapshot> {
    snap: &'a S,
    applied: Option<u64>,
    next_posting: u64,
}

impl<S: Snapshot> RuleView for RuleSnap<'_, S> {
    fn expected_seq(&self) -> Result<u64, StoreError> {
        Ok(self.applied.map_or(0, |a| a + 1))
    }
    fn expected_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.next_posting)
    }
    fn stored_account(&self, id: &AccountId) -> Result<Option<Account>, StoreError> {
        match read_leaf(self.snap, ACCOUNTS, &id.0).map_err(eng)? {
            Some((_, b)) => Ok(Some(decode_account(&b)?)),
            None => Ok(None),
        }
    }
    fn transfer_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        Ok(read_leaf(self.snap, TRANSFERS, &id.0)
            .map_err(eng)?
            .is_some())
    }
    fn journal_stored(&self) -> Result<bool, StoreError> {
        Ok(!self
            .snap
            .scan(Space::Leaf, &[JOURNALS.0], None, 1)
            .map_err(eng)?
            .is_empty())
    }
    fn external_id_in_slot(&self, slot: &[u8; 16]) -> Result<Option<[u8; 32]>, StoreError> {
        match read_leaf(self.snap, EXTERNAL_IDS, slot).map_err(eng)? {
            Some((_, b)) if b.len() == 32 => Ok(Some(array32(&b))),
            Some(_) => Err(StoreError::Corrupt("external id leaf".into())),
            None => Ok(None),
        }
    }
    fn voided_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        Ok(read_leaf(self.snap, VOIDED, &id.0).map_err(eng)?.is_some())
    }
    fn pending_stored(&self, id: &TransferId) -> Result<Option<PendingStatus>, StoreError> {
        match read_leaf(self.snap, PENDING, &id.0).map_err(eng)? {
            Some((_, b)) => match b.as_slice() {
                [code] => Ok(Some(decode_pending(*code)?)),
                _ => Err(StoreError::Corrupt("pending leaf".into())),
            },
            None => Ok(None),
        }
    }
}

impl<E: Engine> LeafStore for BookStore<E> {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        Ok(self.head.roots.kernel)
    }

    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        self.view()?.load(keys)
    }

    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        Ok(self.commit_native(seq, delta)?)
    }

    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        Ok(self.head.applied)
    }

    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.head.next_posting)
    }

    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        let Some(archive) = self.archive.as_ref() else {
            return Err(StoreError::Io("this store keeps no archive".into()));
        };
        let rec = archive
            .get(seq)
            .map_err(|e| StoreError::Io(format!("{e:?}")))?;
        Ok(rec.map(|r| LogRecord {
            seq: r.sequence(),
            delta: r.kernel_delta().clone(),
            root_after: r.roots_after().kernel_root(),
        }))
    }
}

/// One entry of an account's history (`REQ-PO-2`, `REQ-QR-5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The posting's place in the global sequence.
    pub posting_seq: u64,
    /// The entry's index within its transfer.
    pub index: u16,
    /// The transfer.
    pub transfer: TransferId,
    /// Debit or credit.
    pub direction: Direction,
    /// The kernel layer.
    pub layer: Layer,
}

/// A read-only view of the store pinned to one applied sequence (`REQ-QR-2`, `REQ-BA-3`).
pub struct ReadView<S: Snapshot> {
    snap: S,
    applied: Option<u64>,
    verify: bool,
}

impl<S: Snapshot> core::fmt::Debug for ReadView<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadView")
            .field("applied", &self.applied)
            .finish_non_exhaustive()
    }
}

fn corrupt(what: &str) -> StoreError {
    StoreError::Corrupt(what.to_string())
}

impl<S: Snapshot> ReadView<S> {
    fn new(snap: S, verify: bool) -> Result<Self, StoreError> {
        let applied = match snap.get(Space::Meta, META_APPLIED).map_err(eng)? {
            None => None,
            Some(b) if b.len() == 8 => {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                Some(u64::from_be_bytes(a))
            }
            Some(_) => return Err(corrupt("meta row")),
        };
        Ok(Self {
            snap,
            applied,
            verify,
        })
    }

    /// The applied sequence this view is pinned to.
    pub fn applied_sequence(&self) -> Option<u64> {
        self.applied
    }

    /// The seven roots, as of this view.
    pub fn roots(&self) -> Result<Roots, StoreError> {
        roots_of(&self.snap, empty_chain()).map_err(eng)
    }

    /// The snapshot below.
    pub fn snapshot(&self) -> &S {
        &self.snap
    }

    fn leaf(&self, tree: TreeId, key: &[u8; 16]) -> Result<Option<LeafRow>, StoreError> {
        read_leaf(&self.snap, tree, key).map_err(eng)
    }

    fn checked(
        &self,
        stored_hash: &[u8; 32],
        content: &[u8],
        what: &str,
    ) -> Result<(), StoreError> {
        if self.verify && smt_leaf_hash(content) != *stored_hash {
            return Err(StoreError::Corrupt(format!(
                "{what} leaf does not match its hash"
            )));
        }
        Ok(())
    }

    /// A kernel account.
    pub fn account(&self, id: &AccountId) -> Result<Option<Account>, StoreError> {
        match self.leaf(ACCOUNTS, &id.0)? {
            Some((h, b)) => {
                let a = decode_account(&b)?;
                self.checked(&h, &account_leaf_content(&a), "account")?;
                Ok(Some(a))
            }
            None => Ok(None),
        }
    }

    /// A kernel transfer.
    pub fn transfer(&self, id: &TransferId) -> Result<Option<Transfer>, StoreError> {
        match self.leaf(TRANSFERS, &id.0)? {
            Some((h, b)) => {
                let t = decode_transfer(&b)?;
                self.checked(&h, &transfer_leaf_content(&t), "transfer")?;
                Ok(Some(t))
            }
            None => Ok(None),
        }
    }

    fn journal(&self, id: &[u8; 16]) -> Result<Option<Journal>, StoreError> {
        match self.leaf(JOURNALS, id)? {
            Some((h, b)) => {
                let j = decode_journal(&b)?;
                self.checked(&h, &journal_leaf_content(&j), "journal")?;
                Ok(Some(j))
            }
            None => Ok(None),
        }
    }

    /// Answer every key of `keys`, present or absent (`F-001`'s `LeafStore::load`).
    pub fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        let mut out = Loaded::default();
        for id in &keys.accounts {
            out.accounts.insert(*id, self.account(id)?);
        }
        for id in &keys.transfers {
            out.transfers.insert(*id, self.transfer(id)?);
        }
        for id in &keys.journals {
            out.journals.insert(*id, self.journal(&id.0)?);
        }
        for eid in &keys.external_ids {
            let slot = external_id_key(&eid.0);
            let seen = match self.leaf(EXTERNAL_IDS, &slot)? {
                None => false,
                Some((h, b)) if b.as_slice() == eid.0.as_slice() => {
                    self.checked(&h, &external_id_leaf_content(&eid.0), "external id")?;
                    true
                }
                Some(_) => return Err(StoreError::ExternalIdCollision { slot }),
            };
            out.external_ids.insert(ExternalId(eid.0), seen);
        }
        for id in &keys.voided {
            out.voided.insert(*id, self.leaf(VOIDED, &id.0)?.is_some());
        }
        for id in &keys.pending {
            let p = match self.leaf(PENDING, &id.0)? {
                Some((_, b)) => match b.as_slice() {
                    [code] => Some(decode_pending(*code)?),
                    _ => return Err(corrupt("pending leaf")),
                },
                None => None,
            };
            out.pending.insert(*id, p);
        }
        Ok(out)
    }

    /// A service leaf.
    pub fn service_leaf(
        &self,
        kind: ServiceKind,
        id: &[u8; 16],
    ) -> Result<Option<ServiceLeaf>, StoreError> {
        self.service_leaf_at(&service_key(kind, id))
    }

    /// Every service leaf of `kind`, in key order: the kind's whole subtree (`F-005`).
    pub fn service_leaves_of(&self, kind: ServiceKind) -> Result<Vec<ServiceLeaf>, StoreError> {
        let mut out = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let rows = self
                .snap
                .scan(
                    Space::Leaf,
                    &[TreeId::SERVICE.0, kind.code()],
                    after.as_deref(),
                    4096,
                )
                .map_err(eng)?;
            if rows.is_empty() {
                return Ok(out);
            }
            for (k, v) in &rows {
                if k.len() != 17 || v.len() < 32 {
                    return Err(corrupt("leaf row"));
                }
                if v.len() == 32 && v[..32] == SMT_EMPTY_LEAF {
                    continue;
                }
                let l = ServiceLeaf::decode(&v[32..])
                    .map_err(|e| StoreError::Corrupt(format!("service leaf: {e:?}")))?;
                self.checked(&array32(&v[..32]), &l.leaf_content(), "service")?;
                out.push(l);
            }
            after = rows.last().map(|(k, _)| k.clone());
        }
    }

    /// The service leaf at a key.
    pub fn service_leaf_at(&self, key: &[u8; 16]) -> Result<Option<ServiceLeaf>, StoreError> {
        match self.leaf(TreeId::SERVICE, key)? {
            Some((h, b)) => {
                let l = ServiceLeaf::decode(&b)
                    .map_err(|e| StoreError::Corrupt(format!("service leaf: {e:?}")))?;
                self.checked(&h, &l.leaf_content(), "service")?;
                Ok(Some(l))
            }
            None => Ok(None),
        }
    }

    /// The proof of `key` in `tree` against this view's root: CipherClerk's [`SmtProof`].
    pub fn proof(&self, tree: TreeId, key: &[u8; 16]) -> Result<SmtProof, StoreError> {
        trie::prove(&self.snap, tree, empty_chain(), key).map_err(eng)
    }

    /// The multiproof frontier of `keys` in `tree`.
    pub fn frontier(&self, tree: TreeId, keys: &[[u8; 16]]) -> Result<Vec<Frontier>, StoreError> {
        trie::frontier(&self.snap, tree, empty_chain(), keys).map_err(eng)
    }

    /// The v2 proof of `key` in `tree` (`DEC-25`; a v2 build's trees only).
    pub fn proof_v2(
        &self,
        tree: TreeId,
        key: &[u8; 16],
    ) -> Result<ledger_kernel::compact_smt::v2::Proof, StoreError> {
        trie::prove_v2(&self.snap, tree, key).map_err(eng)
    }

    /// The v2 multiproof of `keys` in `tree`, node entries opened if `open` (experiment
    /// `DEC-25`; a v2 build's trees only).
    pub fn multiproof(
        &self,
        tree: TreeId,
        keys: &[[u8; 16]],
        open: bool,
    ) -> Result<ledger_kernel::compact_smt::v2::MultiProof, StoreError> {
        trie::multiproof(&self.snap, tree, keys, open).map_err(eng)
    }

    /// Up to `limit` entries of a kernel account's history in posting order, after
    /// `(posting seq, entry index)` when given: a cursor page (`REQ-QR-5`).
    pub fn history(
        &self,
        account: &AccountId,
        after: Option<(u64, u16)>,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>, StoreError> {
        let after_key = after.map(|(s, i)| cat(&[&account.0, &s.to_be_bytes(), &i.to_be_bytes()]));
        let rows = self
            .snap
            .scan(Space::History, &account.0, after_key.as_deref(), limit)
            .map_err(eng)?;
        rows.into_iter()
            .map(|(k, v)| {
                if k.len() != 26 || v.len() != 18 {
                    return Err(corrupt("history row"));
                }
                let mut seq = [0u8; 8];
                seq.copy_from_slice(&k[16..24]);
                let mut idx = [0u8; 2];
                idx.copy_from_slice(&k[24..26]);
                let mut t = [0u8; 16];
                t.copy_from_slice(&v[..16]);
                Ok(HistoryEntry {
                    posting_seq: u64::from_be_bytes(seq),
                    index: u16::from_be_bytes(idx),
                    transfer: TransferId(t),
                    direction: match v[16] {
                        0 => Direction::Debit,
                        1 => Direction::Credit,
                        _ => return Err(corrupt("history side")),
                    },
                    layer: layer_of(v[17]).ok_or_else(|| corrupt("history layer"))?,
                })
            })
            .collect()
    }

    /// The transfer holding posting `seq` of the global sequence (`REQ-PO-2`).
    pub fn posting(&self, seq: u64) -> Result<Option<TransferId>, StoreError> {
        match self
            .snap
            .get(Space::Posting, &seq.to_be_bytes())
            .map_err(eng)?
        {
            Some(b) if b.len() == 16 => {
                let mut t = [0u8; 16];
                t.copy_from_slice(&b);
                Ok(Some(TransferId(t)))
            }
            Some(_) => Err(corrupt("posting row")),
            None => Ok(None),
        }
    }

    /// The ledger accounts an identity owns.
    pub fn accounts_of(&self, identity: &[u8; 16]) -> Result<Vec<[u8; 16]>, StoreError> {
        let rows = self
            .snap
            .scan(Space::Owned, identity, None, usize::MAX)
            .map_err(eng)?;
        Ok(rows.into_iter().map(|(k, _)| array16(&k[16..32])).collect())
    }

    /// The holders of authority over an account, with the grant each holds it by (`INV-16`).
    pub fn holders_of(&self, account: &[u8; 16]) -> Result<Vec<HolderGrant>, StoreError> {
        let rows = self
            .snap
            .scan(Space::Holder, account, None, usize::MAX)
            .map_err(eng)?;
        Ok(rows
            .into_iter()
            .map(|(k, _)| (array16(&k[16..32]), array16(&k[32..48])))
            .collect())
    }

    /// The reserved total of a ledger account in an asset, in minor units (`REQ-BA-6`): one point
    /// read, whatever the account's history.
    pub fn reserved(&self, account: &[u8; 16], asset: u32) -> Result<u64, StoreError> {
        match self
            .snap
            .get(Space::Reserved, &reserved_key(account, asset))
            .map_err(eng)?
        {
            None => Ok(0),
            Some(b) if b.len() == 8 => {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                Ok(u64::from_be_bytes(a))
            }
            Some(_) => Err(corrupt("reserved row")),
        }
    }

    /// Visit every leaf of a tree in key order, with its stored bytes.
    pub fn for_each_leaf(&self, tree: TreeId, visit: &mut LeafVisit<'_>) -> Result<(), StoreError> {
        let mut after: Option<Vec<u8>> = None;
        loop {
            let rows = self
                .snap
                .scan(Space::Leaf, &[tree.0], after.as_deref(), 4096)
                .map_err(eng)?;
            if rows.is_empty() {
                return Ok(());
            }
            for (k, v) in &rows {
                if k.len() != 17 || v.len() < 32 {
                    return Err(corrupt("leaf row"));
                }
                if v.len() == 32 && v[..32] == SMT_EMPTY_LEAF {
                    // An emptied service leaf.
                    continue;
                }
                visit(&array16(&k[1..]), &v[32..])?;
            }
            after = rows.last().map(|(k, _)| k.clone());
        }
    }

    /// Visit every row of an index space, in key order.
    pub fn for_each_row(&self, space: Space, visit: &mut RowVisit<'_>) -> Result<(), StoreError> {
        let mut after: Option<Vec<u8>> = None;
        loop {
            let rows = self
                .snap
                .scan(space, &[], after.as_deref(), 4096)
                .map_err(eng)?;
            if rows.is_empty() {
                return Ok(());
            }
            for (k, v) in &rows {
                visit(k, v)?;
            }
            after = rows.last().map(|(k, _)| k.clone());
        }
    }
}

fn array16(b: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(b);
    out
}

/// Every index row recomputed from leaves and the posting sequence alone, by full key: what the
/// live indexes must equal (`F-006` T-006-07).
pub fn recompute_indexes<S: Snapshot>(
    view: &ReadView<S>,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
    let mut out: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut postings: Vec<(u64, TransferId)> = Vec::new();
    view.for_each_row(Space::Posting, &mut |k, v| {
        if k.len() != 8 || v.len() != 16 {
            return Err(corrupt("posting row"));
        }
        let mut s = [0u8; 8];
        s.copy_from_slice(k);
        postings.push((u64::from_be_bytes(s), TransferId(array16(v))));
        Ok(())
    })?;
    for (seq, id) in postings {
        let t = view
            .transfer(&id)?
            .ok_or_else(|| corrupt("a posting names a missing transfer"))?;
        for (i, e) in t.entries.iter().enumerate() {
            let (k, v) = history_row(&e.account_id, seq, i as u16, &t.id, e.direction, e.layer);
            out.insert(crate::engine::full_key(Space::History, &k), v);
        }
    }
    let mut reserved: BTreeMap<[u8; 20], u128> = BTreeMap::new();
    view.for_each_leaf(TreeId::SERVICE, &mut |_, bytes| {
        let leaf = ServiceLeaf::decode(bytes)
            .map_err(|e| StoreError::Corrupt(format!("service leaf: {e:?}")))?;
        for f in leaf.facts() {
            match f {
                Fact::OwnedBy { identity } => {
                    out.insert(
                        crate::engine::full_key(Space::Owned, &cat(&[identity, &leaf.id()])),
                        Vec::new(),
                    );
                }
                Fact::Holder { account, holder } => {
                    out.insert(
                        crate::engine::full_key(
                            Space::Holder,
                            &cat(&[account, holder, &leaf.id()]),
                        ),
                        Vec::new(),
                    );
                }
                Fact::Reserves {
                    account,
                    asset,
                    amount,
                } => {
                    *reserved.entry(reserved_key(account, *asset)).or_default() +=
                        u128::from(amount.get());
                }
            }
        }
        Ok(())
    })?;
    for (k, total) in reserved {
        let total = u64::try_from(total).map_err(|_| corrupt("reserved total past u64"))?;
        out.insert(
            crate::engine::full_key(Space::Reserved, &k),
            total.to_be_bytes().to_vec(),
        );
    }
    Ok(out)
}

/// Every live index row, by full key.
pub fn live_indexes<S: Snapshot>(
    view: &ReadView<S>,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
    let mut out = BTreeMap::new();
    for space in [Space::History, Space::Owned, Space::Holder, Space::Reserved] {
        view.for_each_row(space, &mut |k, v| {
            out.insert(crate::engine::full_key(space, k), v.to_vec());
            Ok(())
        })?;
    }
    Ok(out)
}

#[allow(dead_code)]
const _: () = assert!(SMT_DEPTH == 128);
