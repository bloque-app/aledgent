// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The witness builder (`F-006` T-006-04, `DEC-4`, `REQ-RT-3`).
//!
//! A core that does not hold the leaves receives, per command, the leaves the command reads and
//! a proof that they are the book's: the **touched leaves** of the read set plus one multiproof
//! (a frontier) per sub-tree, against the roots the core committed. Cost `O(touched · log N)`,
//! independent of the book's size (`F-002` §6: 5.4–5.9 KB at 1 000 000 accounts).
//!
//! - [`read_set`] finds the keys a step reads, the same way `F-001`'s working set does; and
//!   [`Recording`] records every key an arbitrary run of the library loads through a
//!   [`LeafStore`], for the driver of `F-005` to find the exact read set of a command.
//! - [`Witness::build`] turns a read set into a witness from a [`ReadView`].
//! - [`Witness::verify`] is the core's side: it rebuilds `root_before` from the witness alone,
//!   runs the real kernel over exactly the witnessed leaves — any read outside them **fails
//!   closed**, it is never answered with a default — and derives `root_after` from the proofs.
//! - [`Witness::to_succinct`] emits the same witness as CipherClerk's own
//!   `SuccinctTransitionWitness`, byte-compatible with its `verify_transition`.
//! - [`ServiceWitness`] does the same for the service tree.
//!
//! The wire carrier of the witness into an Agent (`clerk-witness`'s `ClerkTransitionWitness`
//! layout, or a larger private input) is `F-005`'s: it needs the VOS binding, which waits on
//! issue #38. The frontiers and touched leaves here are exactly that layout's content.

use std::cell::RefCell;
use std::collections::BTreeMap;

use cipher_clerk::kernel::CreateAccount;
use cipher_clerk::merkle::{BatchProof, SMT_EMPTY_LEAF, smt_leaf_hash};

pub use crate::trie::batch_proof;
use cipher_clerk::state::{Opening, PendingStatus};
use cipher_clerk::state_root::{
    account_leaf_content, external_id_key, external_id_leaf_content, journal_leaf_content,
    pending_leaf_content, transfer_leaf_content, voided_leaf_content,
};
use cipher_clerk::{
    Account, EventStatus, Journal, OpeningsOracle, Transfer, apply_account_creations, apply_batch,
};
use ledger_core::delta::{ServiceDelta, ServiceLeaf};
use ledger_kernel::store::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, StoreError, SubRoots, composite_root,
};
use ledger_kernel::working_set::{UnloadedKey, WorkingSet, discover, referenced_accounts};

use crate::engine::Snapshot;
use crate::store::ReadView;
use crate::trie::{BatchProofMirror, Frontier, TreeId};

// CipherClerk's witness type has private fields: this mirror has the same fields in the same
// order, so its rkyv archive is the kernel's (checked by decoding it as the kernel's type).

#[derive(rkyv::Archive, rkyv::Serialize)]
struct SuccinctMirror {
    accounts_proof: BatchProofMirror,
    transfers_proof: BatchProofMirror,
    journals_proof: BatchProofMirror,
    external_ids_proof: BatchProofMirror,
    voided_proof: BatchProofMirror,
    pending_proof: BatchProofMirror,
    present_accounts: Vec<Account>,
    present_transfers: Vec<Transfer>,
    present_journals: Vec<Journal>,
    absent_accounts: Vec<[u8; 16]>,
    absent_transfers: Vec<[u8; 16]>,
    absent_journals: Vec<[u8; 16]>,
    present_external_ids: Vec<[u8; 32]>,
    absent_external_ids: Vec<[u8; 16]>,
    present_voided: Vec<[u8; 16]>,
    absent_voided: Vec<[u8; 16]>,
    present_pending: Vec<([u8; 16], PendingStatus)>,
    absent_pending: Vec<[u8; 16]>,
    oracle: OpeningsOracle,
    events: Vec<Transfer>,
    batch_seed_timestamp: u64,
}

// ---------------------------------------------------------------------------------------------

