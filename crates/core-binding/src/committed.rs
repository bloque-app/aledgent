// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Committed maps: sparse Merkle trees kept as the Agent's own rows (`F-031` lever 2, `DEC-22`,
//! `T-031-06`).
//!
//! The kernel's six trees and the service tree move into the Agent, each a **committed map**: a
//! sparse Merkle tree over its leaves, stored as ordinary storage rows and maintained
//! incrementally. Its shape is CipherClerk's, so its root is byte for byte the root the kernel,
//! `F-006`'s leaf store and VOS's `committed.rs` compute for the same leaves: depth 128 over
//! 16-byte keys, bits read most significant first, leaves hashed with `SMT_LEAF`, nodes with
//! `SMT_NODE`, an absent subtree taking the empty-subtree hash of its height.
//!
//! **Only branching nodes are stored** (the same structure as `leaf_store::trie`). A tree of `n`
//! leaves keeps `n − 1` branch rows. A branch at level `L` keeps, for each side, a reference to
//! the next stored node below and that side's hash at level `L + 1`.
//!
//! | Row | Key | Value |
//! |---|---|---|
//! | leaf | `m ‖ tree ‖ l ‖ key` | the leaf hash, then the leaf's stored bytes |
//! | branch | `m ‖ tree ‖ n ‖ level ‖ prefix` | left ref, left hash, right ref, right hash (98 B) |
//! | top | `m ‖ tree ‖ t` | top ref, root (49 B) |
//!
//! **Cost.** A read is one row. A write reads and rewrites the branches on its key's path (about
//! `log₂ n` of them) and hashes the 128 levels of the path. Nothing else is read, decoded or
//! hashed: the cost of an invocation follows the leaves it touches, not the size of the book.
//!
//! **Root versions** (`DEC-25`). In a v2 build the same rows hold v2 hashes
//! (`compact_smt::v2`, as `leaf_store::trie` does): a write hashes one leaf node and one node a
//! branch on its path, about `log₂ n + 1` hashes instead of 128, and a removal takes the leaf out
//! of the structure (its row stays, emptied), so every branch keeps two present sides.
//!
//! **Removal.** A removed leaf keeps its place with the empty sentinel as its hash, as `F-006`
//! does: the root is the same as if the key had never been written, because an empty leaf folds
//! up through empty siblings to the empty subtree's hash.

use alloc::vec::Vec;

use cipher_clerk::merkle::{SMT_DEPTH, SMT_EMPTY_LEAF, build_empty_chain, smt_node_hash};
use ledger_kernel::compact_smt::Version;
use ledger_kernel::compact_smt::v2::{self, Summary};

use crate::transition::Rows;

/// The empty-subtree hashes, by height.
pub type EmptyChain = [[u8; 32]; SMT_DEPTH + 1];

/// The rows' common prefix.
pub const PREFIX: u8 = b'm';
const LEAF: u8 = b'l';
const NODE: u8 = b'n';
const TOP: u8 = b't';
const LEAF_LEVEL: u8 = SMT_DEPTH as u8;

/// Why a committed map could not be read: a row that does not decode. Never expected; the
/// transition refuses and commits nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Corrupt(pub &'static str);

/// A stored node: a branch at `level` < 128, or a leaf at level 128 whose prefix is its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NodeRef {
    level: u8,
    prefix: [u8; 16],
}

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

    fn decode(b: &[u8]) -> Result<Self, Corrupt> {
        if b.len() != 17 || b[0] > LEAF_LEVEL {
            return Err(Corrupt("node reference"));
        }
        Ok(Self {
            level: b[0],
            prefix: array16(&b[1..]),
        })
    }
}

