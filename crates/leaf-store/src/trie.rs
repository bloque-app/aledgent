// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The sparse Merkle trees, stored compressed (`F-006` T-006-02, T-006-03).
//!
//! Every tree has CipherClerk's shape: depth 128 over 16-byte keys, bits read most significant
//! first, leaves hashed with `SMT_LEAF`, nodes with `SMT_NODE`, an absent subtree taking the
//! empty-subtree hash of its height. Roots and proofs are byte-for-byte the kernel's.
//!
//! **Only branching nodes are stored.** `F-001`'s `FileStore` writes 128 node rows per leaf, which
//! `F-002` measured at about 18 KB per leaf. Here a node is stored only where both of its children
//! are non-empty, so a tree of `n` leaves stores `n − 1` branch rows. A branch at level `L` (the
//! number of key bits its path fixes) keeps, for each side, a reference to the next stored node
//! below and that side's subtree hash at level `L + 1`. The hash of a lone node's subtree further
//! up is its own hash folded up through empty siblings, computed when needed.
//!
//! | Row | Key | Value |
//! |---|---|---|
//! | leaf | `tree ‖ key` | `leaf hash ‖ stored bytes` (written by the store) |
//! | branch | `tree ‖ level ‖ prefix` | `left ref ‖ left hash ‖ right ref ‖ right hash` (98 B) |
//! | top | `tree` | `top ref ‖ root` |
//!
//! **Root versions** (`DEC-25`). In a v2 build (`compact_smt::Version::ACTIVE`)
//! the same rows hold v2 hashes: a side's hash is its subtree's v2 hash at level `L + 1` (a lone
//! leaf's shortcut node, unchanged by height; a branch's node, hashed once a level with an empty
//! sibling), so a write hashes one leaf node and one node a branch on its path. A removal (the
//! empty sentinel, the service tree's) takes the leaf out of the structure, so every branch keeps
//! two present sides. Proofs are `compact_smt::v2`'s ([`prove_v2`], [`multiproof`]).
//!
//! **Cost of a write.** An upsert reads and rewrites the branches on the key's path — about
//! `log₂ n` of them — and hashes exactly the 128 levels of the path. Nothing depends on how many
//! entries the book holds beyond that logarithm, and no key is ever removed (`INV-2`), so the
//! structure only grows.

use cipher_clerk::merkle::{BatchProof, SMT_DEPTH, SMT_EMPTY_LEAF, SmtProof, smt_node_hash};
use ledger_kernel::compact_smt::Version;
use ledger_kernel::compact_smt::v2::{self, Entry, FrontierNode, MultiProof, Summary};

use crate::engine::{EngineError, Space};
use crate::kv::{Kv, Txn};

/// The empty-subtree hashes, by height.
pub type EmptyChain = [[u8; 32]; SMT_DEPTH + 1];

/// Which tree: the kernel's six sub-trees in composite order, then the service tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreeId(pub u8);

impl TreeId {
    /// The service tree (`F-006` Proposal, T-006-03).
    pub const SERVICE: TreeId = TreeId(6);
    /// Every tree, in order.
    pub const ALL: [TreeId; 7] = [
        TreeId(0),
        TreeId(1),
        TreeId(2),
        TreeId(3),
        TreeId(4),
        TreeId(5),
        TreeId(6),
    ];
}

/// A stored node: a branch at `level` < 128, or a leaf at level 128 whose prefix is its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeRef {
    /// Key bits fixed by the path to the node.
    pub level: u8,
    /// Those bits, most significant first, the rest zero.
    pub prefix: [u8; 16],
}

const LEAF_LEVEL: u8 = SMT_DEPTH as u8;

impl NodeRef {
    fn leaf(key: [u8; 16]) -> Self {
        Self {
            level: LEAF_LEVEL,
            prefix: key,
        }
    }

    fn is_leaf(&self) -> bool {
        self.level == LEAF_LEVEL
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.level);
        out.extend_from_slice(&self.prefix);
    }

    fn decode(b: &[u8]) -> Result<Self, EngineError> {
        if b.len() != 17 || b[0] > LEAF_LEVEL {
            return Err(EngineError("corrupt node reference".into()));
        }
        let mut prefix = [0u8; 16];
        prefix.copy_from_slice(&b[1..]);
        Ok(Self {
            level: b[0],
            prefix,
        })
    }
}

/// A branch: both sides non-empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Branch {
    left: NodeRef,
    left_hash: [u8; 32],
    right: NodeRef,
    right_hash: [u8; 32],
}

impl Branch {
    fn own(&self) -> [u8; 32] {
        smt_node_hash(&self.left_hash, &self.right_hash)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(98);
        self.left.encode(&mut out);
        out.extend_from_slice(&self.left_hash);
        self.right.encode(&mut out);
        out.extend_from_slice(&self.right_hash);
        out
    }