/// The keys one step reads, as `F-001`'s working set discovers them: the step's own keys, then
/// the accounts of any transfer it references.
pub fn read_set<S: Snapshot>(
    view: &ReadView<S>,
    creations: &[CreateAccount],
    events: &[Transfer],
) -> Result<KeySet, StoreError> {
    let mut keys = discover(creations, events);
    let loaded = view.load(&keys)?;
    keys.extend(&referenced_accounts(events, &loaded));
    Ok(keys)
}

/// A read-only [`LeafStore`] that records every key loaded through it. Running the library
/// natively over it yields the exact read set of a command (`F-006` §Witness builder).
pub struct Recording<'a, L: LeafStore> {
    inner: &'a L,
    seen: RefCell<KeySet>,
}

impl<'a, L: LeafStore> Recording<'a, L> {
    /// Record loads from `inner`.
    pub fn new(inner: &'a L) -> Self {
        Self {
            inner,
            seen: RefCell::new(KeySet::default()),
        }
    }

    /// Every key loaded so far.
    pub fn read_set(&self) -> KeySet {
        self.seen.borrow().clone()
    }
}

impl<L: LeafStore> LeafStore for Recording<'_, L> {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        self.inner.subroots()
    }
    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        self.seen.borrow_mut().extend(keys);
        self.inner.load(keys)
    }
    fn commit(&mut self, _: u64, _: &BookDelta) -> Result<SubRoots, StoreError> {
        Err(StoreError::Io("a recording store is read-only".into()))
    }
    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.inner.last_committed()
    }
    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        self.inner.next_posting_seq()
    }
    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        self.inner.log_record(seq)
    }
}

/// What the kernel runs in one step, besides the leaves: the creations and the chain, their
/// seeds, and the openings the kernel asks for (private inputs, `REQ-RT-6`).
#[derive(Clone, Debug, Default)]
pub struct StepInput {
    /// Account creations, applied first.
    pub creations: Vec<CreateAccount>,
    /// The creations' batch seed.
    pub creation_seed: u64,
    /// The chain.
    pub events: Vec<Transfer>,
    /// The chain's batch seed.
    pub batch_seed: u64,
    /// The openings of the entries and of the flagged accounts' nets.
    pub openings: Vec<Opening>,
}

/// Why a witness does not verify. The core fails closed on every one of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WitnessError {
    /// The witnessed leaves and proofs do not give `root_before`.
    RootBefore {
        /// The root they give.
        computed: [u8; 32],
    },
    /// The kernel read a key the witness does not prove.
    Unproven(UnloadedKey),
    /// A service key the witness does not prove.
    UnprovenService([u8; 16]),
    /// The run wrote a key the witness does not prove.
    UnprovenWrite,
    /// The kernel refused an event: a witness is only built for a step that applies.
    Refused {
        /// Whether the event was an account creation.
        creation: bool,
        /// Its position.
        index: usize,
        /// What the kernel said.
        status: EventStatus,
    },
    /// The store failed while building.
    Store(StoreError),
}

impl From<StoreError> for WitnessError {
    fn from(e: StoreError) -> Self {
        WitnessError::Store(e)
    }
}

/// A verified step: the root after, from the proofs alone, and the kernel's writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The composite root after the step.
    pub root_after: [u8; 32],
    /// What the step wrote.
    pub delta: BookDelta,
}

/// CipherClerk's own witness for the chain of a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Succinct {
    /// The archive of a `SuccinctTransitionWitness`.
    pub bytes: Vec<u8>,
    /// The root the chain starts from: `root_before` with the step's creations applied (the
    /// kernel's type covers `apply_batch` only; creations are checked beside it).
    pub chain_root_before: [u8; 32],
}

/// A witness for one step (module documentation).
#[derive(Clone, Debug)]
pub struct Witness {
    frontiers: [Vec<Frontier>; 6],
    pre: Loaded,
    input: StepInput,
}

