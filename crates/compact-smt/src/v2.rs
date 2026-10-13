// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The v2 tree: a compact sparse Merkle tree (crate documentation).
//!
//! **Definition.** Over a set of leaves `(key, leaf hash)` (a leaf whose hash is the empty
//! sentinel is absent), the subtree at depth `d` under a `d`-bit prefix has a [`Summary`]:
//!
//! - no leaf: [`Summary::Empty`], hash [`EMPTY`] (32 zero bytes);
//! - one leaf: [`Summary::One`], hash [`leaf_node`]`(key, leaf hash)`, the same at every depth
//!   (the leaf is a *shortcut* standing where its subtree becomes its own);
//! - two or more: [`Summary::Many`], hash [`node`]`(left, right)` of its two children's hashes,
//!   one of which may be [`EMPTY`].
//!
//! That is [`Summary::combine`]: two empties are empty, an empty and a single leaf are that
//! leaf, anything else is a node. The root is the hash of the whole tree's summary. It is the
//! hashing of Diem's and Aptos's Jellyfish Merkle tree over a binary radix (their sparse Merkle
//! proofs verify the same way), with CipherClerk's 16-byte keys and leaf hashes.
//!
//! **Why this design.** It is the standard compact SMT: one leaf-node hash and one node hash a
//! branching level; the root is a function of the set of leaves alone (no insertion order, no
//! history); inclusion and non-inclusion proofs are a list of siblings whose length fixes the
//! position, so a verifier needs no levels; an empty subtree costs nothing. A Patricia trie that
//! also skips one-child *internal* nodes would save about one hash a path more, but its nodes
//! must then commit to their levels for non-inclusion to be sound, which is a format of our own.
//!
//! **Cost.** A path is the levels above the key's shortcut, about `log₂ n + 1` node hashes plus
//! one leaf-node hash, against 128 in v1 (`proposals/cipherclerk/0006`: 7–15× fewer for books of
//! a thousand to a million leaves). Each hash is one BLAKE2b block, as in v1.

use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;
use cipher_clerk::merkle::SMT_EMPTY_LEAF;
use serde::{Deserialize, Serialize};

use crate::{bit, mask};

/// Bit depth of the tree: the 16-byte key space, as in v1.
pub const DEPTH: usize = 128;
/// Domain of a shortcut leaf's node: `H(LEAF_NODE ‖ key ‖ leaf hash)`.
pub const LEAF_NODE: &[u8] = b"cipher-clerk/smt/leaf-node/v2";
/// Domain of an internal node: `H(NODE ‖ left ‖ right)`.
pub const NODE: &[u8] = b"cipher-clerk/smt/node/v2";
/// Domain of the composite state root: `H(STATE_ROOT ‖ version ‖ six sub-roots)`.
pub const STATE_ROOT: &[u8] = b"cipher-clerk/state-root/v2";
/// The version byte the composite root commits to.
pub const VERSION: u8 = 2;
/// The hash of an empty subtree, and the root of an empty tree.
pub const EMPTY: [u8; 32] = [0u8; 32];

/// A sorted leaf: `(key, leaf hash)`.
pub type Leaf = ([u8; 16], [u8; 32]);

/// A shortcut leaf's node hash.
pub fn leaf_node(key: &[u8; 16], leaf_hash: &[u8; 32]) -> [u8; 32] {
    blake2b_256(LEAF_NODE, &[key, leaf_hash])
}

/// An internal node's hash.
pub fn node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    blake2b_256(NODE, &[left, right])
}

/// The composite state root over the six sub-roots, in composite order (accounts, transfers,
/// journals, external ids, voided, pending), with the version byte: one hash of two blocks.
pub fn composite_root(sub: &[[u8; 32]; 6]) -> [u8; 32] {
    blake2b_256(
        STATE_ROOT,
        &[
            &[VERSION],
            &sub[0],
            &sub[1],
            &sub[2],
            &sub[3],
            &sub[4],
            &sub[5],
        ],
    )
}

/// What a subtree holds, as far as its parent's hash is concerned (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Summary {
    /// No leaf.
    Empty,
    /// One leaf: its [`leaf_node`] hash.
    One([u8; 32]),
    /// Two or more leaves: a [`node`] hash.
    Many([u8; 32]),
}

impl Summary {
    /// A leaf's own summary: empty if the hash is the empty sentinel (a removed leaf).
    pub fn leaf(key: &[u8; 16], leaf_hash: &[u8; 32]) -> Self {
        if *leaf_hash == SMT_EMPTY_LEAF {
            Summary::Empty
        } else {
            Summary::One(leaf_node(key, leaf_hash))
        }
    }