    fn decode(b: &[u8]) -> Result<Self, EngineError> {
        if b.len() != 98 {
            return Err(EngineError("corrupt branch".into()));
        }
        Ok(Self {
            left: NodeRef::decode(&b[..17])?,
            left_hash: array32(&b[17..49]),
            right: NodeRef::decode(&b[49..66])?,
            right_hash: array32(&b[66..98]),
        })
    }
}

fn array32(b: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(b);
    out
}

/// Bit `pos` of `key`, most significant first.
pub fn bit(key: &[u8; 16], pos: usize) -> bool {
    (key[pos / 8] >> (7 - pos % 8)) & 1 == 1
}

/// `key` with every bit from position `bits` on cleared.
pub fn mask(key: &[u8; 16], bits: usize) -> [u8; 16] {
    let mut out = [0u8; 16];
    let full = bits / 8;
    out[..full].copy_from_slice(&key[..full]);
    if full < 16 && bits % 8 != 0 {
        out[full] = key[full] & (0xFFu8 << (8 - bits % 8));
    }
    out
}

fn with_bit(prefix: [u8; 16], pos: usize) -> [u8; 16] {
    let mut p = prefix;
    p[pos / 8] |= 1 << (7 - pos % 8);
    p
}

/// How many leading bits `a` and `b` share.
fn common_prefix(a: &[u8; 16], b: &[u8; 16]) -> usize {
    for i in 0..16 {
        let x = a[i] ^ b[i];
        if x != 0 {
            return i * 8 + x.leading_zeros() as usize;
        }
    }
    SMT_DEPTH
}

/// Fold the hash `h` of the subtree at `from` up to level `to` (`to ≤ from`), through empty
/// siblings, along the bits of `prefix`.
pub fn fold(empty: &EmptyChain, mut h: [u8; 32], prefix: &[u8; 16], from: u8, to: u8) -> [u8; 32] {
    for depth in (usize::from(to)..usize::from(from)).rev() {
        let sibling = empty[SMT_DEPTH - depth - 1];
        h = if bit(prefix, depth) {
            smt_node_hash(&sibling, &h)
        } else {
            smt_node_hash(&h, &sibling)
        };
    }
    h
}

fn node_key(tree: TreeId, r: &NodeRef) -> [u8; 18] {
    let mut k = [0u8; 18];
    k[0] = tree.0;
    k[1] = r.level;
    k[2..].copy_from_slice(&r.prefix);
    k
}

/// The key of a leaf row.
pub fn leaf_key(tree: TreeId, key: &[u8; 16]) -> [u8; 17] {
    let mut k = [0u8; 17];
    k[0] = tree.0;
    k[1..].copy_from_slice(key);
    k
}

/// A tree's top node and root.
fn top<K: Kv + ?Sized>(kv: &K, tree: TreeId) -> Result<Option<(NodeRef, [u8; 32])>, EngineError> {
    match kv.get(Space::Top, &[tree.0])? {
        None => Ok(None),
        Some(v) if v.len() == 49 => Ok(Some((NodeRef::decode(&v[..17])?, array32(&v[17..])))),
        Some(_) => Err(EngineError("corrupt top row".into())),
    }
}

/// The root of `tree`.
pub fn root<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    empty: &EmptyChain,
) -> Result<[u8; 32], EngineError> {
    let empty_root = match Version::ACTIVE {
        Version::V1 => empty[SMT_DEPTH],
        Version::V2 => v2::EMPTY,
    };
    Ok(top(kv, tree)?.map_or(empty_root, |(_, r)| r))
}

/// The leaf hash stored for `key`, if the leaf exists.
pub fn leaf_hash<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    key: &[u8; 16],
) -> Result<Option<[u8; 32]>, EngineError> {
    match kv.get(Space::Leaf, &leaf_key(tree, key))? {
        None => Ok(None),
        Some(v) if v.len() >= 32 => Ok(Some(array32(&v[..32]))),
        Some(_) => Err(EngineError("corrupt leaf row".into())),
    }
}

fn branch<K: Kv + ?Sized>(kv: &K, tree: TreeId, r: &NodeRef) -> Result<Branch, EngineError> {
    match kv.get(Space::Node, &node_key(tree, r))? {
        Some(v) => Branch::decode(&v),
        None => Err(EngineError(format!(
            "missing branch at level {} of tree {}",
            r.level, tree.0
        ))),
    }
}

/// The hash of the subtree at `r`'s own level.
fn own<K: Kv + ?Sized>(kv: &K, tree: TreeId, r: &NodeRef) -> Result<[u8; 32], EngineError> {
    if r.is_leaf() {
        leaf_hash(kv, tree, &r.prefix)?
            .ok_or_else(|| EngineError("a reference names a missing leaf".into()))
    } else {
        Ok(branch(kv, tree, r)?.own())
    }
}

