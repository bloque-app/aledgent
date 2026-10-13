// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The witnessed-leaves shape of `DEC-4`, built the way CipherClerk's
//! `SuccinctTransitionWitness` is (`cipher-clerk/src/succinct.rs`), but from a
//! compact node index instead of a full `VecLedger` snapshot.
//!
//! CipherClerk's own producer, `SuccinctTransitionWitness::from_full`, needs
//! the whole book in memory and re-hashes every untouched subtree: `O(book)`
//! per witness. `F-001`'s `FileStore` keeps a node table but offers no proof
//! read. So the spike keeps its own index ([`Tree`]): the sorted leaf hashes
//! of a sub-tree plus a memo of every subtree with two or more leaves. A
//! single-leaf subtree is folded up from its leaf on demand. Its roots are
//! asserted equal to `FileStore`'s, so its frontiers are the book's.
//!
//! The witness itself is CipherClerk's type. Its fields are private, so the
//! spike serialises a field-for-field mirror ([`WitnessMirror`]) with rkyv and
//! deserialises the bytes as `SuccinctTransitionWitness`: the archive layout
//! is determined by the field types in order, and a mismatch fails the
//! checked deserialisation or the kernel's own `verify_transition`.

use std::collections::{BTreeMap, HashMap};

use cipher_clerk::merkle::{BatchProof, SMT_DEPTH, SMT_EMPTY_LEAF, smt_leaf_hash, smt_node_hash};
use cipher_clerk::state::PendingStatus;
use cipher_clerk::state_root::{
    account_leaf_content, composite_root_from_subroots, external_id_key, external_id_leaf_content,
    journal_leaf_content, pending_leaf_content, transfer_leaf_content, voided_leaf_content,
};
use cipher_clerk::succinct::SuccinctTransitionWitness;
use cipher_clerk::{Account, Journal, OpeningsOracle, Transfer};
use ledger_kernel::store::{BookDelta, KeySet, Loaded, SubRoots};

type Key = [u8; 16];
type Hash = [u8; 32];

fn bit(key: &Key, level: usize) -> bool {
    (key[level / 8] >> (7 - level % 8)) & 1 == 1
}

fn with_bit(prefix: Key, level: usize) -> Key {
    let mut p = prefix;
    p[level / 8] |= 1 << (7 - level % 8);
    p
}

fn mask(key: &Key, bits: usize) -> Key {
    let mut out = [0u8; 16];
    let full = bits / 8;
    out[..full].copy_from_slice(&key[..full]);
    if full < 16 && bits % 8 != 0 {
        out[full] = key[full] & (0xFFu8 << (8 - bits % 8));
    }
    out
}

/// One frontier node: the hash of an untouched, non-empty subtree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frontier {
    /// Key bits fixed by the path (0 = the whole tree).
    pub level: u16,
    /// Those bits, MSB first, zero-padded.
    pub prefix: Key,
    /// The subtree's hash.
    pub hash: Hash,
}

/// A compact index of one sparse Merkle sub-tree (depth 128): the leaf
/// hashes in key order, and a memo of the hash of every subtree holding two
/// or more leaves. Updating a leaf forgets the memo along its path only.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    leaves: BTreeMap<Key, Hash>,
    memo: HashMap<(u16, Key), Hash>,
}

/// The largest key under `prefix` at `level`.
fn upper(prefix: &Key, level: usize) -> Key {
    let mut hi = *prefix;
    for bit_pos in level..SMT_DEPTH {
        hi[bit_pos / 8] |= 1 << (7 - bit_pos % 8);
    }
    hi
}

impl Tree {
    /// A tree over these (key, leaf hash) pairs.
    pub fn from_leaves(leaves: Vec<(Key, Hash)>) -> Self {
        Self {
            leaves: leaves.into_iter().collect(),
            memo: HashMap::new(),
        }
    }

    /// Number of leaves.
    pub fn len(&self) -> usize {
        self.leaves.len()
    }

    /// Whether the tree has no leaf.
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// Memoised internal nodes.
    pub fn memo_len(&self) -> usize {
        self.memo.len()
    }

    /// Set (insert or replace) one leaf and forget the memo along its path.
    pub fn set(&mut self, key: Key, leaf: Hash) {
        self.leaves.insert(key, leaf);
        for level in 0..=SMT_DEPTH {
            self.memo.remove(&(level as u16, mask(&key, level)));
        }
    }

