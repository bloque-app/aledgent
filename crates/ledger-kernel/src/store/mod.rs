// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The storage trait and its write rules (`F-001` §4, `DEC-1`, `DEC-4`).
//!
//! CipherClerk's own `LedgerState` is keyed by primary key, infallible, and
//! roots over six sparse Merkle trees. [`LeafStore`] is aledgent's storage
//! trait instead: fallible, batch-shaped, and it enforces the write rules of
//! `INV-2` itself, so a store refuses an overwrite no matter who asks.
//!
//! | Sub-tree | Key (16 B) | Write rule the store enforces |
//! |---|---|---|
//! | accounts | `AccountId` | insert, then overwrite of balances, timestamp and `CLOSED` only. No delete |
//! | transfers | `TransferId` | insert once. Overwrite refused |
//! | journals | `JournalId` | exactly one, at bootstrap. Any later write refused (`INV-25`) |
//! | external ids | `external_id_key(eid)` | insert once. A slot holding a different id is a fatal error |
//! | voided | `TransferId` | insert once |
//! | pending | `TransferId` | `Pending`, then `Posted` or `Voided` only |
//!
//! There is no method that removes a leaf. Every commit is all-or-nothing:
//! a refused commit leaves the store as it was.
//!
//! Each commit also appends `(seq, delta, root_after)` to an append-only delta
//! log. The log is the source of truth `DEC-4` names; the leaves are a
//! projection of it.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use cipher_clerk::ids::ExternalId;
use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::composite_root_from_subroots;
use cipher_clerk::{Account, AccountId, Journal, JournalId, Transfer, TransferId};

pub mod codec;
#[cfg(feature = "std")]
pub mod file;
pub mod mem;
pub mod rules;
pub mod smt;
/// The write-rule suite every [`LeafStore`] must pass, for other stores' tests
/// (`F-006`). Feature `store-suite`; never a production dependency.
#[cfg(any(test, feature = "store-suite"))]
pub mod suite;

#[cfg(feature = "std")]
pub use file::{CommitProfile, FileStore, FileStoreOptions};
pub use mem::MemStore;

/// The six sub-trees of the book, in the kernel's composite order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SubTree {
    /// Kernel accounts.
    Accounts,
    /// Kernel transfers.
    Transfers,
    /// The journal (exactly one per book).
    Journals,
    /// External ids the kernel has accepted.
    ExternalIds,
    /// Transfers that have been voided.
    Voided,
    /// Lifecycle of pending transfers.
    Pending,
}

impl SubTree {
    /// All six, in composite order.
    pub const ALL: [SubTree; 6] = [
        SubTree::Accounts,
        SubTree::Transfers,
        SubTree::Journals,
        SubTree::ExternalIds,
        SubTree::Voided,
        SubTree::Pending,
    ];

    /// Position in [`SubRoots`].
    pub const fn index(self) -> usize {
        match self {
            SubTree::Accounts => 0,
            SubTree::Transfers => 1,
            SubTree::Journals => 2,
            SubTree::ExternalIds => 3,
            SubTree::Voided => 4,
            SubTree::Pending => 5,
        }
    }
}

/// The six sub-tree roots, indexed by [`SubTree::index`].
pub type SubRoots = [[u8; 32]; 6];

/// Fold six sub-roots into the book's composite root, exactly as CipherClerk
/// does (`composite_root_from_subroots`), in the build's root version
/// (`compact_smt::Version::ACTIVE`; v2 is `DEC-25`).
pub fn composite_root(sub: &SubRoots) -> [u8; 32] {
    composite_root_in(compact_smt::Version::ACTIVE, sub)
}

/// [`composite_root`] in a given root version.
pub fn composite_root_in(version: compact_smt::Version, sub: &SubRoots) -> [u8; 32] {
    match version {
        compact_smt::Version::V1 => {
            composite_root_from_subroots(&sub[0], &sub[1], &sub[2], &sub[3], &sub[4], &sub[5])
        }
        compact_smt::Version::V2 => compact_smt::v2::composite_root(sub),
    }
}