#[derive(Clone, Copy, Debug)]
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

    fn decode(b: &[u8]) -> Result<Self, Corrupt> {
        if b.len() != 98 {
            return Err(Corrupt("branch"));
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

fn array16(b: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(b);
    out
}

fn bit(key: &[u8; 16], pos: usize) -> bool {
    (key[pos / 8] >> (7 - pos % 8)) & 1 == 1
}

fn mask(key: &[u8; 16], bits: usize) -> [u8; 16] {
    let mut out = [0u8; 16];
    let full = bits / 8;
    out[..full].copy_from_slice(&key[..full]);
    if full < 16 && bits % 8 != 0 {
        out[full] = key[full] & (0xFFu8 << (8 - bits % 8));
    }
    out
}

fn common_prefix(a: &[u8; 16], b: &[u8; 16]) -> usize {
    for i in 0..16 {
        let x = a[i] ^ b[i];
        if x != 0 {
            return i * 8 + x.leading_zeros() as usize;
        }
    }
    SMT_DEPTH
}

/// Fold the hash `h` of the subtree at `from` up to level `to`, through empty siblings.
fn fold(empty: &EmptyChain, mut h: [u8; 32], prefix: &[u8; 16], from: u8, to: u8) -> [u8; 32] {
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

/// One committed map over the Agent's rows: tree `tree` (module documentation).
pub struct CommittedMap<'a> {
    rows: &'a mut dyn Rows,
    tree: u8,
    empty: &'a EmptyChain,
}

/// The empty-subtree chain, computed once per invocation and shared by every map.
pub fn empty_chain() -> EmptyChain {
    build_empty_chain()
}

/// The key of tree `tree`'s leaf row for `key`.
pub fn leaf_row(tree: u8, key: &[u8; 16]) -> [u8; 19] {
    let mut k = [0u8; 19];
    k[0] = PREFIX;
    k[1] = tree;
    k[2] = LEAF;
    k[3..].copy_from_slice(key);
    k
}

fn node_row(tree: u8, r: &NodeRef) -> [u8; 20] {
    let mut k = [0u8; 20];
    k[0] = PREFIX;
    k[1] = tree;
    k[2] = NODE;
    k[3] = r.level;
    k[4..].copy_from_slice(&r.prefix);
    k
}

fn top_row(tree: u8) -> [u8; 3] {
    [PREFIX, tree, TOP]
}

/// A leaf as it stands: its hash and its stored bytes, or nothing.
pub type Leaf = Option<([u8; 32], Vec<u8>)>;

/// Leaves by key: each key and its stored bytes.
pub type Entries = Vec<([u8; 16], Vec<u8>)>;

fn top_of(rows: &dyn Rows, tree: u8) -> Result<Option<(NodeRef, [u8; 32])>, Corrupt> {
    match rows.get(&top_row(tree)) {
        None => Ok(None),
        Some(v) if v.len() == 49 => Ok(Some((NodeRef::decode(&v[..17])?, array32(&v[17..])))),
        Some(_) => Err(Corrupt("top")),
    }
}

/// The root of map `tree` over `rows`, read only.
pub fn root(rows: &dyn Rows, tree: u8, empty: &EmptyChain) -> Result<[u8; 32], Corrupt> {
    let empty_root = match Version::ACTIVE {
        Version::V1 => empty[SMT_DEPTH],
        Version::V2 => v2::EMPTY,
    };
    Ok(top_of(rows, tree)?.map_or(empty_root, |(_, r)| r))
}

/// The leaf of map `tree` at `key`, read only: its hash and stored bytes, or nothing if absent
/// or removed.
pub fn get(rows: &dyn Rows, tree: u8, key: &[u8; 16]) -> Result<Leaf, Corrupt> {
    match rows.get(&leaf_row(tree, key)) {
        None => Ok(None),
        Some(v) if v.len() < 32 => Err(Corrupt("leaf")),
        Some(v) => {
            let hash = array32(&v[..32]);
            if hash == SMT_EMPTY_LEAF {
                return Ok(None);
            }
            Ok(Some((hash, v[32..].to_vec())))
        }
    }
}

/// Every present leaf of map `tree` whose key starts with the first `bits` bits of `prefix`, in
/// key order: a walk of the branches under that prefix, reading nothing outside it.
pub fn leaves_under(
    rows: &dyn Rows,
    tree: u8,
    prefix: &[u8; 16],
    bits: u8,
) -> Result<Entries, Corrupt> {
    let mut out = Vec::new();
    let Some((top, _)) = top_of(rows, tree)? else {
        return Ok(out);
    };
    let mut stack = alloc::vec![top];
    while let Some(r) = stack.pop() {
        let shared = common_prefix(prefix, &r.prefix).min(usize::from(r.level));
        if shared < usize::from(bits.min(r.level)) {
            continue;
        }
        if r.is_leaf() {
            if let Some((_, bytes)) = get(rows, tree, &r.prefix)? {
                out.push((r.prefix, bytes));
            }
            continue;
        }
        let b = match rows.get(&node_row(tree, &r)) {
            Some(v) => Branch::decode(&v)?,
            None => return Err(Corrupt("missing branch")),
        };
        stack.push(b.right);
        stack.push(b.left);
    }
    Ok(out)
}

impl<'a> CommittedMap<'a> {
    /// The map `tree` over `rows`.
    pub fn new(rows: &'a mut dyn Rows, tree: u8, empty: &'a EmptyChain) -> Self {
        Self { rows, tree, empty }
    }

    fn top(&self) -> Result<Option<(NodeRef, [u8; 32])>, Corrupt> {
        top_of(&*self.rows, self.tree)
    }

    /// The map's root.
    pub fn root(&self) -> Result<[u8; 32], Corrupt> {
        root(&*self.rows, self.tree, self.empty)
    }

    /// The leaf at `key`: its hash and stored bytes, or nothing if absent or removed.
    pub fn get(&self, key: &[u8; 16]) -> Result<Leaf, Corrupt> {
        get(&*self.rows, self.tree, key)
    }

    fn leaf_hash(&self, key: &[u8; 16]) -> Result<[u8; 32], Corrupt> {
        match self.rows.get(&leaf_row(self.tree, key)) {
            Some(v) if v.len() >= 32 => Ok(array32(&v[..32])),
            _ => Err(Corrupt("a reference names a missing leaf")),
        }
    }

    fn branch(&self, r: &NodeRef) -> Result<Branch, Corrupt> {
        match self.rows.get(&node_row(self.tree, r)) {
            Some(v) => Branch::decode(&v),
            None => Err(Corrupt("missing branch")),
        }
    }

    fn own(&self, r: &NodeRef) -> Result<[u8; 32], Corrupt> {
        if r.is_leaf() {
            self.leaf_hash(&r.prefix)
        } else {
            Ok(self.branch(r)?.own())
        }
    }

    /// Set the leaf at `key` to `hash` with `bytes` stored beside it, and update its path.
    /// `None` removes it (the empty sentinel, module documentation). Returns the new root.
    pub fn put(
        &mut self,
        key: &[u8; 16],
        leaf: Option<([u8; 32], &[u8])>,
    ) -> Result<[u8; 32], Corrupt> {
        let leaf_hash = leaf.map_or(SMT_EMPTY_LEAF, |(h, _)| h);
        if leaf.is_none() && self.rows.get(&leaf_row(self.tree, key)).is_none() {
            // Removing what was never there changes nothing.
            return self.root();
        }
        let mut row = Vec::with_capacity(32 + leaf.map_or(0, |(_, b)| b.len()));
        row.extend_from_slice(&leaf_hash);
        if let Some((_, b)) = leaf {
            row.extend_from_slice(b);
        }
        self.rows.put(&leaf_row(self.tree, key), row);
        if Version::ACTIVE == Version::V2 {
            return if leaf.is_none() {
                self.remove_v2(key)
            } else {
                self.upsert_v2(key, leaf_hash)
            };
        }
        let empty = self.empty;
        let mut path: Vec<(NodeRef, Branch, bool)> = Vec::new();
        let mut cur = self.top()?.map(|(r, _)| r);
        let (mut child, mut child_own) = loop {
            let Some(r) = cur else {
                break (NodeRef::leaf(*key), leaf_hash);
            };
            let cpl = common_prefix(key, &r.prefix).min(usize::from(r.level));
            if cpl < usize::from(r.level) {
                let r_own = self.own(&r)?;
                let new = NodeRef::leaf(*key);
                let level = cpl as u8;
                let (l, lh, rr, rh) = if bit(key, cpl) {
                    (r, r_own, new, leaf_hash)
                } else {
                    (new, leaf_hash, r, r_own)
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
                self.rows.put(&node_row(self.tree, &at), b.encode());
                break (at, b.own());
            }
            if r.is_leaf() {
                break (r, leaf_hash);
            }
            let b = self.branch(&r)?;
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
            self.rows.put(&node_row(self.tree, &at), b.encode());
            child = at;
            child_own = b.own();
        }
        let root = fold(empty, child_own, &child.prefix, child.level, 0);
        let mut v = Vec::with_capacity(49);
        child.encode(&mut v);
        v.extend_from_slice(&root);
        self.rows.put(&top_row(self.tree), v);
        Ok(root)
    }
}

/// A stored node's own v2 summary at its own level (module documentation).
impl CommittedMap<'_> {
    fn own_v2(&self, r: &NodeRef) -> Result<Summary, Corrupt> {
        if r.is_leaf() {
            Ok(Summary::leaf(&r.prefix, &self.leaf_hash(&r.prefix)?))
        } else {
            let b = self.branch(r)?;
            Ok(Summary::Many(v2::node(&b.left_hash, &b.right_hash)))
        }
    }

    fn upsert_v2(&mut self, key: &[u8; 16], leaf_hash: [u8; 32]) -> Result<[u8; 32], Corrupt> {
        let leaf_own = Summary::leaf(key, &leaf_hash);
        let mut path: Vec<(NodeRef, Branch, bool)> = Vec::new();
        let mut cur = self.top()?.map(|(r, _)| r);
        let (child, child_own) = loop {
            let Some(r) = cur else {
                break (NodeRef::leaf(*key), leaf_own);
            };
            let cpl = common_prefix(key, &r.prefix).min(usize::from(r.level));
            if cpl < usize::from(r.level) {
                let r_own = self.own_v2(&r)?;
                let new = NodeRef::leaf(*key);
                let (l, lo, rr, ro) = if bit(key, cpl) {
                    (r, r_own, new, leaf_own)
                } else {
                    (new, leaf_own, r, r_own)
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
                self.rows.put(&node_row(self.tree, &at), b.encode());
                break (at, Summary::Many(v2::node(&b.left_hash, &b.right_hash)));
            }
            if r.is_leaf() {
                break (r, leaf_own);
            }
            let b = self.branch(&r)?;
            let side = bit(key, usize::from(r.level));
            path.push((r, b, side));
            cur = Some(if side { b.right } else { b.left });
        };
        Ok(self.finish_v2(path, child, child_own))
    }

    fn remove_v2(&mut self, key: &[u8; 16]) -> Result<[u8; 32], Corrupt> {
        let Some((mut cur, root)) = self.top()? else {
            return Ok(v2::EMPTY);
        };
        let mut path: Vec<(NodeRef, Branch, bool)> = Vec::new();
        loop {
            let cpl = common_prefix(key, &cur.prefix).min(usize::from(cur.level));
            if cpl < usize::from(cur.level) {
                return Ok(root);
            }
            if cur.is_leaf() {
                break;
            }
            let b = self.branch(&cur)?;
            let side = bit(key, usize::from(cur.level));
            path.push((cur, b, side));
            cur = if side { b.right } else { b.left };
        }
        let Some((parent, pb, side)) = path.pop() else {
            self.rows.remove(&top_row(self.tree));
            return Ok(v2::EMPTY);
        };
        let sibling = if side { pb.left } else { pb.right };
        let sibling_own = self.own_v2(&sibling)?;
        self.rows.remove(&node_row(self.tree, &parent));
        Ok(self.finish_v2(path, sibling, sibling_own))
    }

    fn finish_v2(
        &mut self,
        path: Vec<(NodeRef, Branch, bool)>,
        mut child: NodeRef,
        mut child_own: Summary,
    ) -> [u8; 32] {
        for (at, mut b, side) in path.into_iter().rev() {
            let h = lifted(child_own, &child, usize::from(at.level) + 1).hash();
            if side {
                b.right = child;
                b.right_hash = h;
            } else {
                b.left = child;
                b.left_hash = h;
            }
            self.rows.put(&node_row(self.tree, &at), b.encode());
            child = at;
            child_own = Summary::Many(v2::node(&b.left_hash, &b.right_hash));
        }
        let root = lifted(child_own, &child, 0).hash();
        let mut v = Vec::with_capacity(49);
        child.encode(&mut v);
        v.extend_from_slice(&root);
        self.rows.put(&top_row(self.tree), v);
        root
    }
}

/// `own`, standing at `r.level`, seen from level `to`.
fn lifted(own: Summary, r: &NodeRef, to: usize) -> Summary {
    own.lift(&r.prefix, usize::from(r.level), to)
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;

    use cipher_clerk::merkle::{SparseMerkleTree, smt_leaf_hash};

    use super::*;
    use crate::transition::MemRows;

    fn key(n: u64) -> [u8; 16] {
        let d = cipher_clerk::crypto::blake2b_256(b"test", &[&n.to_be_bytes()]);
        array16(&d[..16])
    }

    /// CipherClerk's own tree over the same leaves: the reference (in a v2 build, the same
    /// leaves under `compact_smt::v2`).
    fn reference(leaves: &BTreeMap<[u8; 16], Vec<u8>>) -> [u8; 32] {
        if Version::ACTIVE == Version::V2 {
            let sorted: Vec<([u8; 16], [u8; 32])> =
                leaves.iter().map(|(k, c)| (*k, smt_leaf_hash(c))).collect();
            return v2::root_sorted(&sorted);
        }
        let mut t = SparseMerkleTree::new();
        for (k, content) in leaves {
            t.insert(*k, content);
        }
        t.root()
    }

    #[test]
    fn req_rt_3_roots_equal_cipherclerks_under_inserts_updates_and_removals() {
        let empty = empty_chain();
        let mut rows = MemRows::default();
        let mut model: BTreeMap<[u8; 16], Vec<u8>> = BTreeMap::new();
        let mut map = CommittedMap::new(&mut rows, 3, &empty);
        assert_eq!(map.root().unwrap(), Version::ACTIVE.empty_root());
        for n in 0..200u64 {
            let k = key(n % 150);
            if n % 7 == 3 {
                map.put(&k, None).unwrap();
                model.remove(&k);
            } else {
                let content = n.to_be_bytes();
                map.put(&k, Some((smt_leaf_hash(&content), &content)))
                    .unwrap();
                model.insert(k, content.to_vec());
            }
            assert_eq!(map.root().unwrap(), reference(&model), "after {n}");
        }
        for (k, content) in &model {
            assert_eq!(
                map.get(k).unwrap(),
                Some((smt_leaf_hash(content), content.clone()))
            );
        }
        assert_eq!(map.get(&key(9999)).unwrap(), None);
    }

    #[test]
    fn leaves_under_a_prefix_are_every_leaf_of_that_subtree_and_no_other() {
        let empty = empty_chain();
        let mut rows = MemRows::default();
        let mut keys: Vec<[u8; 16]> = (0..100).map(key).collect();
        for (i, k) in keys.iter_mut().enumerate() {
            k[0] = (i % 4) as u8;
        }
        for k in &keys {
            let content = k.to_vec();
            CommittedMap::new(&mut rows, 6, &empty)
                .put(k, Some((smt_leaf_hash(&content), &content)))
                .unwrap();
        }
        let mut p = [0u8; 16];
        p[0] = 2;
        let got = leaves_under(&rows, 6, &p, 8).unwrap();
        let mut want: Vec<[u8; 16]> = keys.iter().filter(|k| k[0] == 2).copied().collect();
        want.sort();
        assert_eq!(got.iter().map(|(k, _)| *k).collect::<Vec<_>>(), want);
        assert_eq!(leaves_under(&rows, 6, &[9; 16], 8).unwrap(), Vec::new());
    }

    #[test]
    fn a_tree_of_n_leaves_stores_n_minus_one_branches_and_order_does_not_matter() {
        let empty = empty_chain();
        let (mut a, mut b) = (MemRows::default(), MemRows::default());
        let keys: Vec<[u8; 16]> = (0..64).map(key).collect();
        let h = smt_leaf_hash(b"x");
        for k in &keys {
            CommittedMap::new(&mut a, 0, &empty)
                .put(k, Some((h, b"x")))
                .unwrap();
        }
        for k in keys.iter().rev() {
            CommittedMap::new(&mut b, 0, &empty)
                .put(k, Some((h, b"x")))
                .unwrap();
        }
        assert_eq!(a, b, "the rows are a function of the key set");
        let branches = a.0.keys().filter(|k| k.len() == 20 && k[2] == NODE).count();
        assert_eq!(branches, keys.len() - 1);
    }
}