/// Set the leaf at `key` to `leaf_hash` (insert or replace) and update its path. The leaf row
/// itself must already be in `txn`. Returns the new root.
pub fn upsert<S: crate::engine::Snapshot>(
    txn: &mut Txn<'_, S>,
    tree: TreeId,
    empty: &EmptyChain,
    key: &[u8; 16],
    leaf_hash: [u8; 32],
) -> Result<[u8; 32], EngineError> {
    if Version::ACTIVE == Version::V2 {
        return upsert_v2(txn, tree, key, leaf_hash);
    }
    let mut path: Vec<(NodeRef, Branch, bool)> = Vec::new();
    let mut cur = top(txn, tree)?.map(|(r, _)| r);
    let (mut child, mut child_own) = loop {
        let Some(r) = cur else {
            break (NodeRef::leaf(*key), leaf_hash);
        };
        let cpl = common_prefix(key, &r.prefix).min(usize::from(r.level));
        if cpl < usize::from(r.level) {
            // The key leaves `r`'s path above it: a new branch where they part.
            let r_own = own(txn, tree, &r)?;
            let leaf = NodeRef::leaf(*key);
            let level = cpl as u8;
            let (l, lh, rr, rh) = if bit(key, cpl) {
                (r, r_own, leaf, leaf_hash)
            } else {
                (leaf, leaf_hash, r, r_own)
            };
            let b = Branch {
                left: l,
                left_hash: fold(empty, lh, &l.prefix, l.level, level + 1),
                right: rr,
                right_hash: fold(empty, rh, &rr.prefix, rr.level, level + 1),
            };
            let at = NodeRef {
                level,
                prefix: mask(key, cpl),
            };
            txn.put(Space::Node, &node_key(tree, &at), b.encode());
            break (at, b.own());
        }
        if r.is_leaf() {
            break (r, leaf_hash);
        }
        let b = branch(txn, tree, &r)?;
        let side = bit(key, usize::from(r.level));
        path.push((r, b, side));
        cur = Some(if side { b.right } else { b.left });
    };
    for (at, mut b, side) in path.into_iter().rev() {
        let h = fold(empty, child_own, &child.prefix, child.level, at.level + 1);
        if side {
            b.right = child;
            b.right_hash = h;
        } else {
            b.left = child;
            b.left_hash = h;
        }
        txn.put(Space::Node, &node_key(tree, &at), b.encode());
        child = at;
        child_own = b.own();
    }
    let root = fold(empty, child_own, &child.prefix, child.level, 0);
    let mut v = Vec::with_capacity(49);
    child.encode(&mut v);
    v.extend_from_slice(&root);
    txn.put(Space::Top, &[tree.0], v);
    Ok(root)
}

// ---------------------------------------------------------------------------------------------
// v2 (DEC-25).

/// A stored node's own v2 summary, at its own level. A referenced leaf is present (v2 takes a
/// removed leaf out of the structure), and a branch has two present sides.
fn own_v2<K: Kv + ?Sized>(kv: &K, tree: TreeId, r: &NodeRef) -> Result<Summary, EngineError> {
    if r.is_leaf() {
        let h = leaf_hash(kv, tree, &r.prefix)?
            .ok_or_else(|| EngineError("a reference names a missing leaf".into()))?;
        Ok(Summary::leaf(&r.prefix, &h))
    } else {
        let b = branch(kv, tree, r)?;
        Ok(Summary::Many(v2::node(&b.left_hash, &b.right_hash)))
    }
}

/// `own`, standing at `r.level`, seen from level `to`.
fn lifted(own: Summary, r: &NodeRef, to: usize) -> Summary {
    own.lift(&r.prefix, usize::from(r.level), to)
}