    /// The root.
    pub fn root(&mut self, empty: &[Hash; SMT_DEPTH + 1]) -> Hash {
        self.subtree(0, [0; 16], empty)
    }

    fn fold(key: &Key, leaf: Hash, level: usize, empty: &[Hash; SMT_DEPTH + 1]) -> Hash {
        let mut h = leaf;
        for depth in (level..SMT_DEPTH).rev() {
            let sibling = empty[SMT_DEPTH - depth - 1];
            h = if bit(key, depth) {
                smt_node_hash(&sibling, &h)
            } else {
                smt_node_hash(&h, &sibling)
            };
        }
        h
    }

    /// The hash of the subtree at `level` under `prefix`.
    fn subtree(&mut self, level: usize, prefix: Key, empty: &[Hash; SMT_DEPTH + 1]) -> Hash {
        if let Some(h) = self.memo.get(&(level as u16, prefix)) {
            return *h;
        }
        let hi = upper(&prefix, level);
        let mut it = self.leaves.range(prefix..=hi);
        let first = it.next().map(|(k, h)| (*k, *h));
        let two = it.next().is_some();
        match (first, two) {
            (None, _) => empty[SMT_DEPTH - level],
            (Some((k, h)), false) => Self::fold(&k, h, level, empty),
            (Some(_), true) => {
                let lh = self.subtree(level + 1, prefix, empty);
                let rh = self.subtree(level + 1, with_bit(prefix, level), empty);
                let h = smt_node_hash(&lh, &rh);
                self.memo.insert((level as u16, prefix), h);
                h
            }
        }
    }

    fn non_empty(&self, level: usize, prefix: &Key) -> bool {
        self.leaves
            .range(*prefix..=upper(prefix, level))
            .next()
            .is_some()
    }

    /// The frontier for `touched` (any order): the hash of every untouched,
    /// non-empty subtree next to a touched path, in `(level, prefix)` order,
    /// exactly as `BatchProof::build` computes it.
    pub fn frontier(&mut self, touched: &[Key], empty: &[Hash; SMT_DEPTH + 1]) -> Vec<Frontier> {
        let mut t = touched.to_vec();
        t.sort_unstable();
        t.dedup();
        let mut out = Vec::new();
        self.frontier_rec(&t, [0; 16], 0, empty, &mut out);
        out.sort_unstable_by_key(|a| (a.level, a.prefix));
        out
    }

    fn frontier_rec(
        &mut self,
        touched: &[Key],
        prefix: Key,
        level: usize,
        empty: &[Hash; SMT_DEPTH + 1],
        out: &mut Vec<Frontier>,
    ) {
        if touched.is_empty() {
            if self.non_empty(level, &prefix) {
                let hash = self.subtree(level, prefix, empty);
                out.push(Frontier {
                    level: level as u16,
                    prefix,
                    hash,
                });
            }
            return;
        }
        if level == SMT_DEPTH {
            return;
        }
        let ts = touched.partition_point(|k| !bit(k, level));
        let (tl, tr) = touched.split_at(ts);
        self.frontier_rec(tl, prefix, level + 1, empty, out);
        self.frontier_rec(tr, with_bit(prefix, level), level + 1, empty, out);
    }
}

/// Leaf hashes by key, per sub-tree, while an index is being built.
pub type LeafMaps = [HashMap<Key, Hash>; 6];

/// The book's six sub-trees, in composite order.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// Accounts, transfers, journals, external ids, voided, pending.
    pub trees: [Tree; 6],
}

fn account_leaf(a: &Account) -> Hash {
    smt_leaf_hash(&account_leaf_content(a))
}