/// The composite root recomputed from scratch over every record, in `version`: CipherClerk's
/// own `composite_state_root` for v1, the same leaves under `compact_smt::v2` for v2. The
/// independent reference stores and exports are checked against. Same preconditions as
/// `composite_state_root` (accounts, transfers, voided and pending sorted by id).
pub fn recomputed_composite_root(
    version: compact_smt::Version,
    accounts: &[cipher_clerk::Account],
    transfers: &[cipher_clerk::Transfer],
    journal: Option<&cipher_clerk::Journal>,
    external_ids: &[[u8; 32]],
    voided: &[[u8; 16]],
    pending: &[([u8; 16], u8)],
) -> [u8; 32] {
    use cipher_clerk::merkle::smt_leaf_hash;
    use cipher_clerk::state_root as sr;
    if version == compact_smt::Version::V1 {
        return sr::composite_state_root(
            accounts,
            transfers,
            journal,
            external_ids,
            voided,
            pending,
        );
    }
    let sorted = |mut v: alloc::vec::Vec<([u8; 16], [u8; 32])>| {
        v.sort_unstable_by_key(|(k, _)| *k);
        compact_smt::v2::root_sorted(&v)
    };
    let sub: SubRoots = [
        sorted(
            accounts
                .iter()
                .map(|a| (a.id.0, smt_leaf_hash(&sr::account_leaf_content(a))))
                .collect(),
        ),
        sorted(
            transfers
                .iter()
                .map(|t| (t.id.0, smt_leaf_hash(&sr::transfer_leaf_content(t))))
                .collect(),
        ),
        sorted(
            journal
                .iter()
                .map(|j| (j.id.0, smt_leaf_hash(&sr::journal_leaf_content(j))))
                .collect(),
        ),
        sorted(
            external_ids
                .iter()
                .map(|e| {
                    (
                        sr::external_id_key(e),
                        smt_leaf_hash(&sr::external_id_leaf_content(e)),
                    )
                })
                .collect(),
        ),
        sorted(
            voided
                .iter()
                .map(|v| (*v, smt_leaf_hash(&sr::voided_leaf_content(v))))
                .collect(),
        ),
        sorted(
            pending
                .iter()
                .map(|(id, c)| (*id, smt_leaf_hash(&sr::pending_leaf_content(id, *c))))
                .collect(),
        ),
    ];
    compact_smt::v2::composite_root(&sub)
}

/// The six sub-roots of an empty book in the build's root version.
pub fn empty_subroots() -> SubRoots {
    [compact_smt::Version::ACTIVE.empty_root(); 6]
}

/// The keys one unit needs, per sub-tree. Built by the working set's key
/// discovery (`F-001` §5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeySet {
    /// Kernel accounts.
    pub accounts: BTreeSet<AccountId>,
    /// Kernel transfers.
    pub transfers: BTreeSet<TransferId>,
    /// Journals.
    pub journals: BTreeSet<JournalId>,
    /// External ids.
    pub external_ids: BTreeSet<ExternalId>,
    /// Voided markers.
    pub voided: BTreeSet<TransferId>,
    /// Pending statuses.
    pub pending: BTreeSet<TransferId>,
}

impl KeySet {
    /// Whether no key is asked for.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
            && self.transfers.is_empty()
            && self.journals.is_empty()
            && self.external_ids.is_empty()
            && self.voided.is_empty()
            && self.pending.is_empty()
    }

    /// Add every key of `other`.
    pub fn extend(&mut self, other: &KeySet) {
        self.accounts.extend(other.accounts.iter().copied());
        self.transfers.extend(other.transfers.iter().copied());
        self.journals.extend(other.journals.iter().copied());
        self.external_ids.extend(other.external_ids.iter().copied());
        self.voided.extend(other.voided.iter().copied());
        self.pending.extend(other.pending.iter().copied());
    }

    /// The keys of `self` that `loaded` does not answer yet.
    pub fn missing_from(&self, loaded: &Loaded) -> KeySet {
        KeySet {
            accounts: self
                .accounts
                .iter()
                .filter(|k| !loaded.accounts.contains_key(k))
                .copied()
                .collect(),
            transfers: self
                .transfers
                .iter()
                .filter(|k| !loaded.transfers.contains_key(k))
                .copied()
                .collect(),
            journals: self
                .journals
                .iter()
                .filter(|k| !loaded.journals.contains_key(k))
                .copied()
                .collect(),
            external_ids: self
                .external_ids
                .iter()
                .filter(|k| !loaded.external_ids.contains_key(k))
                .copied()
                .collect(),
            voided: self
                .voided
                .iter()
                .filter(|k| !loaded.voided.contains_key(k))
                .copied()
                .collect(),
            pending: self
                .pending
                .iter()
                .filter(|k| !loaded.pending.contains_key(k))
                .copied()
                .collect(),
        }
    }
}