fn upsert_v2<S: crate::engine::Snapshot>(
    txn: &mut Txn<'_, S>,
    tree: TreeId,
    key: &[u8; 16],
    leaf_hash: [u8; 32],
) -> Result<[u8; 32], EngineError> {
    if leaf_hash == SMT_EMPTY_LEAF {
        return remove_v2(txn, tree, key);
    }
    let leaf_own = Summary::leaf(key, &leaf_hash);
    let mut path: Vec<(NodeRef, Branch, bool)> = Vec::new();
    let mut cur = top(txn, tree)?.map(|(r, _)| r);
    let (child, child_own) = loop {
        let Some(r) = cur else {
            break (NodeRef::leaf(*key), leaf_own);
        };
        let cpl = common_prefix(key, &r.prefix).min(usize::from(r.level));
        if cpl < usize::from(r.level) {
            let r_own = own_v2(txn, tree, &r)?;
            let leaf = NodeRef::leaf(*key);
            let (l, lo, rr, ro) = if bit(key, cpl) {
                (r, r_own, leaf, leaf_own)
            } else {
                (leaf, leaf_own, r, r_own)
            };
            let b = Branch {
                left: l,
                left_hash: lifted(lo, &l, cpl + 1).hash(),
                right: rr,
                right_hash: lifted(ro, &rr, cpl + 1).hash(),
            };
            let at = NodeRef {
                level: cpl as u8,
                prefix: mask(key, cpl),
            };
            txn.put(Space::Node, &node_key(tree, &at), b.encode());
            break (at, Summary::Many(v2::node(&b.left_hash, &b.right_hash)));
        }
        if r.is_leaf() {
            break (r, leaf_own);
        }
        let b = branch(txn, tree, &r)?;
        let side = bit(key, usize::from(r.level));
        path.push((r, b, side));
        cur = Some(if side { b.right } else { b.left });
    };
    finish_v2(txn, tree, path, child, child_own)
}

/// Rewrite the branches of `path` (top down) over a new `child`, and the top row.
fn finish_v2<S: crate::engine::Snapshot>(
    txn: &mut Txn<'_, S>,
    tree: TreeId,
    path: Vec<(NodeRef, Branch, bool)>,
    mut child: NodeRef,
    mut child_own: Summary,
) -> Result<[u8; 32], EngineError> {
    for (at, mut b, side) in path.into_iter().rev() {
        let h = lifted(child_own, &child, usize::from(at.level) + 1).hash();
        if side {
            b.right = child;
            b.right_hash = h;
        } else {
            b.left = child;
            b.left_hash = h;
        }
        txn.put(Space::Node, &node_key(tree, &at), b.encode());
        child = at;
        child_own = Summary::Many(v2::node(&b.left_hash, &b.right_hash));
    }
    let root = lifted(child_own, &child, 0).hash();
    let mut v = Vec::with_capacity(49);
    child.encode(&mut v);
    v.extend_from_slice(&root);
    txn.put(Space::Top, &[tree.0], v);
    Ok(root)
}

/// Take `key`'s leaf out of the structure: its parent branch gives way to its sibling.
fn remove_v2<S: crate::engine::Snapshot>(
    txn: &mut Txn<'_, S>,
    tree: TreeId,
    key: &[u8; 16],
) -> Result<[u8; 32], EngineError> {
    let Some((top_ref, top_root)) = top(txn, tree)? else {
        return Ok(v2::EMPTY);
    };
    let mut path: Vec<(NodeRef, Branch, bool)> = Vec::new();
    let mut cur = top_ref;
    loop {
        let cpl = common_prefix(key, &cur.prefix).min(usize::from(cur.level));
        if cpl < usize::from(cur.level) {
            return Ok(top_root);
        }
        if cur.is_leaf() {
            break;
        }
        let b = branch(txn, tree, &cur)?;
        let side = bit(key, usize::from(cur.level));
        path.push((cur, b, side));
        cur = if side { b.right } else { b.left };
    }
    let Some((parent, pb, side)) = path.pop() else {
        txn.delete(Space::Top, &[tree.0]);
        return Ok(v2::EMPTY);
    };
    let sibling = if side { pb.left } else { pb.right };
    let sibling_own = own_v2(txn, tree, &sibling)?;
    txn.delete(Space::Node, &node_key(tree, &parent));
    finish_v2(txn, tree, path, sibling, sibling_own)
}

/// The v2 proof for `key` against the tree's root (`compact_smt::v2::Proof`): a v2 build's
/// trees only.
pub fn prove_v2<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    key: &[u8; 16],
) -> Result<v2::Proof, EngineError> {
    let mut siblings = Vec::new();
    let mut cur = top(kv, tree)?.map(|(r, _)| r);
    while let Some(r) = cur {
        if r.is_leaf() {
            let h = leaf_hash(kv, tree, &r.prefix)?
                .ok_or_else(|| EngineError("a reference names a missing leaf".into()))?;
            return Ok(v2::Proof {
                siblings,
                terminal: Some((r.prefix, h)),
            });
        }
        let level = usize::from(r.level);
        let cpl = common_prefix(key, &r.prefix).min(level);
        siblings.resize(cpl.min(level), v2::EMPTY);
        if cpl < level {
            siblings.push(lifted(own_v2(kv, tree, &r)?, &r, cpl + 1).hash());
            return Ok(v2::Proof {
                siblings,
                terminal: None,
            });
        }
        let b = branch(kv, tree, &r)?;
        let side = bit(key, level);
        siblings.push(if side { b.left_hash } else { b.right_hash });
        cur = Some(if side { b.right } else { b.left });
    }
    Ok(v2::Proof {
        siblings,
        terminal: None,
    })
}