impl Model {
    /// Build the index from every delta of the book's log, in order
    /// (last writer wins per key, as in the store).
    pub fn from_deltas<'a>(deltas: impl Iterator<Item = &'a BookDelta>) -> Self {
        let mut maps = LeafMaps::default();
        for d in deltas {
            Self::fold_delta(&mut maps, d);
        }
        Self::from_maps(maps)
    }

    /// The index over leaf maps built with [`Model::fold_delta`].
    pub fn from_maps(maps: LeafMaps) -> Self {
        let trees = maps.map(|m| Tree::from_leaves(m.into_iter().collect()));
        Self { trees }
    }

    /// Fold one delta into leaf maps (last writer wins per key).
    pub fn fold_delta(maps: &mut LeafMaps, d: &BookDelta) {
        for (id, a) in &d.accounts {
            maps[0].insert(id.0, account_leaf(a));
        }
        for (id, t) in &d.transfers {
            maps[1].insert(id.0, smt_leaf_hash(&transfer_leaf_content(t)));
        }
        for (id, j) in &d.journals {
            maps[2].insert(id.0, smt_leaf_hash(&journal_leaf_content(j)));
        }
        for eid in &d.external_ids {
            maps[3].insert(
                external_id_key(&eid.0),
                smt_leaf_hash(&external_id_leaf_content(&eid.0)),
            );
        }
        for id in &d.voided {
            maps[4].insert(id.0, smt_leaf_hash(&voided_leaf_content(&id.0)));
        }
        for (id, s) in &d.pending {
            maps[5].insert(id.0, smt_leaf_hash(&pending_leaf_content(&id.0, s.code())));
        }
    }

    /// Apply one more committed delta.
    pub fn apply(&mut self, d: &BookDelta) {
        let mut maps = LeafMaps::default();
        Self::fold_delta(&mut maps, d);
        for (tree, m) in self.trees.iter_mut().zip(maps) {
            for (k, h) in m {
                tree.set(k, h);
            }
        }
    }

    /// The six sub-roots.
    pub fn subroots(&mut self, empty: &[Hash; SMT_DEPTH + 1]) -> SubRoots {
        let mut out = [[0u8; 32]; 6];
        for (o, t) in out.iter_mut().zip(self.trees.iter_mut()) {
            *o = t.root(empty);
        }
        out
    }

    /// The composite root.
    pub fn root(&mut self, empty: &[Hash; SMT_DEPTH + 1]) -> Hash {
        let s = self.subroots(empty);
        composite_root_from_subroots(&s[0], &s[1], &s[2], &s[3], &s[4], &s[5])
    }
}