    /// The subtree's hash.
    pub fn hash(&self) -> [u8; 32] {
        match self {
            Summary::Empty => EMPTY,
            Summary::One(h) | Summary::Many(h) => *h,
        }
    }

    /// Whether the subtree holds no leaf.
    pub fn is_empty(&self) -> bool {
        matches!(self, Summary::Empty)
    }

    /// The parent of two subtrees.
    pub fn combine(left: Summary, right: Summary) -> Summary {
        match (left, right) {
            (Summary::Empty, Summary::Empty) => Summary::Empty,
            (Summary::Empty, Summary::One(h)) | (Summary::One(h), Summary::Empty) => {
                Summary::One(h)
            }
            (l, r) => Summary::Many(node(&l.hash(), &r.hash())),
        }
    }

    /// This subtree, standing at depth `from` under `prefix`, seen from depth `to` (`to ≤ from`)
    /// with every sibling in between empty. A single leaf rises unchanged; a node is hashed once
    /// a level with an empty sibling.
    pub fn lift(self, prefix: &[u8; 16], from: usize, to: usize) -> Summary {
        match self {
            Summary::Many(mut h) => {
                for depth in (to..from).rev() {
                    h = if bit(prefix, depth) {
                        node(&EMPTY, &h)
                    } else {
                        node(&h, &EMPTY)
                    };
                }
                Summary::Many(h)
            }
            other => other,
        }
    }
}

/// The present leaves of `leaves` (those whose hash is not the empty sentinel): at most `n`.
fn first_present(leaves: &[Leaf], n: usize) -> usize {
    leaves
        .iter()
        .filter(|(_, h)| *h != SMT_EMPTY_LEAF)
        .take(n)
        .count()
}

/// The summary of the subtree at depth `level` holding `leaves` (sorted ascending by key, keys
/// unique, all under one `level`-bit prefix).
pub fn summary_sorted(leaves: &[Leaf], level: usize) -> Summary {
    match first_present(leaves, 2) {
        0 => Summary::Empty,
        1 => {
            let (k, h) = leaves
                .iter()
                .find(|(_, h)| *h != SMT_EMPTY_LEAF)
                .expect("one present leaf");
            Summary::One(leaf_node(k, h))
        }
        _ => {
            if level >= DEPTH {
                // Two present leaves at one key: the caller broke the precondition.
                let (k, h) = leaves[0];
                return Summary::One(leaf_node(&k, &h));
            }
            let split = leaves.partition_point(|(k, _)| !bit(k, level));
            let (l, r) = leaves.split_at(split);
            Summary::combine(summary_sorted(l, level + 1), summary_sorted(r, level + 1))
        }
    }
}

/// The root of the tree holding `leaves` (sorted ascending by key, keys unique).
pub fn root_sorted(leaves: &[Leaf]) -> [u8; 32] {
    summary_sorted(leaves, 0).hash()
}

// ---------------------------------------------------------------------------------------------
// Single-key proofs.

/// A proof that a key is in the tree with a leaf hash, or that it is not.
///
/// `siblings[d]` is the hash of the sibling at depth `d + 1` of the key's path, from the root
/// down; the path ends at depth `siblings.len()`, where the key's subtree is either empty
/// (`terminal` is `None`) or a single leaf (`terminal` is that leaf). The leaf is the key's own
/// (inclusion) or another key that shares the path (non-inclusion).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    /// Siblings from the root down.
    pub siblings: Vec<[u8; 32]>,
    /// The single leaf where the path ends, if any: `(key, leaf hash)`.
    pub terminal: Option<Leaf>,
}

/// What a valid proof says about its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    /// The key holds this leaf hash.
    Present([u8; 32]),
    /// The key holds no leaf.
    Absent,
}

/// The proof for `key` in the tree holding `leaves` (sorted, unique).
pub fn prove(leaves: &[Leaf], key: &[u8; 16]) -> Proof {
    let present: Vec<Leaf> = leaves
        .iter()
        .filter(|(_, h)| *h != SMT_EMPTY_LEAF)
        .copied()
        .collect();
    let mut slice: &[Leaf] = &present;
    let mut siblings = Vec::new();
    let mut level = 0;
    loop {
        match slice.len() {
            0 => {
                return Proof {
                    siblings,
                    terminal: None,
                };
            }
            1 => {
                return Proof {
                    siblings,
                    terminal: Some(slice[0]),
                };
            }
            _ => {}
        }
        let split = slice.partition_point(|(k, _)| !bit(k, level));
        let (l, r) = slice.split_at(split);
        let (on, off) = if bit(key, level) { (r, l) } else { (l, r) };
        siblings.push(summary_sorted(off, level + 1).hash());
        slice = on;
        level += 1;
    }
}

