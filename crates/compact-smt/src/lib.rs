// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # compact-smt
//!
//! The book's state commitment (`DEC-25`, amendment 0019, ruled by the owner on 2026-10-05: v2
//! from genesis; `proposals/cipherclerk/0006-compact-smt.md`). The book's trees are CipherClerk's sparse Merkle
//! trees: depth 128 over 16-byte keys, bits read most significant first. In **v1** (CipherClerk's
//! today) every path is 128 node hashes, almost all of them over one non-empty child and an empty
//! sibling. **v2** is the same key space with the Diem/Aptos "Jellyfish" hashing: a subtree that
//! holds one leaf is that leaf's node, `H(leaf-node ‖ key ‖ leaf hash)`, wherever it stands; an
//! empty subtree is 32 zero bytes; every other subtree is `H(node ‖ left ‖ right)`. A path is
//! then about `log₂ n + 1` hashes instead of 128.
//!
//! - [`v2`]: the tree's definition, its root over sorted leaves, single-key inclusion and
//!   non-inclusion proofs, the multiproof the Agent's witnesses carry, and the composite state
//!   root with its version byte.
//! - [`Version`]: which root format is active: v2, or v1 with the feature `v1` for the whole
//!   build (a supported opt-in, owner ruling cs-q04). Both are always compiled, so a differential
//!   test runs the same transitions under each.
//!
//! **Leaves do not change.** A leaf's content and its hash (`smt_leaf_hash`) are the kernel's in
//! both versions; only the tree above the leaves is hashed differently. The v2 roots of a book
//! are therefore recomputable from its leaf store at any time.

#![no_std]

extern crate alloc;

pub mod v2;

use cipher_clerk::merkle::SMT_DEPTH;

/// A root format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Version {
    /// CipherClerk's full-depth tree: 128 node hashes a path.
    V1,
    /// The compact tree ([`v2`]).
    V2,
}

/// The v1 tree's empty root: the empty-subtree hash at height 128 (`SMT_EMPTY_CHAIN[128]`).
/// A constant of the tree's definition; a test holds it to CipherClerk's recurrence.
pub const V1_EMPTY_ROOT: [u8; 32] = [
    0x3b, 0xed, 0x4f, 0xc2, 0xcf, 0xe3, 0x5d, 0xf2, 0xf0, 0xe7, 0x74, 0x41, 0x5e, 0xff, 0x9f, 0x5e,
    0x48, 0xb9, 0x0f, 0x3d, 0x9c, 0xad, 0x3f, 0x7a, 0x16, 0x32, 0xd9, 0x10, 0x20, 0x01, 0x1b, 0xaa,
];

impl Version {
    /// The active version of this build: v2 (`DEC-25`), or v1 with the opt-in feature `v1`.
    pub const ACTIVE: Version = if cfg!(feature = "v1") {
        Version::V1
    } else {
        Version::V2
    };

    /// The version byte: `1` or `2`.
    pub const fn byte(self) -> u8 {
        match self {
            Version::V1 => 1,
            Version::V2 => v2::VERSION,
        }
    }

    /// The root of an empty tree.
    pub const fn empty_root(self) -> [u8; 32] {
        match self {
            Version::V1 => V1_EMPTY_ROOT,
            Version::V2 => v2::EMPTY,
        }
    }

    /// The root of the tree holding `leaves`: `(key, leaf hash)`, sorted ascending by key, keys
    /// unique. A leaf whose hash is the empty sentinel is absent, in both versions.
    pub fn root_of_sorted(self, leaves: &[([u8; 16], [u8; 32])]) -> [u8; 32] {
        match self {
            Version::V1 => {
                let chain = cipher_clerk::merkle::build_empty_chain();
                cipher_clerk::state_root::sparse_root_sorted(leaves, 0, SMT_DEPTH, &chain)
            }
            Version::V2 => v2::root_sorted(leaves),
        }
    }

    /// The kernel's composite state root over its six sub-roots, in composite order.
    pub fn composite(self, sub: &[[u8; 32]; 6]) -> [u8; 32] {
        match self {
            Version::V1 => cipher_clerk::state_root::composite_root_from_subroots(
                &sub[0], &sub[1], &sub[2], &sub[3], &sub[4], &sub[5],
            ),
            Version::V2 => v2::composite_root(sub),
        }
    }
}

/// Bit `pos` of `key`, most significant first.
#[inline]
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

/// `prefix` with bit `pos` set.
pub fn with_bit(prefix: [u8; 16], pos: usize) -> [u8; 16] {
    let mut p = prefix;
    p[pos / 8] |= 1 << (7 - pos % 8);
    p
}

/// How many leading bits `a` and `b` share (128 when equal).
pub fn common_prefix(a: &[u8; 16], b: &[u8; 16]) -> usize {
    for i in 0..16 {
        let x = a[i] ^ b[i];
        if x != 0 {
            return i * 8 + x.leading_zeros() as usize;
        }
    }
    SMT_DEPTH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_v1_empty_root_is_cipherclerks() {
        assert_eq!(
            V1_EMPTY_ROOT,
            cipher_clerk::merkle::build_empty_chain()[SMT_DEPTH]
        );
        assert_eq!(Version::V1.root_of_sorted(&[]), V1_EMPTY_ROOT);
        assert_eq!(Version::V2.root_of_sorted(&[]), v2::EMPTY);
    }

    #[test]
    fn the_v1_composite_is_cipherclerks() {
        let empty = [V1_EMPTY_ROOT; 6];
        assert_eq!(
            Version::V1.composite(&empty),
            cipher_clerk::state_root::composite_state_root(&[], &[], None, &[], &[], &[])
        );
    }

    #[test]
    fn versions_never_share_a_root() {
        let leaves = [([7u8; 16], [9u8; 32])];
        assert_ne!(
            Version::V1.root_of_sorted(&leaves),
            Version::V2.root_of_sorted(&leaves)
        );
        assert_ne!(
            Version::V1.composite(&[V1_EMPTY_ROOT; 6]),
            Version::V2.composite(&[v2::EMPTY; 6])
        );
    }

    #[test]
    fn masks_and_prefixes() {
        let k = [0xFF; 16];
        assert_eq!(mask(&k, 0), [0; 16]);
        assert_eq!(mask(&k, 128), k);
        assert_eq!(mask(&k, 12)[1], 0xF0);
        assert_eq!(with_bit([0; 16], 9)[1], 0x40);
        assert_eq!(common_prefix(&[0; 16], &[0; 16]), 128);
        assert_eq!(common_prefix(&[0; 16], &with_bit([0; 16], 77)), 77);
    }
}
