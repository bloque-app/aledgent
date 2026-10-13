// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! An incremental sparse Merkle tree over a node table (`F-001` §10).
//!
//! Same shape as CipherClerk's `SparseMerkleTree`: depth 128 over 16-byte
//! keys, bits read most-significant first, leaves hashed with `SMT_LEAF`,
//! nodes with `SMT_NODE`, absent subtrees taking the empty-subtree hash of
//! their height. Instead of recomputing the root from every leaf, each write
//! updates the 128 nodes on its path, reading the 128 siblings it needs.
//!
//! A table that can say how many leading bits a key shares with its nearest other leaf
//! ([`NodeTable::neighbours`], `T-031-19`) saves most of those reads: every sibling deeper than
//! that is an empty subtree, hashed from the empty chain without a read. In a tree of `n` random
//! keys a write then reads about `log2(n)` siblings instead of 128.
//!
//! **Root versions** (`DEC-25`). A table hashes as its [`NodeTable::version`]
//! says: v1 (above), or v2 ([`compact_smt::v2`]), where a node holds its subtree's v2 hash: a
//! lone leaf's shortcut node, or a node over two children, or nothing. [`set_leaf`] walks the
//! same 128 levels and reads the same siblings in both; v2 hashes only where both sides hold a
//! leaf (and once a level above a lone *pair*'s chain), about `log₂ n + 1` hashes a write.
//!
//! A write therefore costs the same whatever the size of the tree. The
//! kernel's own `SparseMerkleTree::root` rebuilds the root from every leaf on
//! each call, so a store that asked it for a root per commit paid a cost that
//! grew with the book (issue #37); both stores use this module instead.

use alloc::collections::BTreeMap;

use cipher_clerk::merkle::{SMT_DEPTH, SMT_EMPTY_LEAF, build_empty_chain, smt_node_hash};
use compact_smt::Version;
use compact_smt::v2::{self, Summary};

/// A node's address: its depth from the root (0 = root, 128 = leaf) and the
/// first `depth` bits of the keys below it, the rest zeroed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeAddr {
    /// Depth from the root.
    pub depth: u8,
    /// The key prefix, masked to `depth` bits.
    pub prefix: [u8; 16],
}

impl NodeAddr {
    /// The address of the node at `depth` above `key`.
    pub fn above(key: &[u8; 16], depth: usize) -> Self {
        Self {
            depth: depth as u8,
            prefix: mask(key, depth),
        }
    }

    /// The 17-byte encoding: depth, then prefix.
    pub fn to_bytes(&self) -> [u8; 17] {
        let mut out = [0u8; 17];
        out[0] = self.depth;
        out[1..].copy_from_slice(&self.prefix);
        out
    }
}

/// `key` with every bit from position `bits` on cleared.
fn mask(key: &[u8; 16], bits: usize) -> [u8; 16] {
    let mut out = [0u8; 16];
    let full = bits / 8;
    out[..full].copy_from_slice(&key[..full]);
    if full < 16 && bits % 8 != 0 {
        let keep = 0xFFu8 << (8 - bits % 8);
        out[full] = key[full] & keep;
    }
    out
}

/// Bit `pos` of `key`, most significant first.
fn bit(key: &[u8; 16], pos: usize) -> bool {
    (key[pos / 8] >> (7 - pos % 8)) & 1 == 1
}

/// What a table knows of the leaves nearest a key, other than the key itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Neighbours {
    /// The table cannot tell cheaply: every sibling is read.
    Unknown,
    /// No other leaf: every sibling on the key's path is an empty subtree.
    Alone,
    /// The most leading bits the key shares with any other leaf: a sibling at a greater depth
    /// is an empty subtree.
    Shares(usize),
}

/// The number of leading bits `a` and `b` share (128 when equal).
pub fn shared_bits(a: &[u8; 16], b: &[u8; 16]) -> usize {
    for i in 0..16 {
        let x = a[i] ^ b[i];
        if x != 0 {
            return i * 8 + x.leading_zeros() as usize;
        }
    }
    128
}