/// The per-sub-tree keys of a key set, in composite order.
pub fn keys_of(k: &KeySet) -> [Vec<Key>; 6] {
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

// ---------------------------------------------------------------------------
// The mirror of CipherClerk's witness types (field order is the contract).

/// Mirror of `cipher_clerk::merkle::FrontierNode`.
#[derive(rkyv::Archive, rkyv::Serialize, Clone, Debug)]
pub struct FrontierNodeMirror {
    level: u16,
    prefix: [u8; 16],
    hash: [u8; 32],
}

/// Mirror of `cipher_clerk::merkle::BatchProof`.
#[derive(rkyv::Archive, rkyv::Serialize, Clone, Debug, Default)]
pub struct BatchProofMirror {
    frontier: Vec<FrontierNodeMirror>,
}

impl BatchProofMirror {
    /// From frontier nodes.
    pub fn new(f: &[Frontier]) -> Self {
        Self {
            frontier: f
                .iter()
                .map(|n| FrontierNodeMirror {
                    level: n.level,
                    prefix: n.prefix,
                    hash: n.hash,
                })
                .collect(),
        }
    }

    /// Frontier nodes carried.
    pub fn len(&self) -> usize {
        self.frontier.len()
    }

    /// Whether the proof carries no node.
    pub fn is_empty(&self) -> bool {
        self.frontier.is_empty()
    }

    /// As CipherClerk's own `BatchProof`, through its archive.
    pub fn to_kernel(&self) -> BatchProof {
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(self).expect("serialise proof");
        rkyv::from_bytes::<BatchProof, rkyv::rancor::Error>(&bytes).expect("proof layout")
    }
}

/// Mirror of `cipher_clerk::succinct::SuccinctTransitionWitness`.
#[derive(rkyv::Archive, rkyv::Serialize, Clone)]
pub struct WitnessMirror {
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

/// How big each part of a witness is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WitnessSize {
    /// Frontier nodes, per sub-tree.
    pub frontier_nodes: [usize; 6],
    /// Touched keys, per sub-tree.
    pub touched: [usize; 6],
    /// The whole archive, in bytes.
    pub bytes: usize,
    /// The frontier alone (six proofs), in bytes.
    pub proof_bytes: usize,
    /// The events alone, in bytes.
    pub event_bytes: usize,
    /// The oracle (openings) alone, in bytes.
    pub oracle_bytes: usize,
}

/// Everything a witness is made of, before serialisation.
pub struct WitnessParts {
    /// The six proofs.
    pub proofs: [BatchProofMirror; 6],
    /// The touched leaves as loaded from the store, pre-state.
    pub pre: Loaded,
    /// The openings the kernel will ask for.
    pub oracle: OpeningsOracle,
    /// The kernel events.
    pub events: Vec<Transfer>,
    /// The batch seed.
    pub seed: u64,
}

fn size_of<T>(v: &T) -> usize
where
    T: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                rkyv::rancor::Error,
            >,
        >,
{
    rkyv::to_bytes::<rkyv::rancor::Error>(v)
        .expect("serialise")
        .len()
}

impl WitnessParts {
    /// The frontiers for `touched`, from the index.
    pub fn proofs(
        model: &mut Model,
        touched: &KeySet,
        empty: &[Hash; SMT_DEPTH + 1],
    ) -> ([BatchProofMirror; 6], [usize; 6]) {
        let keys = keys_of(touched);
        let mut counts = [0usize; 6];
        let proofs: [BatchProofMirror; 6] = std::array::from_fn(|i| {
            let f = model.trees[i].frontier(&keys[i], empty);
            counts[i] = f.len();
            BatchProofMirror::new(&f)
        });
        (proofs, counts)
    }

    /// The mirror, ready to serialise.
    pub fn mirror(&self) -> WitnessMirror {
        let pre = &self.pre;
        let mut present_accounts = Vec::new();
        let mut absent_accounts = Vec::new();
        for (id, a) in &pre.accounts {
            match a {
                Some(a) => present_accounts.push(a.clone()),
                None => absent_accounts.push(id.0),
            }
        }
        let mut present_transfers = Vec::new();
        let mut absent_transfers = Vec::new();
        for (id, t) in &pre.transfers {
            match t {
                Some(t) => present_transfers.push(t.clone()),
                None => absent_transfers.push(id.0),
            }
        }
        let mut present_journals = Vec::new();
        let mut absent_journals = Vec::new();
        for (id, j) in &pre.journals {
            match j {
                Some(j) => present_journals.push(j.clone()),
                None => absent_journals.push(id.0),
            }
        }
        let mut present_external_ids = Vec::new();
        let mut absent_external_ids = Vec::new();
        for (eid, seen) in &pre.external_ids {
            if *seen {
                present_external_ids.push(eid.0);
            } else {
                absent_external_ids.push(external_id_key(&eid.0));
            }
        }
        let mut present_voided = Vec::new();
        let mut absent_voided = Vec::new();
        for (id, v) in &pre.voided {
            if *v {
                present_voided.push(id.0);
            } else {
                absent_voided.push(id.0);
            }
        }
        let mut present_pending = Vec::new();
        let mut absent_pending = Vec::new();
        for (id, p) in &pre.pending {
            match p {
                Some(s) => present_pending.push((id.0, *s)),
                None => absent_pending.push(id.0),
            }
        }
        let [a, t, j, e, v, p] = self.proofs.clone();
        WitnessMirror {
            accounts_proof: a,
            transfers_proof: t,
            journals_proof: j,
            external_ids_proof: e,
            voided_proof: v,
            pending_proof: p,
            present_accounts,
            present_transfers,
            present_journals,
            absent_accounts,
            absent_transfers,
            absent_journals,
            present_external_ids,
            absent_external_ids,
            present_voided,
            absent_voided,
            present_pending,
            absent_pending,
            oracle: self.oracle.clone(),
            events: self.events.clone(),
            batch_seed_timestamp: self.seed,
        }
    }

    /// Serialise: the bytes that would travel to the Agent.
    pub fn to_bytes(&self) -> Vec<u8> {
        rkyv::to_bytes::<rkyv::rancor::Error>(&self.mirror())
            .expect("serialise witness")
            .to_vec()
    }

    /// Sizes of the parts.
    pub fn size(&self, touched: &KeySet) -> WitnessSize {
        let keys = keys_of(touched);
        WitnessSize {
            frontier_nodes: std::array::from_fn(|i| self.proofs[i].len()),
            touched: std::array::from_fn(|i| keys[i].len()),
            bytes: self.to_bytes().len(),
            proof_bytes: self.proofs.iter().map(size_of).sum(),
            event_bytes: size_of(&self.events),
            oracle_bytes: size_of(&self.oracle),
        }
    }

    /// The root these proofs give for the given touched leaf hashes, per
    /// sub-tree, computed by CipherClerk's `BatchProof::root`.
    pub fn root_from_proofs(proofs: &[BatchProof; 6], leaves: &[Vec<(Key, Hash)>; 6]) -> Hash {
        let r: [Hash; 6] = std::array::from_fn(|i| {
            let mut l = leaves[i].clone();
            l.sort_unstable_by_key(|(k, _)| *k);
            proofs[i].root(&l)
        });
        composite_root_from_subroots(&r[0], &r[1], &r[2], &r[3], &r[4], &r[5])
    }
}

/// Decode witness bytes as CipherClerk's own type.
pub fn decode(bytes: &[u8]) -> SuccinctTransitionWitness {
    let mut aligned = rkyv::util::AlignedVec::<16>::with_capacity(bytes.len());
    aligned.extend_from_slice(bytes);
    rkyv::from_bytes::<SuccinctTransitionWitness, rkyv::rancor::Error>(&aligned)
        .expect("witness layout matches CipherClerk's")
}

/// The touched leaf hashes of `loaded` (pre- or post-state), per sub-tree.
pub fn leaf_hashes(loaded: &Loaded) -> [Vec<(Key, Hash)>; 6] {
    [
        loaded
            .accounts
            .iter()
            .map(|(k, a)| (k.0, a.as_ref().map_or(SMT_EMPTY_LEAF, account_leaf)))
            .collect(),
        loaded
            .transfers
            .iter()
            .map(|(k, t)| {
                (
                    k.0,
                    t.as_ref()
                        .map_or(SMT_EMPTY_LEAF, |t| smt_leaf_hash(&transfer_leaf_content(t))),
                )
            })
            .collect(),
        loaded
            .journals
            .iter()
            .map(|(k, j)| {
                (
                    k.0,
                    j.as_ref()
                        .map_or(SMT_EMPTY_LEAF, |j| smt_leaf_hash(&journal_leaf_content(j))),
                )
            })
            .collect(),
        loaded
            .external_ids
            .iter()
            .map(|(k, seen)| {
                (
                    external_id_key(&k.0),
                    if *seen {
                        smt_leaf_hash(&external_id_leaf_content(&k.0))
                    } else {
                        SMT_EMPTY_LEAF
                    },
                )
            })
            .collect(),
        loaded
            .voided
            .iter()
            .map(|(k, v)| {
                (
                    k.0,
                    if *v {
                        smt_leaf_hash(&voided_leaf_content(&k.0))
                    } else {
                        SMT_EMPTY_LEAF
                    },
                )
            })
            .collect(),
        loaded
            .pending
            .iter()
            .map(|(k, p)| {
                (
                    k.0,
                    p.map_or(SMT_EMPTY_LEAF, |s| {
                        smt_leaf_hash(&pending_leaf_content(&k.0, s.code()))
                    }),
                )
            })
            .collect(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher_clerk::merkle::{SparseMerkleTree, build_empty_chain};

    fn key(i: u64) -> Key {
        let h = cipher_clerk::crypto::blake2b_256(b"spike-key", &[&i.to_le_bytes()]);
        let mut k = [0u8; 16];
        k.copy_from_slice(&h[..16]);
        k
    }

    #[test]
    fn index_roots_and_frontiers_equal_the_kernels() {
        let empty = build_empty_chain();
        let mut reference = SparseMerkleTree::new();
        let mut leaves = Vec::new();
        for i in 0..500u64 {
            reference.insert(key(i), &i.to_le_bytes());
            leaves.push((key(i), smt_leaf_hash(&i.to_le_bytes())));
        }
        let mut tree = Tree::from_leaves(leaves);
        assert_eq!(tree.root(&empty), reference.root());

        // A frontier for two present keys and one absent key reconstructs the
        // root through CipherClerk's BatchProof, before and after an update.
        let touched = [key(3), key(250), key(9_999)];
        let f = tree.frontier(&touched, &empty);
        let proof = BatchProofMirror::new(&f).to_kernel();
        assert_eq!(proof, BatchProof::build(&reference, &touched));
        let mut before: Vec<(Key, Hash)> = touched
            .iter()
            .map(|k| (*k, reference.get(k).unwrap_or(SMT_EMPTY_LEAF)))
            .collect();
        before.sort_unstable_by_key(|(k, _)| *k);
        assert_eq!(proof.root(&before), reference.root());

        reference.insert(key(9_999), b"new");
        tree.set(key(9_999), smt_leaf_hash(b"new"));
        assert_eq!(tree.root(&empty), reference.root());
        let mut after: Vec<(Key, Hash)> = touched
            .iter()
            .map(|k| (*k, reference.get(k).unwrap_or(SMT_EMPTY_LEAF)))
            .collect();
        after.sort_unstable_by_key(|(k, _)| *k);
        assert_eq!(proof.root(&after), reference.root());
    }
}