/// The v2 multiproof of `touched` (any order, repeats allowed): exactly what
/// `compact_smt::v2::MultiProof::build` computes over the full tree, read from the stored
/// branches. `open` gives node entries opened (the service tree, which removes leaves).
pub fn multiproof<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    touched: &[[u8; 16]],
    open: bool,
) -> Result<MultiProof, EngineError> {
    let mut t = touched.to_vec();
    t.sort_unstable();
    t.dedup();
    let mut frontier = Vec::new();
    let sub = top(kv, tree)?.map(|(r, _)| r);
    multiproof_rec(kv, tree, &t, sub, 0, [0; 16], open, &mut frontier)?;
    frontier.sort_unstable_by_key(|f| (f.prefix, f.level));
    Ok(MultiProof { frontier })
}

/// The entry for the untouched subtree at (`level`, `prefix`) whose topmost stored node is `r`.
fn entry_v2<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    r: &NodeRef,
    level: usize,
    open: bool,
) -> Result<Entry, EngineError> {
    if r.is_leaf() {
        let leaf = leaf_hash(kv, tree, &r.prefix)?
            .ok_or_else(|| EngineError("a reference names a missing leaf".into()))?;
        return Ok(Entry::One {
            key: r.prefix,
            leaf,
        });
    }
    let own = own_v2(kv, tree, r)?;
    if !open {
        return Ok(Entry::Many {
            hash: lifted(own, r, level).hash(),
        });
    }
    if usize::from(r.level) == level {
        let b = branch(kv, tree, r)?;
        return Ok(Entry::Node {
            left: b.left_hash,
            right: b.right_hash,
        });
    }
    let h = lifted(own, r, level + 1).hash();
    Ok(if bit(&r.prefix, level) {
        Entry::Node {
            left: v2::EMPTY,
            right: h,
        }
    } else {
        Entry::Node {
            left: h,
            right: v2::EMPTY,
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn multiproof_rec<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    touched: &[[u8; 16]],
    sub: Option<NodeRef>,
    level: usize,
    prefix: [u8; 16],
    open: bool,
    out: &mut Vec<FrontierNode>,
) -> Result<(), EngineError> {
    if touched.is_empty() {
        if let Some(r) = sub {
            out.push(FrontierNode {
                level: level as u16,
                prefix,
                entry: entry_v2(kv, tree, &r, level, open)?,
            });
        }
        return Ok(());
    }
    if touched.len() == 1 {
        match sub {
            None => return Ok(()),
            Some(r) if r.is_leaf() && r.prefix == touched[0] => return Ok(()),
            _ => {}
        }
    }
    if level == SMT_DEPTH {
        return Ok(());
    }
    let (left, right) = match sub {
        None => (None, None),
        Some(r) if usize::from(r.level) > level => {
            if bit(&r.prefix, level) {
                (None, Some(r))
            } else {
                (Some(r), None)
            }
        }
        Some(r) => {
            let b = branch(kv, tree, &r)?;
            (Some(b.left), Some(b.right))
        }
    };
    let split = touched.partition_point(|k| !bit(k, level));
    let (tl, tr) = touched.split_at(split);
    multiproof_rec(kv, tree, tl, left, level + 1, prefix, open, out)?;
    multiproof_rec(
        kv,
        tree,
        tr,
        right,
        level + 1,
        with_bit(prefix, level),
        open,
        out,
    )
}

/// The proof for `key` against the tree's root: CipherClerk's [`SmtProof`], 128 siblings from
/// the leaf level up, the leaf hash or the empty sentinel for an absent key.
pub fn prove<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    empty: &EmptyChain,
    key: &[u8; 16],
) -> Result<SmtProof, EngineError> {
    // sib[l] is the sibling of the path node at level l (1..=128).
    let mut sib = [[0u8; 32]; SMT_DEPTH + 1];
    for (l, s) in sib.iter_mut().enumerate().skip(1) {
        *s = empty[SMT_DEPTH - l];
    }
    let mut leaf = SMT_EMPTY_LEAF;
    let mut cur = top(kv, tree)?.map(|(r, _)| r);
    while let Some(r) = cur {
        let cpl = common_prefix(key, &r.prefix).min(usize::from(r.level));
        if cpl < usize::from(r.level) {
            sib[cpl + 1] = fold(empty, own(kv, tree, &r)?, &r.prefix, r.level, cpl as u8 + 1);
            break;
        }
        if r.is_leaf() {
            leaf = own(kv, tree, &r)?;
            break;
        }
        let b = branch(kv, tree, &r)?;
        let side = bit(key, usize::from(r.level));
        sib[usize::from(r.level) + 1] = if side { b.left_hash } else { b.right_hash };
        cur = Some(if side { b.right } else { b.left });
    }
    let siblings = (0..SMT_DEPTH).map(|i| sib[SMT_DEPTH - i]).collect();
    Ok(SmtProof {
        key: *key,
        leaf,
        siblings,
    })
}

/// One node of a multiproof's frontier: the hash of an untouched, non-empty subtree next to a
/// touched path (CipherClerk's `BatchProof`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frontier {
    /// Key bits fixed by the path to the subtree.
    pub level: u16,
    /// Those bits, most significant first.
    pub prefix: [u8; 16],
    /// The subtree's hash.
    pub hash: [u8; 32],
}

/// The frontier of `touched` (any order, repeats allowed), sorted by `(level, prefix)`: exactly
/// what `BatchProof::build` computes over the full tree, read from the stored branches.
pub fn frontier<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    empty: &EmptyChain,
    touched: &[[u8; 16]],
) -> Result<Vec<Frontier>, EngineError> {
    let mut t = touched.to_vec();
    t.sort_unstable();
    t.dedup();
    let mut out = Vec::new();
    let top = top(kv, tree)?;
    let (sub, known) = match top {
        Some((r, root)) => (Some(r), Some(root)),
        None => (None, None),
    };
    frontier_rec(kv, tree, empty, &t, sub, known, 0, [0; 16], &mut out)?;
    out.sort_unstable_by_key(|f| (f.level, f.prefix));
    Ok(out)
}

