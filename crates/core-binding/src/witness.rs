// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Witness verification inside the Agent (`T-005-04`, `DEC-4`, `REQ-RT-3`, `INV-2`).
//!
//! The Agent holds the kernel's six sub-roots, not its leaves. Each input brings a
//! [`KernelWitness`]: the leaves the input reads, as they stand, and one CipherClerk multiproof
//! (`BatchProof`) per sub-tree, built by `F-006`'s leaf store. [`WitnessStore`] is the
//! `LeafStore` the library runs over inside the Agent:
//!
//! - it is built only if the witnessed leaves and proofs give exactly the Agent's committed
//!   sub-roots (a tampered leaf or a stale root fails here, `T-005-04`);
//! - a load answers witnessed keys and **fails closed** on any other: the read is refused, the
//!   store remembers it, and the transition commits nothing ([`WitnessStore::missed`]);
//! - a commit runs `F-001`'s write rules over the witnessed leaves (`INV-2`), refuses a write to
//!   a key the witness does not prove, and derives the new sub-roots from the same proofs, so the
//!   Agent never needs a leaf it was not given.
//!
//! The witness's wire form is the kernel's own rkyv archives, as `F-006`'s is.
//!
//! **Root versions** (`DEC-25`). A proof is CipherClerk's `BatchProof` (v1)
//! or `compact_smt::v2::MultiProof` (v2): [`KernelProof`]. The store accepts only the build's
//! version. A v2 multiproof's node entries are opaque, which is sound here because a kernel
//! tree never loses a leaf (`INV-2`): the store's commits only insert and overwrite, so no
//! touched subtree goes from present to empty (`compact_smt::v2::MultiProof::root`).

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;

use cipher_clerk::ids::ExternalId;
use cipher_clerk::merkle::{SMT_EMPTY_LEAF, smt_leaf_hash};
use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::{
    account_leaf_content, external_id_key, external_id_leaf_content, journal_leaf_content,
    pending_leaf_content, transfer_leaf_content, voided_leaf_content,
};
use cipher_clerk::{Account, AccountId, Journal, JournalId, Transfer, TransferId};
use ledger_kernel::store::rules::{self, RuleView};
use ledger_kernel::store::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, StoreError, SubRoots, composite_root,
};

pub use proof::KernelProof;

/// rkyv's derive leaves the archived enum's resolver undocumented; nothing else here is.
mod proof {
    #![allow(missing_docs)]

    use cipher_clerk::merkle::BatchProof;
    use ledger_kernel::compact_smt::v2::PackedProof;

    /// One sub-tree's multiproof, in the root version it was built for (module documentation).
    #[derive(Clone, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
    pub enum KernelProof {
        /// CipherClerk's v1 multiproof.
        V1(BatchProof),
        /// The compact tree's multiproof, in its wire form.
        V2(PackedProof),
    }
}

impl KernelProof {
    /// The root over `touched` (sorted by key; the empty sentinel for an absent key), if this
    /// proof is of the build's root version and well formed.
    pub fn root(&self, touched: &[([u8; 16], [u8; 32])]) -> Option<[u8; 32]> {
        use ledger_kernel::compact_smt::Version;
        match (self, Version::ACTIVE) {
            (KernelProof::V1(p), Version::V1) => Some(p.root(touched)),
            (KernelProof::V2(p), Version::V2) => p.unpack().ok()?.root(touched).ok(),
            _ => None,
        }
    }
}

/// The leaves one input reads and their proofs (module documentation).
#[derive(Clone, Debug, Default, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct KernelWitness {
    /// One multiproof per sub-tree, in composite order.
    pub proofs: Vec<KernelProof>,
    /// Kernel accounts.
    pub accounts: Vec<(AccountId, Option<Account>)>,
    /// Transfers.
    pub transfers: Vec<(TransferId, Option<Transfer>)>,
    /// Journals.
    pub journals: Vec<(JournalId, Option<Journal>)>,
    /// External ids, and whether each is taken.
    pub external_ids: Vec<(ExternalId, bool)>,
    /// Voided markers.
    pub voided: Vec<(TransferId, bool)>,
    /// Pending statuses.
    pub pending: Vec<(TransferId, Option<PendingStatus>)>,
}

impl KernelWitness {
    /// The witness of `pre` (the leaves as they stand) with `proofs`, one per sub-tree.
    pub fn new(pre: &Loaded, proofs: Vec<KernelProof>) -> Self {
        KernelWitness {
            proofs,
            accounts: pre.accounts.iter().map(|(k, v)| (*k, v.clone())).collect(),
            transfers: pre.transfers.iter().map(|(k, v)| (*k, v.clone())).collect(),
            journals: pre.journals.iter().map(|(k, v)| (*k, v.clone())).collect(),
            external_ids: pre.external_ids.iter().map(|(k, v)| (*k, *v)).collect(),
            voided: pre.voided.iter().map(|(k, v)| (*k, *v)).collect(),
            pending: pre.pending.iter().map(|(k, v)| (*k, *v)).collect(),
        }
    }