impl Proof {
    /// Check the proof for `key` against `root`: what it proves, or `None` if it proves nothing.
    pub fn verify(&self, root: &[u8; 32], key: &[u8; 16]) -> Option<Membership> {
        let len = self.siblings.len();
        if len > DEPTH {
            return None;
        }
        let (mut h, membership) = match &self.terminal {
            None => (EMPTY, Membership::Absent),
            Some((k, leaf)) => {
                if *leaf == SMT_EMPTY_LEAF || mask(k, len) != mask(key, len) {
                    return None;
                }
                let m = if k == key {
                    Membership::Present(*leaf)
                } else {
                    Membership::Absent
                };
                (leaf_node(k, leaf), m)
            }
        };
        for (d, sibling) in self.siblings.iter().enumerate().rev() {
            h = if bit(key, d) {
                node(sibling, &h)
            } else {
                node(&h, sibling)
            };
        }
        (h == *root).then_some(membership)
    }
}

// ---------------------------------------------------------------------------------------------
// Multiproofs: what a witness carries.

pub use entry::Entry;

/// rkyv's derive leaves the archived enum's resolver undocumented; nothing else here is.
mod entry {
    #![allow(missing_docs)]

    use serde::{Deserialize, Serialize};

    /// One untouched subtree beside a touched path.
    #[derive(
        Clone,
        Copy,
        Debug,
        PartialEq,
        Eq,
        Hash,
        Serialize,
        Deserialize,
        rkyv::Archive,
        rkyv::Serialize,
        rkyv::Deserialize,
    )]
    pub enum Entry {
        /// It holds exactly one leaf, given whole: the verifier hashes its node itself, so the entry
        /// cannot pass a node off as a leaf.
        One {
            /// The leaf's key, which must lie under the entry's prefix.
            key: [u8; 16],
            /// The leaf's hash.
            leaf: [u8; 32],
        },
        /// It holds two or more leaves: its node hash, taken on trust as a node. Sound as long as
        /// the touched subtree beside it is never emptied by the transition (no removal), which is
        /// true of the kernel's trees (`INV-2`); see [`super::MultiProof::root`].
        Many {
            /// The node hash.
            hash: [u8; 32],
        },
        /// It holds two or more leaves, opened one level: the verifier hashes the node itself, so
        /// it is known to be a node. What a tree with removals (the service tree) carries.
        Node {
            /// The left child's hash.
            left: [u8; 32],
            /// The right child's hash.
            right: [u8; 32],
        },
    }
}

/// A frontier node: an [`Entry`] at a position.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct FrontierNode {
    /// Key bits fixed by the path to the subtree.
    pub level: u16,
    /// Those bits, most significant first, the rest zero.
    pub prefix: [u8; 16],
    /// The subtree.
    pub entry: Entry,
}

/// A multiproof for a set of touched keys: the frontier of untouched, non-empty subtrees beside
/// their paths, sorted by `(prefix, level)`. With the touched keys' leaves it gives the root; with
/// their leaves after a transition, the root after (the untouched subtrees do not change).
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct MultiProof {
    /// The frontier.
    pub frontier: Vec<FrontierNode>,
}

/// A multiproof's wire form: one 50-byte record a frontier node, as v1's, plus a byte for its
/// kind. A [`Entry::One`] record carries the leaf's key where the others carry the prefix (the
/// prefix is the key masked to the level) and the leaf hash as its hash; a [`Entry::Node`]
/// record carries the left hash, and its right hash follows in `rights`. (The logical
/// [`MultiProof`] archived as an enum takes the largest variant's room for every entry, 84 bytes,
/// which made a reservation's witness 45 % larger than v1's.)
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct PackedProof {
    /// `(level, prefix or key, hash)`, in the frontier's order.
    pub nodes: Vec<(u16, [u8; 16], [u8; 32])>,
    /// One kind a node: 0 one leaf, 1 a node taken on trust, 2 an opened node.
    pub kinds: Vec<u8>,
    /// The right hashes of the opened nodes, in order.
    pub rights: Vec<[u8; 32]>,
}

impl From<&MultiProof> for PackedProof {
    fn from(p: &MultiProof) -> Self {
        let mut out = PackedProof::default();
        for f in &p.frontier {
            let (kind, bytes, hash) = match f.entry {
                Entry::One { key, leaf } => (0, key, leaf),
                Entry::Many { hash } => (1, f.prefix, hash),
                Entry::Node { left, right } => {
                    out.rights.push(right);
                    (2, f.prefix, left)
                }
            };
            out.nodes.push((f.level, bytes, hash));
            out.kinds.push(kind);
        }
        out
    }
}