/// `sub` is the one stored node that tops the subtree at (`level`, `prefix`), if it is not
/// empty; `known` is that subtree's hash at `level`, when the caller already has it.
#[allow(clippy::too_many_arguments)]
fn frontier_rec<K: Kv + ?Sized>(
    kv: &K,
    tree: TreeId,
    empty: &EmptyChain,
    touched: &[[u8; 16]],
    sub: Option<NodeRef>,
    known: Option<[u8; 32]>,
    level: usize,
    prefix: [u8; 16],
    out: &mut Vec<Frontier>,
) -> Result<(), EngineError> {
    if touched.is_empty() {
        if let Some(r) = sub {
            let hash = match known {
                Some(h) => h,
                None => fold(empty, own(kv, tree, &r)?, &r.prefix, r.level, level as u8),
            };
            out.push(Frontier {
                level: level as u16,
                prefix,
                hash,
            });
        }
        return Ok(());
    }
    if level == SMT_DEPTH {
        return Ok(());
    }
    type Side = (Option<NodeRef>, Option<[u8; 32]>);
    let (left, right): (Side, Side) = match sub {
        None => ((None, None), (None, None)),
        Some(r) if usize::from(r.level) > level => {
            if bit(&r.prefix, level) {
                ((None, None), (Some(r), None))
            } else {
                ((Some(r), None), (None, None))
            }
        }
        Some(r) => {
            let b = branch(kv, tree, &r)?;
            (
                (Some(b.left), Some(b.left_hash)),
                (Some(b.right), Some(b.right_hash)),
            )
        }
    };
    let split = touched.partition_point(|k| !bit(k, level));
    let (tl, tr) = touched.split_at(split);
    frontier_rec(kv, tree, empty, tl, left.0, left.1, level + 1, prefix, out)?;
    frontier_rec(
        kv,
        tree,
        empty,
        tr,
        right.0,
        right.1,
        level + 1,
        with_bit(prefix, level),
        out,
    )
}

// ---------------------------------------------------------------------------------------------
// CipherClerk's proof types have private fields. These mirrors have the same fields in the same
// order, so their rkyv archives are the kernel's; decoding them as the kernel's types is checked.

#[derive(rkyv::Archive, rkyv::Serialize, Clone, Debug)]
pub(crate) struct FrontierNodeMirror {
    level: u16,
    prefix: [u8; 16],
    hash: [u8; 32],
}

#[derive(rkyv::Archive, rkyv::Serialize, Clone, Debug, Default)]
pub(crate) struct BatchProofMirror {
    frontier: Vec<FrontierNodeMirror>,
}

impl BatchProofMirror {
    pub(crate) fn new(f: &[Frontier]) -> Self {
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
}

/// CipherClerk's [`BatchProof`] with this frontier.
pub fn batch_proof(f: &[Frontier]) -> BatchProof {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&BatchProofMirror::new(f))
        .expect("archiving a proof only fails on allocator exhaustion");
    rkyv::from_bytes::<BatchProof, rkyv::rancor::Error>(&bytes)
        .expect("the mirror's archive is the kernel's BatchProof")
}