/// [`Neighbours`] from the leaves just before and just after a key, in key order.
pub fn neighbours_of(
    key: &[u8; 16],
    before: Option<&[u8; 16]>,
    after: Option<&[u8; 16]>,
) -> Neighbours {
    match (before, after) {
        (None, None) => Neighbours::Alone,
        (b, a) => Neighbours::Shares(
            b.map_or(0, |b| shared_bits(key, b))
                .max(a.map_or(0, |a| shared_bits(key, a))),
        ),
    }
}

/// A table of nodes. Absent nodes are empty subtrees.
pub trait NodeTable {
    /// The error of the underlying storage.
    type Error;
    /// The node's hash, if stored.
    fn get(&self, addr: &NodeAddr) -> Result<Option<[u8; 32]>, Self::Error>;
    /// Store the node's hash.
    fn put(&mut self, addr: &NodeAddr, hash: [u8; 32]) -> Result<(), Self::Error>;
    /// The leaves nearest `key` (`T-031-19`). The default knows nothing.
    fn neighbours(&self, _key: &[u8; 16]) -> Result<Neighbours, Self::Error> {
        Ok(Neighbours::Unknown)
    }
    /// The root version the table's nodes are hashed in. The default is the build's.
    fn version(&self) -> Version {
        Version::ACTIVE
    }
    /// How many present leaves lie under the first `depth` bits of `prefix`, counted up to 2;
    /// `None` if the table cannot tell. Only a v2 removal asks (a lone leaf beside an emptied
    /// subtree rises, a node does not); no store of the kernel's trees removes a leaf (`INV-2`).
    fn present_under(
        &self,
        _prefix: &[u8; 16],
        _depth: usize,
    ) -> Result<Option<usize>, Self::Error> {
        Ok(None)
    }
}

/// The deepest level a [`MemNodes`] stores; nodes below it are computed from the leaves.
///
/// A tree of `n` random keys shares its top `log2(n)` levels; below them almost every subtree
/// holds one leaf, and its path is a chain of nodes that only that leaf determines. Storing every
/// one of them (128 per leaf) made the in-process core's node tables 26 KB a command, most of its
/// memory (`T-031-14`, finding F6 of `F-032`). Stopping at depth 32 keeps about `32 - log2(n)`
/// nodes per leaf, and a node below it is computed when asked for: from no leaf (the empty
/// subtree, a range lookup), or from the few leaves under its prefix, by hashing up from them
/// (at most 96 hashes for one). `set_leaf` asks for the siblings on its path; below depth 32 a
/// sibling holds a leaf only when another key shares more than 32 leading bits with the one
/// written, about `n / 2^33` of the time.
pub const MEM_NODES_DEPTH: usize = 32;

/// A node table in memory, for `MemStore`: every leaf's hash, and the nodes down to
/// [`MEM_NODES_DEPTH`]; deeper nodes are computed from the leaves below them, so the table
/// answers exactly what a table of every node would.
#[derive(Clone, Debug)]
pub struct MemNodes {
    upper: BTreeMap<NodeAddr, [u8; 32]>,
    leaves: BTreeMap<[u8; 16], [u8; 32]>,
    empty: alloc::boxed::Box<EmptyChain>,
    version: Version,
}

impl Default for MemNodes {
    fn default() -> Self {
        Self::with_version(Version::ACTIVE)
    }
}

impl MemNodes {
    /// An empty table hashed in `version`.
    pub fn with_version(version: Version) -> Self {
        MemNodes {
            upper: BTreeMap::new(),
            leaves: BTreeMap::new(),
            empty: alloc::boxed::Box::new(empty_chain()),
            version,
        }
    }
}

/// The hash of the subtree at `depth` holding `leaves` (sorted, at least one, all under one
/// prefix of `depth` bits), as a table of every node would hold it, in `version`.
pub fn subtree_hash(
    version: Version,
    empty: &EmptyChain,
    depth: usize,
    leaves: &[([u8; 16], [u8; 32])],
) -> [u8; 32] {
    match version {
        Version::V1 => subtree_hash_v1(empty, depth, leaves),
        Version::V2 => v2::summary_sorted(leaves, depth).hash(),
    }
}