/// The store's answer to a [`KeySet`]: one entry per key asked, present or
/// absent. An absent key is reported as `None` or `false`, never dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    /// Kernel accounts.
    pub accounts: BTreeMap<AccountId, Option<Account>>,
    /// Kernel transfers.
    pub transfers: BTreeMap<TransferId, Option<Transfer>>,
    /// Journals.
    pub journals: BTreeMap<JournalId, Option<Journal>>,
    /// Whether each external id has been accepted.
    pub external_ids: BTreeMap<ExternalId, bool>,
    /// Whether each transfer has been voided.
    pub voided: BTreeMap<TransferId, bool>,
    /// The pending status of each transfer, if it has one.
    pub pending: BTreeMap<TransferId, Option<PendingStatus>>,
}

impl Loaded {
    /// Merge another answer into this one.
    pub fn merge(&mut self, other: Loaded) {
        self.accounts.extend(other.accounts);
        self.transfers.extend(other.transfers);
        self.journals.extend(other.journals);
        self.external_ids.extend(other.external_ids);
        self.voided.extend(other.voided);
        self.pending.extend(other.pending);
    }
}

/// One applied transfer's place in the book's global, gapless posting
/// sequence (`REQ-PO-2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PostingRecord {
    /// The posting sequence number.
    pub seq: u64,
    /// The transfer that holds the posting.
    pub transfer: TransferId,
}

/// Everything one committed unit writes (`DEC-4`: working set in, delta out).
///
/// The maps are last-writer-wins by key and drain in key order, so the same
/// delta always produces the same bytes and the same roots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BookDelta {
    /// Accounts created or updated.
    pub accounts: BTreeMap<AccountId, Account>,
    /// Transfers added.
    pub transfers: BTreeMap<TransferId, Transfer>,
    /// Journals added. Only the bootstrap delta holds one.
    pub journals: BTreeMap<JournalId, Journal>,
    /// External ids accepted.
    pub external_ids: BTreeSet<ExternalId>,
    /// Transfers voided.
    pub voided: BTreeSet<TransferId>,
    /// Pending statuses set.
    pub pending: BTreeMap<TransferId, PendingStatus>,
    /// The posting sequence numbers of the transfers added, in order.
    pub postings: Vec<PostingRecord>,
}

impl BookDelta {
    /// Whether the delta writes nothing.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
            && self.transfers.is_empty()
            && self.journals.is_empty()
            && self.external_ids.is_empty()
            && self.voided.is_empty()
            && self.pending.is_empty()
            && self.postings.is_empty()
    }
}

/// One entry of the append-only delta log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    /// The commit sequence number.
    pub seq: u64,
    /// What the commit wrote.
    pub delta: BookDelta,
    /// The composite root after the commit.
    pub root_after: [u8; 32],
}