// Held to CipherClerk's v1 reference; `v2_tests` holds a v2 build's tree to its own.
#[cfg(all(test, feature = "smt-v1"))]
mod tests {
    use super::*;
    use crate::engine::{Durability, Engine, RedbEngine};
    use cipher_clerk::crypto::blake2b_256;
    use cipher_clerk::merkle::{SparseMerkleTree, build_empty_chain, smt_leaf_hash};
    use proptest::prelude::*;

    fn key(i: u64) -> [u8; 16] {
        let h = blake2b_256(b"trie-test-key", &[&i.to_le_bytes()]);
        let mut k = [0u8; 16];
        k.copy_from_slice(&h[..16]);
        k
    }

    fn put_leaf(e: &RedbEngine, tree: TreeId, k: [u8; 16], content: &[u8]) -> [u8; 32] {
        let empty = build_empty_chain();
        let snap = e.snapshot().unwrap();
        let mut txn = Txn::new(&snap);
        let h = smt_leaf_hash(content);
        let mut row = h.to_vec();
        row.extend_from_slice(content);
        txn.put(Space::Leaf, &leaf_key(tree, &k), row);
        let r = upsert(&mut txn, tree, &empty, &k, h).unwrap();
        e.write(txn.into_batch(), Durability::Visible).unwrap();
        r
    }

    fn check_against(
        e: &RedbEngine,
        tree: TreeId,
        reference: &SparseMerkleTree,
        probe: &[[u8; 16]],
    ) {
        let empty = build_empty_chain();
        let snap = e.snapshot().unwrap();
        assert_eq!(root(&snap, tree, &empty).unwrap(), reference.root());
        for k in probe {
            let p = prove(&snap, tree, &empty, k).unwrap();
            assert_eq!(p, reference.prove(*k));
            assert!(p.verify(&reference.root()));
        }
        let f = frontier(&snap, tree, &empty, probe).unwrap();
        let bp = batch_proof(&f);
        assert_eq!(
            bp,
            cipher_clerk::merkle::BatchProof::build(reference, probe)
        );
    }

    /// T-006-02: the stored tree's roots, single proofs and multiproofs equal CipherClerk's
    /// reference tree's, byte for byte, for the same writes.
    #[test]
    fn roots_and_proofs_equal_the_kernels_reference() {
        let e = RedbEngine::in_memory().unwrap();
        let tree = TreeId(1);
        let mut reference = SparseMerkleTree::new();
        let empty = build_empty_chain();
        assert_eq!(
            root(&e.snapshot().unwrap(), tree, &empty).unwrap(),
            reference.root()
        );
        let edge = [[0u8; 16], [0xFF; 16], [0x80; 16], [0x7F; 16]];
        let mut keys: Vec<[u8; 16]> = edge.to_vec();
        keys.extend((0..300).map(key));
        for (i, k) in keys.iter().enumerate() {
            let content = (i as u64).to_le_bytes();
            let r = put_leaf(&e, tree, *k, &content);
            reference.insert(*k, &content);
            assert_eq!(r, reference.root());
        }
        // Overwrites.
        for k in &keys[..20] {
            put_leaf(&e, tree, *k, b"again");
            reference.insert(*k, b"again");
        }
        let mut probe: Vec<[u8; 16]> = keys.iter().step_by(37).copied().collect();
        probe.push(key(10_000));
        probe.push([0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        check_against(&e, tree, &reference, &probe);
        // Other trees are untouched.
        assert_eq!(
            root(&e.snapshot().unwrap(), TreeId(2), &empty).unwrap(),
            empty[SMT_DEPTH]
        );
    }

    #[test]
    fn a_tree_of_n_leaves_stores_n_minus_one_branches() {
        let e = RedbEngine::in_memory().unwrap();
        let tree = TreeId(0);
        for i in 0..200 {
            put_leaf(&e, tree, key(i), b"x");
        }
        let snap = e.snapshot().unwrap();
        let branches =
            crate::engine::Snapshot::scan(&snap, Space::Node, &[tree.0], None, usize::MAX)
                .unwrap()
                .len();
        assert_eq!(branches, 199);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, failure_persistence: None, .. ProptestConfig::default() })]
        #[test]
        fn random_trees_match_the_reference(
            seeds in proptest::collection::vec(any::<u64>(), 1..60),
            // Keys sharing long prefixes, to exercise deep branches.
            close in proptest::collection::vec((any::<u8>(), 0usize..16), 0..20),
            probes in proptest::collection::vec(any::<u64>(), 1..8),
        ) {
            let e = RedbEngine::in_memory().unwrap();
            let tree = TreeId::SERVICE;
            let mut reference = SparseMerkleTree::new();
            let mut keys: Vec<[u8; 16]> = seeds.iter().map(|s| key(*s)).collect();
            for (b, pos) in close {
                let mut k = keys[0];
                k[pos] ^= b | 1;
                keys.push(k);
            }
            for (i, k) in keys.iter().enumerate() {
                let content = [i as u8; 3];
                put_leaf(&e, tree, *k, &content);
                reference.insert(*k, &content);
            }
            let mut probe: Vec<[u8; 16]> = probes.iter().map(|p| key(p ^ 0xABCD)).collect();
            probe.extend(keys.iter().take(3));
            check_against(&e, tree, &reference, &probe);
        }
    }
}