fn subtree_hash_v1(empty: &EmptyChain, depth: usize, leaves: &[([u8; 16], [u8; 32])]) -> [u8; 32] {
    if depth == SMT_DEPTH {
        return leaves[0].1;
    }
    let split = leaves.partition_point(|(k, _)| !bit(k, depth));
    let side = |part: &[([u8; 16], [u8; 32])]| {
        if part.is_empty() {
            empty[SMT_DEPTH - depth - 1]
        } else {
            subtree_hash_v1(empty, depth + 1, part)
        }
    };
    smt_node_hash(&side(&leaves[..split]), &side(&leaves[split..]))
}

/// `prefix` (masked to `bits`) with every bit from position `bits` on set: the last key under it.
pub fn last_under(prefix: &[u8; 16], bits: usize) -> [u8; 16] {
    let mut out = *prefix;
    for (i, b) in out.iter_mut().enumerate() {
        let from = i * 8;
        if from >= bits {
            *b = 0xFF;
        } else if from + 8 > bits {
            *b |= 0xFFu8 >> (bits - from);
        }
    }
    out
}

impl NodeTable for MemNodes {
    type Error = core::convert::Infallible;
    fn get(&self, addr: &NodeAddr) -> Result<Option<[u8; 32]>, Self::Error> {
        let depth = usize::from(addr.depth);
        if depth <= MEM_NODES_DEPTH {
            return Ok(self.upper.get(addr).copied());
        }
        if depth == SMT_DEPTH {
            return Ok(self.leaves.get(&addr.prefix).copied());
        }
        let under: alloc::vec::Vec<([u8; 16], [u8; 32])> = self
            .leaves
            .range(addr.prefix..=last_under(&addr.prefix, depth))
            .map(|(k, h)| (*k, *h))
            .collect();
        Ok((!under.is_empty()).then(|| subtree_hash(self.version, &self.empty, depth, &under)))
    }
    fn put(&mut self, addr: &NodeAddr, hash: [u8; 32]) -> Result<(), Self::Error> {
        let depth = usize::from(addr.depth);
        if depth == SMT_DEPTH {
            self.leaves.insert(addr.prefix, hash);
        } else if depth <= MEM_NODES_DEPTH {
            self.upper.insert(*addr, hash);
        }
        Ok(())
    }
    fn neighbours(&self, key: &[u8; 16]) -> Result<Neighbours, Self::Error> {
        use core::ops::Bound::{Excluded, Unbounded};
        let before = self.leaves.range(..*key).next_back().map(|(k, _)| k);
        let after = self
            .leaves
            .range((Excluded(*key), Unbounded))
            .next()
            .map(|(k, _)| k);
        Ok(neighbours_of(key, before, after))
    }
    fn version(&self) -> Version {
        self.version
    }
    fn present_under(&self, prefix: &[u8; 16], depth: usize) -> Result<Option<usize>, Self::Error> {
        Ok(Some(
            self.leaves
                .range(*prefix..=last_under(prefix, depth))
                .filter(|(_, h)| **h != SMT_EMPTY_LEAF)
                .take(2)
                .count(),
        ))
    }
}

/// The empty-subtree hashes, by height.
pub type EmptyChain = [[u8; 32]; SMT_DEPTH + 1];

/// The empty-subtree hashes, computed once.
pub fn empty_chain() -> EmptyChain {
    build_empty_chain()
}

/// The root of the tree in `nodes`.
pub fn root<T: NodeTable>(nodes: &T, empty: &EmptyChain) -> Result<[u8; 32], T::Error> {
    let empty_root = match nodes.version() {
        Version::V1 => empty[SMT_DEPTH],
        Version::V2 => v2::EMPTY,
    };
    Ok(nodes
        .get(&NodeAddr {
            depth: 0,
            prefix: [0; 16],
        })?
        .unwrap_or(empty_root))
}