impl PackedProof {
    /// The logical multiproof. Shape errors are [`ProofError::Shape`].
    pub fn unpack(&self) -> Result<MultiProof, ProofError> {
        if self.kinds.len() != self.nodes.len() {
            return Err(ProofError::Shape);
        }
        let mut rights = self.rights.iter();
        let mut frontier = Vec::with_capacity(self.nodes.len());
        for ((level, bytes, hash), kind) in self.nodes.iter().zip(&self.kinds) {
            let (prefix, entry) = match kind {
                0 => (
                    mask(bytes, usize::from(*level).min(DEPTH)),
                    Entry::One {
                        key: *bytes,
                        leaf: *hash,
                    },
                ),
                1 => (*bytes, Entry::Many { hash: *hash }),
                2 => (
                    *bytes,
                    Entry::Node {
                        left: *hash,
                        right: *rights.next().ok_or(ProofError::Shape)?,
                    },
                ),
                _ => return Err(ProofError::Shape),
            };
            frontier.push(FrontierNode {
                level: *level,
                prefix,
                entry,
            });
        }
        if rights.next().is_some() {
            return Err(ProofError::Shape);
        }
        Ok(MultiProof { frontier })
    }
}

/// Why a multiproof gives no root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofError {
    /// Out of order, repeated, past the tree's depth, a prefix with bits past its level, a leaf
    /// outside its entry's prefix, an empty entry, or an entry on a touched path.
    Shape,
    /// An entry the root did not use.
    Unused,
    /// An opaque [`Entry::Many`] where the caller asked for opened entries only.
    Opaque,
}

/// The entry for the untouched, non-empty subtree at `level` holding `leaves`; `open` gives a
/// node as [`Entry::Node`].
fn entry_of(leaves: &[Leaf], level: usize, open: bool) -> Option<Entry> {
    match summary_sorted(leaves, level) {
        Summary::Empty => None,
        Summary::One(_) => leaves
            .iter()
            .find(|(_, h)| *h != SMT_EMPTY_LEAF)
            .map(|(k, h)| Entry::One { key: *k, leaf: *h }),
        Summary::Many(hash) if !open => Some(Entry::Many { hash }),
        Summary::Many(_) => {
            let split = leaves.partition_point(|(k, _)| !bit(k, level));
            let (l, r) = leaves.split_at(split);
            Some(Entry::Node {
                left: summary_sorted(l, level + 1).hash(),
                right: summary_sorted(r, level + 1).hash(),
            })
        }
    }
}

impl MultiProof {
    /// The multiproof for `touched` (any order, repeats allowed) in the tree holding `leaves`
    /// (sorted, unique). `open` gives every node entry opened ([`Entry::Node`]).
    pub fn build(leaves: &[Leaf], touched: &[[u8; 16]], open: bool) -> Self {
        let mut t: Vec<[u8; 16]> = touched.to_vec();
        t.sort_unstable();
        t.dedup();
        let mut frontier = Vec::new();
        build_rec(leaves, &t, 0, [0; 16], open, &mut frontier);
        frontier.sort_unstable_by_key(|f| (f.prefix, f.level));
        MultiProof { frontier }
    }

    /// The root over `touched` (`(key, leaf hash)`, sorted ascending by key, unique; the empty
    /// sentinel for an absent key) and this frontier. Every entry must be used.
    ///
    /// An [`Entry::Many`] is trusted to be a node. Where the touched subtree beside it is empty,
    /// that matters: an empty subtree beside a node is hashed, beside a leaf it is not. Checking
    /// the root *before* a transition binds it then; a root *after* a transition that empties a
    /// touched subtree beside one would not be bound. The kernel's trees never remove a leaf
    /// (`INV-2`), so no touched subtree goes from present to empty; a tree that removes uses
    /// [`MultiProof::root_opened`].
    pub fn root(&self, touched: &[Leaf]) -> Result<[u8; 32], ProofError> {
        self.compute(touched, false)
    }

    /// [`MultiProof::root`], refusing opaque [`Entry::Many`]: every entry's kind is then bound,
    /// whatever the transition does to the touched leaves.
    pub fn root_opened(&self, touched: &[Leaf]) -> Result<[u8; 32], ProofError> {
        self.compute(touched, true)
    }

    fn compute(&self, touched: &[Leaf], opened: bool) -> Result<[u8; 32], ProofError> {
        self.check_shape(opened)?;
        let mut used = 0;
        let s = root_rec(touched, &self.frontier, 0, &mut used)?;
        if used != self.frontier.len() {
            return Err(ProofError::Unused);
        }
        Ok(s.hash())
    }