/// A write the store refuses under the rules of `INV-2` and `INV-25`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteRefusal {
    /// Commits are numbered 0, 1, 2… with no gap and no repeat.
    SeqOutOfOrder {
        /// The sequence number the store expects next.
        expected: u64,
        /// The one it was given.
        got: u64,
    },
    /// Posting sequence numbers continue the book's without a gap (`REQ-PO-2`).
    PostingSeqGap {
        /// The posting sequence number the store expects next.
        expected: u64,
        /// The one it was given.
        got: u64,
    },
    /// Every transfer added needs exactly one posting record, and every
    /// posting record names a transfer added in the same delta.
    PostingsMismatch,
    /// A transfer is written once and never overwritten.
    TransferExists(TransferId),
    /// An account overwrite may only roll balances, move the timestamp and set
    /// `CLOSED`; anything else is a rewrite.
    AccountRewritten(AccountId),
    /// The book has exactly one journal, written at bootstrap (`INV-25`).
    JournalExists,
    /// A bootstrap writes one journal, not several (`INV-25`).
    MoreThanOneJournal,
    /// An external id is accepted once, for ever (`REQ-KER-6`).
    ExternalIdExists(ExternalId),
    /// A transfer is voided at most once.
    VoidedExists(TransferId),
    /// A committed record must fit one row of the core's delta ring
    /// (`F-006` T-006-01: 64 KiB). Stores that archive a delta record per
    /// commit refuse a larger one rather than split it.
    RecordTooLarge {
        /// The encoded size of the record.
        bytes: u64,
        /// The limit.
        limit: u64,
    },
    /// A pending status moves `Pending → Posted | Voided` only.
    PendingTransition {
        /// The transfer.
        id: TransferId,
        /// Its status before, if any.
        from: Option<PendingStatus>,
        /// The status the write asked for.
        to: PendingStatus,
    },
}

/// Why a store operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The write breaks a rule; nothing was written.
    Refused(WriteRefusal),
    /// Two different external ids share one 16-byte slot. The store never
    /// aliases one for the other; this is fatal (as in clerk-ledger).
    ExternalIdCollision {
        /// The shared slot.
        slot: [u8; 16],
    },
    /// The storage engine failed.
    Io(String),
    /// Stored bytes do not decode, or disagree with their hash.
    Corrupt(String),
}

impl From<WriteRefusal> for StoreError {
    fn from(r: WriteRefusal) -> Self {
        StoreError::Refused(r)
    }
}

/// aledgent's storage trait for the book's leaves (`F-001` §4).
///
/// `MemStore` and `FileStore` implement it here; `F-006` implements it for
/// production. Implementations must enforce the write rules of the module
/// documentation in [`LeafStore::commit`], and must never offer a way to
/// remove a leaf.
pub trait LeafStore {
    /// The six sub-tree roots, in composite order.
    fn subroots(&self) -> Result<SubRoots, StoreError>;

    /// The composite root.
    fn root(&self) -> Result<[u8; 32], StoreError> {
        Ok(composite_root_in(self.version(), &self.subroots()?))
    }

    /// The root version the store's trees are hashed in. The default is the build's.
    fn version(&self) -> compact_smt::Version {
        compact_smt::Version::ACTIVE
    }

    /// Answer every key in `keys`; absent keys are reported, never dropped.
    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError>;

    /// Apply `delta` as commit number `seq`, append it to the delta log, and
    /// return the new sub-roots. All or nothing.
    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError>;

    /// The last committed sequence number, or `None` for an empty store.
    fn last_committed(&self) -> Result<Option<u64>, StoreError>;

    /// The posting sequence number the next applied transfer will take.
    fn next_posting_seq(&self) -> Result<u64, StoreError>;

    /// The delta-log record of commit `seq`, if it exists.
    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher_clerk::merkle::build_empty_chain;
    use cipher_clerk::state_root::composite_state_root;

    #[test]
    fn empty_subroots_fold_to_the_kernels_empty_root() {
        let empty = build_empty_chain()[cipher_clerk::merkle::SMT_DEPTH];
        let sub = [empty; 6];
        assert_eq!(
            composite_root_in(compact_smt::Version::V1, &sub),
            composite_state_root(&[], &[], None, &[], &[], &[])
        );
    }

    #[test]
    fn sub_tree_indices_follow_composite_order() {
        for (i, t) in SubTree::ALL.iter().enumerate() {
            assert_eq!(t.index(), i);
        }
    }
}