/// Set the leaf at `key` to `leaf_hash` and update its path. Returns the new
/// root.
pub fn set_leaf<T: NodeTable>(
    nodes: &mut T,
    empty: &EmptyChain,
    key: &[u8; 16],
    leaf_hash: [u8; 32],
) -> Result<[u8; 32], T::Error> {
    // A sibling at depth `d + 1` holds a leaf only if some other leaf shares exactly `d` bits
    // with the key; none shares more than `deepest`.
    let deepest = match nodes.neighbours(key)? {
        Neighbours::Unknown => None,
        Neighbours::Alone => Some(None),
        Neighbours::Shares(bits) => Some(Some(bits)),
    };
    if nodes.version() == Version::V2 {
        return set_leaf_v2(nodes, key, leaf_hash, deepest);
    }
    let mut current = leaf_hash;
    nodes.put(&NodeAddr::above(key, SMT_DEPTH), current)?;
    for depth in (0..SMT_DEPTH).rev() {
        // The child on the path is at depth + 1; its sibling differs in bit
        // `depth`.
        let empty_sibling = empty[SMT_DEPTH - depth - 1];
        let known_empty = match deepest {
            None => false,
            Some(None) => true,
            Some(Some(bits)) => depth > bits,
        };
        let sibling = if known_empty {
            empty_sibling
        } else {
            let mut sibling_key = *key;
            sibling_key[depth / 8] ^= 1 << (7 - depth % 8);
            nodes
                .get(&NodeAddr::above(&sibling_key, depth + 1))?
                .unwrap_or(empty_sibling)
        };
        current = if bit(key, depth) {
            smt_node_hash(&sibling, &current)
        } else {
            smt_node_hash(&current, &sibling)
        };
        nodes.put(&NodeAddr::above(key, depth), current)?;
    }
    Ok(current)
}