    fn check_shape(&self, opened: bool) -> Result<(), ProofError> {
        for (i, f) in self.frontier.iter().enumerate() {
            let level = usize::from(f.level);
            if level > DEPTH || mask(&f.prefix, level) != f.prefix {
                return Err(ProofError::Shape);
            }
            if i > 0 {
                let p = &self.frontier[i - 1];
                if (p.prefix, p.level) >= (f.prefix, f.level) {
                    return Err(ProofError::Shape);
                }
            }
            match f.entry {
                Entry::One { key, leaf } => {
                    if leaf == SMT_EMPTY_LEAF || mask(&key, level) != f.prefix {
                        return Err(ProofError::Shape);
                    }
                }
                Entry::Many { hash } => {
                    if opened {
                        return Err(ProofError::Opaque);
                    }
                    if hash == EMPTY {
                        return Err(ProofError::Shape);
                    }
                }
                Entry::Node { .. } => {}
            }
        }
        Ok(())
    }
}

/// The summary at depth `level` of `touched` (sorted, unique, all under one `level`-bit prefix)
/// and the frontier entries `front` (sorted by `(prefix, level)`, all under the same prefix):
/// a piece of [`MultiProof::root`], for a prover that folds part of a proof into one entry.
/// Every entry must be used.
pub fn summary_with(
    touched: &[Leaf],
    front: &[FrontierNode],
    level: usize,
) -> Result<Summary, ProofError> {
    let mut used = 0;
    let s = root_rec(touched, front, level, &mut used)?;
    if used != front.len() {
        return Err(ProofError::Unused);
    }
    Ok(s)
}

fn entry_summary(e: &Entry) -> Summary {
    match e {
        Entry::One { key, leaf } => Summary::One(leaf_node(key, leaf)),
        Entry::Many { hash } => Summary::Many(*hash),
        Entry::Node { left, right } => Summary::Many(node(left, right)),
    }
}

/// The summary at depth `level` (under a prefix both slices share) of the touched leaves and the
/// frontier entries under it. Mirrors [`build_rec`].
fn root_rec(
    touched: &[Leaf],
    front: &[FrontierNode],
    level: usize,
    used: &mut usize,
) -> Result<Summary, ProofError> {
    if let Some(first) = front.first() {
        // Sorted by (prefix, level): an entry at this node or above it sorts first.
        if usize::from(first.level) <= level {
            if usize::from(first.level) < level || !touched.is_empty() || front.len() != 1 {
                return Err(ProofError::Shape);
            }
            *used += 1;
            return Ok(entry_summary(&first.entry));
        }
    }
    match (touched.len(), front.len()) {
        (0, 0) => return Ok(Summary::Empty),
        (1, 0) => return Ok(Summary::leaf(&touched[0].0, &touched[0].1)),
        _ => {}
    }
    if level >= DEPTH {
        return Err(ProofError::Shape);
    }
    let ts = touched.partition_point(|(k, _)| !bit(k, level));
    let fs = front.partition_point(|f| !bit(&f.prefix, level));
    let (tl, tr) = touched.split_at(ts);
    let (fl, fr) = front.split_at(fs);
    let l = root_rec(tl, fl, level + 1, used)?;
    let r = root_rec(tr, fr, level + 1, used)?;
    Ok(Summary::combine(l, r))
}

/// The frontier of `touched` (sorted, unique) in the subtree at `level` holding `leaves`.
/// Mirrors [`root_rec`]: it stops where the verifier stops.
fn build_rec(
    leaves: &[Leaf],
    touched: &[[u8; 16]],
    level: usize,
    prefix: [u8; 16],
    open: bool,
    out: &mut Vec<FrontierNode>,
) {
    if touched.is_empty() {
        if let Some(entry) = entry_of(leaves, level, open) {
            out.push(FrontierNode {
                level: level as u16,
                prefix,
                entry,
            });
        }
        return;
    }
    if touched.len() == 1 {
        let only = touched[0];
        let other = leaves
            .iter()
            .any(|(k, h)| *h != SMT_EMPTY_LEAF && *k != only);
        if !other {
            return;
        }
    }
    if level >= DEPTH {
        return;
    }
    let ts = touched.partition_point(|k| !bit(k, level));
    let ls = leaves.partition_point(|(k, _)| !bit(k, level));
    let (tl, tr) = touched.split_at(ts);
    let (ll, lr) = leaves.split_at(ls);
    build_rec(ll, tl, level + 1, prefix, open, out);
    build_rec(lr, tr, level + 1, crate::with_bit(prefix, level), open, out);
}
