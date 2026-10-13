// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The binary Merkle tree the seed and the history commitment share (`F-036`).
//!
//! **Shape.** The leaves are given in order. Each level pairs its nodes left to right, and a pair
//! becomes `blake2b-256(node domain ‖ left ‖ right)`. When a level has an odd number of nodes, the
//! last one is **carried up unchanged**: it is never paired with itself, so no two trees of
//! different sizes share a root by duplication. The root of one leaf is that leaf. A tree of no
//! leaf has no root here: each user defines its own empty value (the history's is
//! [`super::history::empty_root`]).
//!
//! **Paths.** A path is the siblings from the leaf up, one per level at which the node had one; a
//! level at which it was carried up contributes nothing. The leaf's index and the leaf count fix
//! which levels those are, so a path is checked against both ([`fold`]).
//!
//! Hashing is CipherClerk's `blake2b_256`, the one every commitment of the book uses: no host
//! call is added (`DEC-16`).

use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

/// One node of the level above `left` and `right`.
pub fn node(domain: &[u8], left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    blake2b_256(domain, &[left, right])
}

/// The level above `level` (module documentation).
fn up(domain: &[u8], level: &[[u8; 32]]) -> Vec<[u8; 32]> {
    level
        .chunks(2)
        .map(|p| match p {
            [l, r] => node(domain, l, r),
            [only] => *only,
            _ => unreachable!("chunks of two"),
        })
        .collect()
}

/// The root of `leaves`, or `None` for no leaf.
pub fn root(domain: &[u8], leaves: &[[u8; 32]]) -> Option<[u8; 32]> {
    if leaves.is_empty() {
        return None;
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = up(domain, &level);
    }
    level.first().copied()
}

/// The path of leaf `index` among `leaves`, or `None` if there is no such leaf.
pub fn path(domain: &[u8], leaves: &[[u8; 32]], index: usize) -> Option<Vec<[u8; 32]>> {
    if index >= leaves.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut level = leaves.to_vec();
    let mut at = index;
    while level.len() > 1 {
        let sibling = at ^ 1;
        if sibling < level.len() {
            out.push(level[sibling]);
        }
        level = up(domain, &level);
        at /= 2;
    }
    Some(out)
}

/// Every level of the tree of `leaves`, from the leaves up to the root.
pub fn levels(domain: &[u8], leaves: Vec<[u8; 32]>) -> Vec<Vec<[u8; 32]>> {
    let mut levels = alloc::vec![leaves];
    while levels.last().is_some_and(|l| l.len() > 1) {
        let next = up(domain, levels.last().expect("a level"));
        levels.push(next);
    }
    levels
}

/// Leaf `index`'s path, read from the tree's `levels`.
pub fn path_in(levels: &[Vec<[u8; 32]>], index: usize) -> Vec<[u8; 32]> {
    let mut at = index;
    let mut out = Vec::new();
    for level in &levels[..levels.len().saturating_sub(1)] {
        let sibling = at ^ 1;
        if sibling < level.len() {
            out.push(level[sibling]);
        }
        at /= 2;
    }
    out
}

/// Every leaf's path, in order: the tree built once, each path read from its levels.
pub fn paths(domain: &[u8], leaves: &[[u8; 32]]) -> Vec<Vec<[u8; 32]>> {
    let levels = levels(domain, leaves.to_vec());
    (0..leaves.len()).map(|i| path_in(&levels, i)).collect()
}

/// The root that `leaf`, at `index` of `count` leaves, reaches through `path`; `None` if the
/// index is out of range or the path has too few or too many siblings for that position.
pub fn fold(
    domain: &[u8],
    leaf: [u8; 32],
    index: u64,
    count: u64,
    path: &[[u8; 32]],
) -> Option<[u8; 32]> {
    if index >= count {
        return None;
    }
    let mut node_hash = leaf;
    let (mut at, mut n) = (index, count);
    let mut siblings = path.iter();
    while n > 1 {
        if at % 2 == 1 {
            node_hash = node(domain, siblings.next()?, &node_hash);
        } else if at + 1 < n {
            node_hash = node(domain, &node_hash, siblings.next()?);
        }
        at /= 2;
        n = n.div_ceil(2);
    }
    if siblings.next().is_some() {
        return None;
    }
    Some(node_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &[u8] = b"test/node";

    fn leaves(n: usize) -> Vec<[u8; 32]> {
        (0..n)
            .map(|i| blake2b_256(b"test/leaf", &[&(i as u64).to_be_bytes()]))
            .collect()
    }

    #[test]
    fn every_path_folds_to_the_root_and_nothing_else_does() {
        for n in 1..=33 {
            let l = leaves(n);
            let r = root(D, &l).unwrap();
            let all = paths(D, &l);
            for (i, leaf) in l.iter().enumerate() {
                let p = path(D, &l, i).unwrap();
                assert_eq!(p, all[i], "paths agrees with path");
                assert_eq!(
                    fold(D, *leaf, i as u64, n as u64, &p),
                    Some(r),
                    "n={n} i={i}"
                );
                // Another index, or one sibling more: not the root. (A path does not commit to
                // the count by itself: each user binds the count beside the root.)
                if n > 1 {
                    let j = ((i + 1) % n) as u64;
                    assert_ne!(fold(D, *leaf, j, n as u64, &p), Some(r));
                }
                let mut longer = p.clone();
                longer.push([0; 32]);
                assert_eq!(fold(D, *leaf, i as u64, n as u64, &longer), None);
            }
        }
    }

    #[test]
    fn trees_of_different_sizes_have_different_roots() {
        let all = leaves(40);
        let roots: alloc::collections::BTreeSet<[u8; 32]> =
            (1..=40).map(|n| root(D, &all[..n]).unwrap()).collect();
        assert_eq!(roots.len(), 40);
    }

    #[test]
    fn an_odd_node_is_carried_up_not_duplicated() {
        let l = leaves(3);
        assert_eq!(root(D, &l).unwrap(), node(D, &node(D, &l[0], &l[1]), &l[2]));
        // Three leaves and the same three with the last repeated have different roots.
        let mut four = l.clone();
        four.push(l[2]);
        assert_ne!(root(D, &l), root(D, &four));
        assert_eq!(root(D, &[]), None);
        assert_eq!(root(D, &l[..1]), Some(l[0]));
    }
}