/// [`set_leaf`] in v2: the same walk, each node the v2 summary of its two children.
fn set_leaf_v2<T: NodeTable>(
    nodes: &mut T,
    key: &[u8; 16],
    leaf_hash: [u8; 32],
    deepest: Option<Option<usize>>,
) -> Result<[u8; 32], T::Error> {
    // Depth 128 holds the leaf's own hash, as in v1: the leaves the table computes from.
    nodes.put(&NodeAddr::above(key, SMT_DEPTH), leaf_hash)?;
    let mut current = Summary::leaf(key, &leaf_hash);
    for depth in (0..SMT_DEPTH).rev() {
        let known_empty = match deepest {
            None => false,
            Some(None) => true,
            Some(Some(bits)) => depth > bits,
        };
        let sibling = if known_empty {
            Summary::Empty
        } else {
            let mut sibling_key = *key;
            sibling_key[depth / 8] ^= 1 << (7 - depth % 8);
            let addr = NodeAddr::above(&sibling_key, depth + 1);
            match nodes.get(&addr)? {
                None => Summary::Empty,
                Some(h) if depth + 1 == SMT_DEPTH => Summary::leaf(&sibling_key, &h),
                Some(h) if h == v2::EMPTY => Summary::Empty,
                // Beside a present subtree a sibling's kind does not matter: they make a node.
                Some(h) if !current.is_empty() => Summary::Many(h),
                Some(h) => match nodes.present_under(&addr.prefix, depth + 1)? {
                    Some(0) => Summary::Empty,
                    Some(1) => Summary::One(h),
                    Some(_) => Summary::Many(h),
                    None => panic!(
                        "a v2 removal needs NodeTable::present_under; no store of the kernel's \
                         trees removes a leaf (INV-2)"
                    ),
                },
            }
        };
        current = if compact_smt::bit(key, depth) {
            Summary::combine(sibling, current)
        } else {
            Summary::combine(current, sibling)
        };
        nodes.put(&NodeAddr::above(key, depth), current.hash())?;
    }
    Ok(current.hash())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher_clerk::merkle::{SparseMerkleTree, smt_leaf_hash};

    #[test]
    fn incremental_roots_equal_the_kernels_tree() {
        let empty = empty_chain();
        let mut nodes = MemNodes::with_version(Version::V1);
        let mut reference = SparseMerkleTree::new();
        assert_eq!(root(&nodes, &empty).unwrap(), reference.root());
        let keys: [[u8; 16]; 6] = [
            [0x00; 16],
            [0xFF; 16],
            [0x80; 16],
            [0x7F; 16],
            [
                0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
                0xCD, 0xEF,
            ],
            [
                0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
                0xCD, 0xEE,
            ],
        ];
        for (i, k) in keys.iter().enumerate() {
            let content = [i as u8; 5];
            let r = set_leaf(&mut nodes, &empty, k, smt_leaf_hash(&content)).unwrap();
            reference.insert(*k, &content);
            assert_eq!(r, reference.root());
        }
        // Overwrite a leaf.
        let r = set_leaf(&mut nodes, &empty, &keys[2], smt_leaf_hash(b"new")).unwrap();
        reference.insert(keys[2], b"new");
        assert_eq!(r, reference.root());
        assert_eq!(root(&nodes, &empty).unwrap(), r);
    }

    #[test]
    fn incremental_roots_equal_the_kernels_tree_over_many_writes() {
        use cipher_clerk::crypto::blake2b_256;
        let empty = empty_chain();
        let mut nodes = MemNodes::with_version(Version::V1);
        let mut reference = SparseMerkleTree::new();
        for i in 0u32..300 {
            // Every fourth write overwrites an earlier key; the rest are new.
            let n = if i % 4 == 3 { i / 2 } else { i };
            let digest = blake2b_256(b"smt-test", &[&n.to_be_bytes()]);
            let mut key = [0u8; 16];
            key.copy_from_slice(&digest[..16]);
            let content = i.to_be_bytes();
            let r = set_leaf(&mut nodes, &empty, &key, smt_leaf_hash(&content)).unwrap();
            reference.insert(key, &content);
            assert_eq!(r, reference.root());
        }
        assert_eq!(root(&nodes, &empty).unwrap(), reference.root());
    }

    /// `T-031-14`: the pruned table answers every node as a full table would, for keys that
    /// share long prefixes (deep subtrees with several leaves) as well as random ones.
    #[test]
    fn the_pruned_table_answers_as_a_full_one() {
        use cipher_clerk::crypto::blake2b_256;
        #[derive(Default)]
        struct Full(BTreeMap<NodeAddr, [u8; 32]>);
        impl NodeTable for Full {
            type Error = core::convert::Infallible;
            fn get(&self, a: &NodeAddr) -> Result<Option<[u8; 32]>, Self::Error> {
                Ok(self.0.get(a).copied())
            }
            fn put(&mut self, a: &NodeAddr, h: [u8; 32]) -> Result<(), Self::Error> {
                self.0.insert(*a, h);
                Ok(())
            }
            fn version(&self) -> Version {
                Version::V1
            }
        }
        let empty = empty_chain();
        let mut full = Full::default();
        let mut pruned = MemNodes::with_version(Version::V1);
        let mut reference = SparseMerkleTree::new();
        let mut keys = alloc::vec::Vec::new();
        for i in 0u32..400 {
            let digest = blake2b_256(b"smt-prune", &[&i.to_be_bytes()]);
            let mut key = [0u8; 16];
            key.copy_from_slice(&digest[..16]);
            // A third of the keys share 40, 64 or 120 leading bits with an earlier one.
            if i % 3 == 0 && !keys.is_empty() {
                let base: [u8; 16] = keys[(i as usize / 3) % keys.len()];
                let share = [40usize, 64, 120][(i as usize / 3) % 3];
                for b in 0..share {
                    let (byte, mask) = (b / 8, 0x80u8 >> (b % 8));
                    key[byte] = (key[byte] & !mask) | (base[byte] & mask);
                }
            }
            // Every fifth write overwrites an earlier key.
            if i % 5 == 4 {
                key = keys[i as usize / 2];
            }
            keys.push(key);
            let content = i.to_be_bytes();
            let a = set_leaf(&mut full, &empty, &key, smt_leaf_hash(&content)).unwrap();
            let b = set_leaf(&mut pruned, &empty, &key, smt_leaf_hash(&content)).unwrap();
            reference.insert(key, &content);
            assert_eq!(a, b, "write {i}");
            assert_eq!(b, reference.root(), "write {i}");
        }
        for (addr, hash) in &full.0 {
            assert_eq!(pruned.get(addr).unwrap(), Some(*hash), "node {addr:?}");
        }
        assert!(pruned.upper.len() * 3 < full.0.len(), "the table is pruned");
    }

    /// `DEC-25`: in v2 a table's roots are `compact_smt::v2`'s over the same
    /// leaves, after every write and whatever the order of the writes, for a full table (every
    /// sibling read) and the pruned one (neighbours known), with keys sharing long prefixes; and
    /// a v2 write hashes far fewer nodes than v1's 128.
    #[test]
    fn v2_roots_equal_the_compact_reference_in_any_order() {
        use cipher_clerk::crypto::blake2b_256;
        #[derive(Default)]
        struct Full(BTreeMap<NodeAddr, [u8; 32]>);
        impl NodeTable for Full {
            type Error = core::convert::Infallible;
            fn get(&self, a: &NodeAddr) -> Result<Option<[u8; 32]>, Self::Error> {
                Ok(self.0.get(a).copied())
            }
            fn put(&mut self, a: &NodeAddr, h: [u8; 32]) -> Result<(), Self::Error> {
                self.0.insert(*a, h);
                Ok(())
            }
            fn version(&self) -> Version {
                Version::V2
            }
        }
        let empty = empty_chain();
        let mut keys = alloc::vec::Vec::new();
        for i in 0u32..300 {
            let digest = blake2b_256(b"smt-v2", &[&i.to_be_bytes()]);
            let mut key = [0u8; 16];
            key.copy_from_slice(&digest[..16]);
            if i % 3 == 0 && !keys.is_empty() {
                let base: [u8; 16] = keys[(i as usize / 3) % keys.len()];
                let share = [40usize, 64, 127][(i as usize / 3) % 3];
                for b in 0..share {
                    let (byte, m) = (b / 8, 0x80u8 >> (b % 8));
                    key[byte] = (key[byte] & !m) | (base[byte] & m);
                }
            }
            keys.push(key);
        }
        let mut model: BTreeMap<[u8; 16], [u8; 32]> = BTreeMap::new();
        let (mut full, mut pruned) = (Full::default(), MemNodes::with_version(Version::V2));
        assert_eq!(root(&pruned, &empty).unwrap(), compact_smt::v2::EMPTY);
        for (i, k) in keys.iter().enumerate() {
            let h = smt_leaf_hash(&(i as u32).to_be_bytes());
            model.insert(*k, h);
            let a = set_leaf(&mut full, &empty, k, h).unwrap();
            let b = set_leaf(&mut pruned, &empty, k, h).unwrap();
            let expected = compact_smt::v2::root_sorted(
                &model
                    .iter()
                    .map(|(k, h)| (*k, *h))
                    .collect::<alloc::vec::Vec<_>>(),
            );
            assert_eq!(a, expected, "write {i}");
            assert_eq!(b, expected, "write {i}");
        }
        // Reverse order, with an overwrite of every key by a stale value first.
        let mut again = MemNodes::with_version(Version::V2);
        for k in keys.iter().rev() {
            set_leaf(&mut again, &empty, k, smt_leaf_hash(b"stale")).unwrap();
        }
        for k in keys.iter().rev() {
            set_leaf(&mut again, &empty, k, model[k]).unwrap();
        }
        assert_eq!(
            root(&again, &empty).unwrap(),
            root(&pruned, &empty).unwrap()
        );
        // A removal (the empty sentinel) gives the root without the leaf.
        let gone = keys[7];
        set_leaf(&mut again, &empty, &gone, SMT_EMPTY_LEAF).unwrap();
        model.remove(&gone);
        let expected = compact_smt::v2::root_sorted(
            &model
                .iter()
                .map(|(k, h)| (*k, *h))
                .collect::<alloc::vec::Vec<_>>(),
        );
        assert_eq!(root(&again, &empty).unwrap(), expected);
    }

    #[test]
    fn last_under_sets_every_bit_past_the_prefix() {
        assert_eq!(last_under(&[0; 16], 0), [0xFF; 16]);
        assert_eq!(last_under(&[0xAB; 16], 128), [0xAB; 16]);
        let l = last_under(&mask(&[0x12; 16], 12), 12);
        assert_eq!(l[0], 0x12);
        assert_eq!(l[1], 0x1F);
        assert_eq!(l[2], 0xFF);
    }

    #[test]
    fn masks_keep_exactly_the_prefix() {
        let k = [0xFF; 16];
        assert_eq!(mask(&k, 0), [0; 16]);
        assert_eq!(mask(&k, 128), k);
        let m = mask(&k, 12);
        assert_eq!(m[0], 0xFF);
        assert_eq!(m[1], 0xF0);
        assert_eq!(m[2], 0);
    }
}