    /// The witnessed leaves.
    pub fn loaded(&self) -> Loaded {
        Loaded {
            accounts: self.accounts.iter().cloned().collect(),
            transfers: self.transfers.iter().cloned().collect(),
            journals: self.journals.iter().cloned().collect(),
            external_ids: self.external_ids.iter().copied().collect(),
            voided: self.voided.iter().copied().collect(),
            pending: self.pending.iter().copied().collect(),
        }
    }

    /// Its bytes: the kernel's rkyv archive.
    pub fn to_bytes(&self) -> Vec<u8> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .expect("archiving a witness only fails on allocator exhaustion")
            .to_vec()
    }

    /// Read it back.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut aligned = rkyv::util::AlignedVec::<16>::with_capacity(bytes.len());
        aligned.extend_from_slice(bytes);
        rkyv::from_bytes::<KernelWitness, rkyv::rancor::Error>(&aligned)
            .map_err(|e| alloc::format!("witness: {e}"))
    }
}

/// The leaf hash of every key `loaded` holds, per sub-tree, sorted by key; absent keys take the
/// empty sentinel. The same leaves `F-006`'s store and the kernel hash.
pub fn leaf_hashes(loaded: &Loaded) -> [Vec<([u8; 16], [u8; 32])>; 6] {
    let hash = |content: Option<Vec<u8>>| content.map_or(SMT_EMPTY_LEAF, |c| smt_leaf_hash(&c));
    let mut out: [Vec<([u8; 16], [u8; 32])>; 6] = [
        loaded
            .accounts
            .iter()
            .map(|(k, a)| (k.0, hash(a.as_ref().map(account_leaf_content))))
            .collect(),
        loaded
            .transfers
            .iter()
            .map(|(k, t)| (k.0, hash(t.as_ref().map(transfer_leaf_content))))
            .collect(),
        loaded
            .journals
            .iter()
            .map(|(k, j)| (k.0, hash(j.as_ref().map(journal_leaf_content))))
            .collect(),
        loaded
            .external_ids
            .iter()
            .map(|(e, seen)| {
                (
                    external_id_key(&e.0),
                    hash(seen.then(|| external_id_leaf_content(&e.0))),
                )
            })
            .collect(),
        loaded
            .voided
            .iter()
            .map(|(k, v)| (k.0, hash(v.then(|| voided_leaf_content(&k.0)))))
            .collect(),
        loaded
            .pending
            .iter()
            .map(|(k, p)| (k.0, hash(p.map(|s| pending_leaf_content(&k.0, s.code())))))
            .collect(),
    ];
    for tree in &mut out {
        tree.sort_unstable_by_key(|(k, _)| *k);
    }
    out
}

/// Why a witness is refused before anything runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WitnessRefusal {
    /// It does not carry one proof per sub-tree.
    Shape,
    /// Its leaves and proofs do not give the Agent's sub-roots: a tampered leaf, a stale root.
    Roots {
        /// The sub-tree that disagrees.
        tree: usize,
    },
}

/// The leaf store the library runs over inside the Agent (module documentation).
pub struct WitnessStore {
    proofs: Vec<KernelProof>,
    leaves: Loaded,
    subroots: SubRoots,
    last: Option<u64>,
    next_posting: u64,
    missed: Cell<bool>,
    unproven_write: Cell<bool>,
}

impl WitnessStore {
    /// A store over `witness`, which must prove exactly `subroots`; the store's last commit and
    /// next posting sequence number are the Agent's.
    pub fn new(
        witness: KernelWitness,
        subroots: SubRoots,
        last: Option<u64>,
        next_posting: u64,
    ) -> Result<Self, WitnessRefusal> {
        if witness.proofs.len() != 6 {
            return Err(WitnessRefusal::Shape);
        }
        let leaves = witness.loaded();
        let hashes = leaf_hashes(&leaves);
        for (tree, proof) in witness.proofs.iter().enumerate() {
            if proof.root(&hashes[tree]) != Some(subroots[tree]) {
                return Err(WitnessRefusal::Roots { tree });
            }
        }
        Ok(WitnessStore {
            proofs: witness.proofs,
            leaves,
            subroots,
            last,
            next_posting,
            missed: Cell::new(false),
            unproven_write: Cell::new(false),
        })
    }

    /// Whether the library asked for a leaf the witness does not prove, or tried to write one.
    /// Either way the run is not the one the host witnessed, and the Agent commits nothing.
    pub fn missed(&self) -> bool {
        self.missed.get() || self.unproven_write.get()
    }

    fn miss(&self) -> StoreError {
        self.missed.set(true);
        StoreError::Io(String::from("a read outside the witness"))
    }

    fn proves(&self, delta: &BookDelta) -> bool {
        let l = &self.leaves;
        delta.accounts.keys().all(|k| l.accounts.contains_key(k))
            && delta.transfers.keys().all(|k| l.transfers.contains_key(k))
            && delta.journals.keys().all(|k| l.journals.contains_key(k))
            && delta
                .external_ids
                .iter()
                .all(|k| l.external_ids.contains_key(k))
            && delta.voided.iter().all(|k| l.voided.contains_key(k))
            && delta.pending.keys().all(|k| l.pending.contains_key(k))
    }
}