/// `DEC-25`: the stored tree in a v2 build against `compact_smt::v2`'s own
/// definition, proofs and multiproof, for the same writes in any order, with removals.
#[cfg(all(test, not(feature = "smt-v1")))]
mod v2_tests {
    use super::*;
    use crate::engine::{Durability, Engine, RedbEngine};
    use cipher_clerk::crypto::blake2b_256;
    use cipher_clerk::merkle::smt_leaf_hash;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn key(i: u64) -> [u8; 16] {
        let h = blake2b_256(b"trie-v2-key", &[&i.to_le_bytes()]);
        let mut k = [0u8; 16];
        k.copy_from_slice(&h[..16]);
        k
    }

    fn write(e: &RedbEngine, tree: TreeId, k: [u8; 16], h: [u8; 32]) -> [u8; 32] {
        let empty = cipher_clerk::merkle::build_empty_chain();
        let snap = e.snapshot().unwrap();
        let mut txn = Txn::new(&snap);
        txn.put(Space::Leaf, &leaf_key(tree, &k), h.to_vec());
        let r = upsert(&mut txn, tree, &empty, &k, h).unwrap();
        e.write(txn.into_batch(), Durability::Visible).unwrap();
        r
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 32, failure_persistence: None, .. ProptestConfig::default() })]
        #[test]
        fn v2_trie_matches_the_definition_and_its_proofs(
            seeds in proptest::collection::vec(any::<u64>(), 1..60),
            close in proptest::collection::vec((any::<u8>(), 0usize..16), 0..15),
            removals in proptest::collection::vec(any::<prop::sample::Index>(), 0..6),
            probes in proptest::collection::vec(any::<u64>(), 1..8),
            reverse in any::<bool>(),
        ) {
            let mut keys: Vec<[u8; 16]> = seeds.iter().map(|s| key(*s)).collect();
            for (b, pos) in close {
                let mut k = keys[0];
                k[pos] ^= b | 1;
                keys.push(k);
            }
            let (e, other) = (RedbEngine::in_memory().unwrap(), RedbEngine::in_memory().unwrap());
            let tree = TreeId::SERVICE;
            let mut model: BTreeMap<[u8; 16], [u8; 32]> = BTreeMap::new();
            for (i, k) in keys.iter().enumerate() {
                let h = smt_leaf_hash(&(i as u64).to_le_bytes());
                model.insert(*k, h);
                let r = write(&e, tree, *k, h);
                let want = v2::root_sorted(&model.iter().map(|(k, h)| (*k, *h)).collect::<Vec<_>>());
                prop_assert_eq!(r, want);
            }
            for r in &removals {
                let k = keys[r.index(keys.len())];
                model.remove(&k);
                let got = write(&e, tree, k, SMT_EMPTY_LEAF);
                let want = v2::root_sorted(&model.iter().map(|(k, h)| (*k, *h)).collect::<Vec<_>>());
                prop_assert_eq!(got, want);
            }
            // The same final leaves written in another order give the same root.
            let mut order: Vec<_> = model.iter().map(|(k, h)| (*k, *h)).collect();
            if reverse {
                order.reverse();
            }
            let mut last = v2::EMPTY;
            for (k, h) in &order {
                last = write(&other, tree, *k, *h);
            }
            let leaves: Vec<_> = model.iter().map(|(k, h)| (*k, *h)).collect();
            let root = v2::root_sorted(&leaves);
            if !order.is_empty() {
                prop_assert_eq!(last, root);
            }
            let snap = e.snapshot().unwrap();
            let empty = cipher_clerk::merkle::build_empty_chain();
            prop_assert_eq!(super::root(&snap, tree, &empty).unwrap(), root);
            let mut probe: Vec<[u8; 16]> = probes.iter().map(|p| key(p ^ 0xBEEF)).collect();
            probe.extend(keys.iter().take(4));
            for k in &probe {
                let p = prove_v2(&snap, tree, k).unwrap();
                prop_assert_eq!(&p, &v2::prove(&leaves, k));
                prop_assert!(p.verify(&root, k).is_some());
            }
            for open in [false, true] {
                let m = multiproof(&snap, tree, &probe, open).unwrap();
                prop_assert_eq!(&m, &MultiProof::build(&leaves, &probe, open));
            }
        }
    }
}