/// The sub-tree keys of a key set, in composite order (external ids as their slots).
pub fn keys_of(k: &KeySet) -> [Vec<[u8; 16]>; 6] {
    [
        k.accounts.iter().map(|x| x.0).collect(),
        k.transfers.iter().map(|x| x.0).collect(),
        k.journals.iter().map(|x| x.0).collect(),
        k.external_ids
            .iter()
            .map(|x| external_id_key(&x.0))
            .collect(),
        k.voided.iter().map(|x| x.0).collect(),
        k.pending.iter().map(|x| x.0).collect(),
    ]
}

type Leaves = [Vec<([u8; 16], [u8; 32])>; 6];

/// The leaf hash of every key of `loaded`, per sub-tree, sorted by key; absent keys take the
/// empty sentinel.
fn leaf_hashes(loaded: &Loaded) -> Leaves {
    let hash = |content: Option<Vec<u8>>| content.map_or(SMT_EMPTY_LEAF, |c| smt_leaf_hash(&c));
    let mut out: Leaves = [
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

/// Whether every key `delta` writes is one `pre` proves.
fn writes_are_proven(pre: &Loaded, delta: &BookDelta) -> bool {
    delta.accounts.keys().all(|k| pre.accounts.contains_key(k))
        && delta
            .transfers
            .keys()
            .all(|k| pre.transfers.contains_key(k))
        && delta.journals.keys().all(|k| pre.journals.contains_key(k))
        && delta
            .external_ids
            .iter()
            .all(|k| pre.external_ids.contains_key(k))
        && delta.voided.iter().all(|k| pre.voided.contains_key(k))
        && delta.pending.keys().all(|k| pre.pending.contains_key(k))
}

/// `pre` with `delta` written over it.
fn overlay(pre: &Loaded, delta: &BookDelta) -> Loaded {
    let mut post = pre.clone();
    for (k, a) in &delta.accounts {
        post.accounts.insert(*k, Some(a.clone()));
    }
    for (k, t) in &delta.transfers {
        post.transfers.insert(*k, Some(t.clone()));
    }
    for (k, j) in &delta.journals {
        post.journals.insert(*k, Some(j.clone()));
    }
    for e in &delta.external_ids {
        post.external_ids.insert(*e, true);
    }
    for v in &delta.voided {
        post.voided.insert(*v, true);
    }
    for (k, s) in &delta.pending {
        post.pending.insert(*k, Some(*s));
    }
    post
}

impl Witness {
    /// Build the witness of a step from a view: the leaves of `read_set` as the view holds them,
    /// and one frontier per sub-tree.
    pub fn build<S: Snapshot>(
        view: &ReadView<S>,
        read_set: &KeySet,
        input: StepInput,
    ) -> Result<Self, StoreError> {
        let pre = view.load(read_set)?;
        let keys = keys_of(read_set);
        let mut frontiers: [Vec<Frontier>; 6] = Default::default();
        for (i, f) in frontiers.iter_mut().enumerate() {
            *f = view.frontier(TreeId(i as u8), &keys[i])?;
        }
        Ok(Self {
            frontiers,
            pre,
            input,
        })
    }

    /// The witnessed pre-state leaves.
    pub fn leaves(&self) -> &Loaded {
        &self.pre
    }

    /// The frontier nodes carried, per sub-tree.
    pub fn frontier_sizes(&self) -> [usize; 6] {
        core::array::from_fn(|i| self.frontiers[i].len())
    }

    fn proofs(&self) -> [BatchProof; 6] {
        core::array::from_fn(|i| batch_proof(&self.frontiers[i]))
    }

    fn root_of(proofs: &[BatchProof; 6], loaded: &Loaded) -> [u8; 32] {
        let leaves = leaf_hashes(loaded);
        let sub: SubRoots = core::array::from_fn(|i| proofs[i].root(&leaves[i]));
        composite_root(&sub)
    }

    /// Tamper with a witnessed account, for tests of the verifier.
    #[cfg(all(test, feature = "smt-v1"))]
    pub(crate) fn leaves_mut(&mut self) -> &mut Loaded {
        &mut self.pre
    }

    /// Tamper with a frontier, for tests of the verifier.
    #[cfg(all(test, feature = "smt-v1"))]
    pub(crate) fn frontier_mut(&mut self, tree: usize) -> &mut Vec<Frontier> {
        &mut self.frontiers[tree]
    }

    /// Verify the step against `root_before` and derive `root_after` from the proofs alone
    /// (module documentation). Fails closed.
    pub fn verify(&self, root_before: [u8; 32]) -> Result<Verified, WitnessError> {
        let proofs = self.proofs();
        let computed = Self::root_of(&proofs, &self.pre);
        if computed != root_before {
            return Err(WitnessError::RootBefore { computed });
        }
        let delta = self.run(root_before)?;
        if !writes_are_proven(&self.pre, &delta) {
            return Err(WitnessError::UnprovenWrite);
        }
        let post = overlay(&self.pre, &delta);
        Ok(Verified {
            root_after: Self::root_of(&proofs, &post),
            delta,
        })
    }

    /// Run the kernel over the witnessed leaves only.
    fn run(&self, root_before: [u8; 32]) -> Result<BookDelta, WitnessError> {
        let mut ws = WorkingSet::new(self.pre.clone(), root_before);
        let mut oracle = OpeningsOracle::new(self.input.openings.clone());
        if !self.input.creations.is_empty() {
            let r = apply_account_creations(
                &mut ws,
                &self.input.creations,
                &mut oracle,
                self.input.creation_seed,
            );
            if let Some(miss) = ws.miss() {
                return Err(WitnessError::Unproven(miss));
            }
            if let Some((index, r)) = r
                .iter()
                .enumerate()
                .find(|(_, r)| r.status != EventStatus::Created)
            {
                return Err(WitnessError::Refused {
                    creation: true,
                    index,
                    status: r.status,
                });
            }
        }
        if !self.input.events.is_empty() {
            let r = apply_batch(
                &mut ws,
                &self.input.events,
                &mut oracle,
                self.input.batch_seed,
            );
            if let Some(miss) = ws.miss() {
                return Err(WitnessError::Unproven(miss));
            }
            if let Some((index, r)) = r
                .iter()
                .enumerate()
                .find(|(_, r)| r.status != EventStatus::Created)
            {
                return Err(WitnessError::Refused {
                    creation: false,
                    index,
                    status: r.status,
                });
            }
        }
        ws.into_delta().map_err(WitnessError::Unproven)
    }

    /// The chain of this step as CipherClerk's `SuccinctTransitionWitness`, starting from the
    /// state after the step's creations.
    pub fn to_succinct(&self) -> Result<Succinct, WitnessError> {
        let proofs = self.proofs();
        let mut chain_pre = self.pre.clone();
        if !self.input.creations.is_empty() {
            let root_before = Self::root_of(&proofs, &self.pre);
            let mut ws = WorkingSet::new(self.pre.clone(), root_before);
            let mut oracle = OpeningsOracle::new(self.input.openings.clone());
            let r = apply_account_creations(
                &mut ws,
                &self.input.creations,
                &mut oracle,
                self.input.creation_seed,
            );
            if let Some((index, r)) = r
                .iter()
                .enumerate()
                .find(|(_, r)| r.status != EventStatus::Created)
            {
                return Err(WitnessError::Refused {
                    creation: true,
                    index,
                    status: r.status,
                });
            }
            let created = ws.into_delta().map_err(WitnessError::Unproven)?;
            chain_pre = overlay(&self.pre, &created);
        }
        let chain_root_before = Self::root_of(&proofs, &chain_pre);
        let p = &chain_pre;
        let mirror = SuccinctMirror {
            accounts_proof: BatchProofMirror::new(&self.frontiers[0]),
            transfers_proof: BatchProofMirror::new(&self.frontiers[1]),
            journals_proof: BatchProofMirror::new(&self.frontiers[2]),
            external_ids_proof: BatchProofMirror::new(&self.frontiers[3]),
            voided_proof: BatchProofMirror::new(&self.frontiers[4]),
            pending_proof: BatchProofMirror::new(&self.frontiers[5]),
            present_accounts: p.accounts.values().flatten().cloned().collect(),
            present_transfers: p.transfers.values().flatten().cloned().collect(),
            present_journals: p.journals.values().flatten().cloned().collect(),
            absent_accounts: p
                .accounts
                .iter()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| k.0)
                .collect(),
            absent_transfers: p
                .transfers
                .iter()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| k.0)
                .collect(),
            absent_journals: p
                .journals
                .iter()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| k.0)
                .collect(),
            present_external_ids: p
                .external_ids
                .iter()
                .filter(|(_, s)| **s)
                .map(|(e, _)| e.0)
                .collect(),
            absent_external_ids: p
                .external_ids
                .iter()
                .filter(|(_, s)| !**s)
                .map(|(e, _)| external_id_key(&e.0))
                .collect(),
            present_voided: p
                .voided
                .iter()
                .filter(|(_, v)| **v)
                .map(|(k, _)| k.0)
                .collect(),
            absent_voided: p
                .voided
                .iter()
                .filter(|(_, v)| !**v)
                .map(|(k, _)| k.0)
                .collect(),
            present_pending: p
                .pending
                .iter()
                .filter_map(|(k, s)| s.map(|s| (k.0, s)))
                .collect(),
            absent_pending: p
                .pending
                .iter()
                .filter(|(_, s)| s.is_none())
                .map(|(k, _)| k.0)
                .collect(),
            oracle: OpeningsOracle::new(self.input.openings.clone()),
            events: self.input.events.clone(),
            batch_seed_timestamp: self.input.batch_seed,
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&mirror)
            .expect("archiving a witness only fails on allocator exhaustion")
            .to_vec();
        Ok(Succinct {
            bytes,
            chain_root_before,
        })
    }
}