impl RuleView for WitnessStore {
    fn expected_seq(&self) -> Result<u64, StoreError> {
        Ok(self.last.map_or(0, |n| n + 1))
    }
    fn expected_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.next_posting)
    }
    fn stored_account(&self, id: &AccountId) -> Result<Option<Account>, StoreError> {
        match self.leaves.accounts.get(id) {
            Some(a) => Ok(a.clone()),
            None => Err(self.miss()),
        }
    }
    fn transfer_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        match self.leaves.transfers.get(id) {
            Some(t) => Ok(t.is_some()),
            None => Err(self.miss()),
        }
    }
    fn journal_stored(&self) -> Result<bool, StoreError> {
        // The book's one journal is always witnessed (`Book::open` loads it), and the kernel
        // tree holds no other (`INV-25`).
        Ok(self.leaves.journals.values().any(Option::is_some))
    }
    fn external_id_in_slot(&self, slot: &[u8; 16]) -> Result<Option<[u8; 32]>, StoreError> {
        // A present external id's leaf is its own content: the slot holds exactly that id.
        let found = self
            .leaves
            .external_ids
            .iter()
            .find(|(e, _)| external_id_key(&e.0) == *slot);
        match found {
            Some((e, true)) => Ok(Some(e.0)),
            Some((_, false)) => Ok(None),
            None => Err(self.miss()),
        }
    }
    fn voided_stored(&self, id: &TransferId) -> Result<bool, StoreError> {
        match self.leaves.voided.get(id) {
            Some(v) => Ok(*v),
            None => Err(self.miss()),
        }
    }
    fn pending_stored(&self, id: &TransferId) -> Result<Option<PendingStatus>, StoreError> {
        match self.leaves.pending.get(id) {
            Some(p) => Ok(*p),
            None => Err(self.miss()),
        }
    }
}

impl LeafStore for WitnessStore {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        Ok(self.subroots)
    }

    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        let l = &self.leaves;
        let mut out = Loaded::default();
        for k in &keys.accounts {
            out.accounts
                .insert(*k, l.accounts.get(k).ok_or_else(|| self.miss())?.clone());
        }
        for k in &keys.transfers {
            out.transfers
                .insert(*k, l.transfers.get(k).ok_or_else(|| self.miss())?.clone());
        }
        for k in &keys.journals {
            out.journals
                .insert(*k, l.journals.get(k).ok_or_else(|| self.miss())?.clone());
        }
        for k in &keys.external_ids {
            out.external_ids
                .insert(*k, *l.external_ids.get(k).ok_or_else(|| self.miss())?);
        }
        for k in &keys.voided {
            out.voided
                .insert(*k, *l.voided.get(k).ok_or_else(|| self.miss())?);
        }
        for k in &keys.pending {
            out.pending
                .insert(*k, *l.pending.get(k).ok_or_else(|| self.miss())?);
        }
        Ok(out)
    }

    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        if !self.proves(delta) {
            self.unproven_write.set(true);
            return Err(StoreError::Io(String::from("a write outside the witness")));
        }
        rules::check(self, seq, delta)?;
        let l = &mut self.leaves;
        for (k, a) in &delta.accounts {
            l.accounts.insert(*k, Some(a.clone()));
        }
        for (k, t) in &delta.transfers {
            l.transfers.insert(*k, Some(t.clone()));
        }
        for (k, j) in &delta.journals {
            l.journals.insert(*k, Some(j.clone()));
        }
        for e in &delta.external_ids {
            l.external_ids.insert(*e, true);
        }
        for v in &delta.voided {
            l.voided.insert(*v, true);
        }
        for (k, s) in &delta.pending {
            l.pending.insert(*k, Some(*s));
        }
        let hashes = leaf_hashes(&self.leaves);
        let mut sub: SubRoots = [[0; 32]; 6];
        for (i, s) in sub.iter_mut().enumerate() {
            // The proofs gave the roots before, so they are of this version and well formed;
            // the leaves after only add keys the witness proves.
            *s = self.proofs[i].root(&hashes[i]).ok_or_else(|| {
                StoreError::Io(String::from("a proof that gave a root no longer does"))
            })?;
        }
        self.subroots = sub;
        self.last = Some(seq);
        self.next_posting += delta.postings.len() as u64;
        Ok(sub)
    }

    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        Ok(self.last)
    }

    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.next_posting)
    }

    fn log_record(&self, _seq: u64) -> Result<Option<LogRecord>, StoreError> {
        // The Agent keeps no delta log: the host archives it. A command never reads it.
        Err(self.miss())
    }
}

/// The composite root of a set of sub-roots, for comparisons.
pub fn root_of(sub: &SubRoots) -> [u8; 32] {
    composite_root(sub)
}