/// A witness over the service tree: the touched leaves and their frontier (`F-006` T-006-03).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWitness {
    frontier: Vec<Frontier>,
    touched: BTreeMap<[u8; 16], Option<ServiceLeaf>>,
}

impl ServiceWitness {
    /// Build it for `keys` from a view.
    pub fn build<S: Snapshot>(view: &ReadView<S>, keys: &[[u8; 16]]) -> Result<Self, StoreError> {
        let mut touched = BTreeMap::new();
        for k in keys {
            touched.insert(*k, view.service_leaf_at(k)?);
        }
        Ok(Self {
            frontier: view.frontier(TreeId::SERVICE, keys)?,
            touched,
        })
    }

    fn root_with(&self, touched: &BTreeMap<[u8; 16], Option<ServiceLeaf>>) -> [u8; 32] {
        let leaves: Vec<([u8; 16], [u8; 32])> = touched
            .iter()
            .map(|(k, l)| {
                (
                    *k,
                    l.as_ref().map_or(SMT_EMPTY_LEAF, ServiceLeaf::leaf_hash),
                )
            })
            .collect();
        batch_proof(&self.frontier).root(&leaves)
    }

    /// The service root the witness proves.
    pub fn root(&self) -> [u8; 32] {
        self.root_with(&self.touched)
    }

    /// A witnessed leaf; a key the witness does not cover fails closed.
    pub fn get(&self, key: &[u8; 16]) -> Result<Option<&ServiceLeaf>, WitnessError> {
        self.touched
            .get(key)
            .map(Option::as_ref)
            .ok_or(WitnessError::UnprovenService(*key))
    }

    /// The service root after `delta`, from the proofs alone. Every key it puts must be
    /// witnessed.
    pub fn root_after(&self, delta: &ServiceDelta) -> Result<[u8; 32], WitnessError> {
        let mut post = self.touched.clone();
        for (k, leaf) in delta.puts() {
            match post.get_mut(k) {
                Some(slot) => *slot = Some(leaf.clone()),
                None => return Err(WitnessError::UnprovenService(*k)),
            }
        }
        for (k, _) in delta.removes() {
            match post.get_mut(k) {
                Some(slot) => *slot = None,
                None => return Err(WitnessError::UnprovenService(*k)),
            }
        }
        Ok(self.root_with(&post))
    }
}
