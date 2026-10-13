// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The v2 tree against an independent reference, and its proofs (`DEC-25`).
//!
//! The reference builds the tree bottom-up over all 128 levels, as the definition reads: every
//! leaf a shortcut at depth 128, each level's nodes combined from their two children, a missing
//! child empty. The implementation under test recurses top-down and stops where a subtree has
//! one leaf; nothing is shared between the two but the two hash functions.
#![allow(missing_docs)]

use std::collections::BTreeMap;

use cipher_clerk::crypto::blake2b_256;
use cipher_clerk::merkle::SMT_EMPTY_LEAF;
use compact_smt::v2::{
    self, Entry, Membership, MultiProof, ProofError, Summary, leaf_node, node, prove, root_sorted,
};
use compact_smt::{Version, mask};
use proptest::prelude::*;

fn key(i: u64) -> [u8; 16] {
    let h = blake2b_256(b"compact-smt-test-key", &[&i.to_le_bytes()]);
    let mut k = [0u8; 16];
    k.copy_from_slice(&h[..16]);
    k
}

fn leaf(i: u64) -> [u8; 32] {
    blake2b_256(b"compact-smt-test-leaf", &[&i.to_le_bytes()])
}

/// The definition, bottom-up over every level.
fn reference_root(set: &BTreeMap<[u8; 16], [u8; 32]>) -> [u8; 32] {
    let mut level: BTreeMap<[u8; 16], Summary> = set
        .iter()
        .filter(|(_, h)| **h != SMT_EMPTY_LEAF)
        .map(|(k, h)| (*k, Summary::One(leaf_node(k, h))))
        .collect();
    for depth in (0..128).rev() {
        let mut up: BTreeMap<[u8; 16], (Summary, Summary)> = BTreeMap::new();
        for (prefix, s) in level {
            let parent = mask(&prefix, depth);
            let slot = up.entry(parent).or_insert((Summary::Empty, Summary::Empty));
            if compact_smt::bit(&prefix, depth) {
                slot.1 = s;
            } else {
                slot.0 = s;
            }
        }
        level = up
            .into_iter()
            .map(|(p, (l, r))| {
                let s = match (l, r) {
                    (Summary::Empty, Summary::Empty) => Summary::Empty,
                    (Summary::Empty, Summary::One(h)) | (Summary::One(h), Summary::Empty) => {
                        Summary::One(h)
                    }
                    (l, r) => Summary::Many(node(&l.hash(), &r.hash())),
                };
                (p, s)
            })
            .collect();
    }
    level.get(&[0u8; 16]).map_or(v2::EMPTY, |s| s.hash())
}

fn sorted(set: &BTreeMap<[u8; 16], [u8; 32]>) -> Vec<([u8; 16], [u8; 32])> {
    set.iter().map(|(k, h)| (*k, *h)).collect()
}

/// Keys from seeds, some sharing long prefixes with an earlier key (deep branches, long chains).
fn keys_of(seeds: &[u64], close: &[(u8, usize)]) -> Vec<[u8; 16]> {
    let mut keys: Vec<[u8; 16]> = seeds.iter().map(|s| key(*s)).collect();
    for (i, (b, pos)) in close.iter().enumerate() {
        let mut k = keys[i % keys.len()];
        k[*pos] ^= b | 1;
        keys.push(k);
    }
    keys
}

#[test]
fn small_trees_by_hand() {
    // Empty, one leaf, two leaves that part at bit 0, two that share 127 bits.
    assert_eq!(root_sorted(&[]), v2::EMPTY);
    let a = ([0u8; 16], leaf(1));
    assert_eq!(root_sorted(&[a]), leaf_node(&a.0, &a.1));
    let b = ([0x80u8; 16], leaf(2));
    assert_eq!(
        root_sorted(&[a, b]),
        node(&leaf_node(&a.0, &a.1), &leaf_node(&b.0, &b.1))
    );
    let mut c0 = [0u8; 16];
    c0[15] = 1;
    let c = (c0, leaf(3));
    let mut chain = node(&leaf_node(&a.0, &a.1), &leaf_node(&c.0, &c.1));
    for d in (0..127).rev() {
        let _ = d;
        chain = node(&chain, &v2::EMPTY);
    }
    assert_eq!(root_sorted(&[a, c]), chain);
    // A removed leaf is absent.
    let gone = ([0x80u8; 16], SMT_EMPTY_LEAF);
    assert_eq!(root_sorted(&[a, gone]), root_sorted(&[a]));
    assert_eq!(Version::V2.root_of_sorted(&[a, b]), root_sorted(&[a, b]));
}

#[test]
fn a_thousand_leaves_match_the_reference() {
    let set: BTreeMap<_, _> = (0..1000).map(|i| (key(i), leaf(i))).collect();
    assert_eq!(root_sorted(&sorted(&set)), reference_root(&set));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: None, .. ProptestConfig::default() })]

    /// The root is the reference's, for any set, and a function of the set alone: the same
    /// leaves written in any order (with overwrites) give the same root.
    #[test]
    fn roots_match_the_reference_whatever_the_order(
        seeds in proptest::collection::vec(any::<u64>(), 1..80),
        close in proptest::collection::vec((any::<u8>(), 0usize..16), 0..20),
        order in any::<u64>(),
        removed in proptest::collection::vec(any::<prop::sample::Index>(), 0..5),
    ) {
        let keys = keys_of(&seeds, &close);
        let mut set = BTreeMap::new();
        for (i, k) in keys.iter().enumerate() {
            set.insert(*k, leaf(i as u64));
        }
        for r in &removed {
            let k = keys[r.index(keys.len())];
            set.insert(k, SMT_EMPTY_LEAF);
        }
        let root = root_sorted(&sorted(&set));
        prop_assert_eq!(root, reference_root(&set));
        // Another order of writes: shuffle, write twice (an overwrite with a stale value first).
        let mut shuffled: Vec<_> = set.iter().map(|(k, h)| (*k, *h)).collect();
        let n = shuffled.len();
        for i in 0..n {
            let j = (order.wrapping_mul(i as u64 + 7).rotate_left(i as u32) % n as u64) as usize;
            shuffled.swap(i, j);
        }
        let mut again = BTreeMap::new();
        for (k, _) in &shuffled {
            again.insert(*k, leaf(u64::MAX));
        }
        for (k, h) in &shuffled {
            again.insert(*k, *h);
        }
        prop_assert_eq!(root_sorted(&sorted(&again)), root);
    }

    /// Inclusion proofs verify for every present key, non-inclusion proofs for absent ones (an
    /// empty position or another leaf on the path), and a tampered proof proves nothing.
    #[test]
    fn inclusion_and_exclusion_proofs_verify(
        seeds in proptest::collection::vec(any::<u64>(), 0..60),
        close in proptest::collection::vec((any::<u8>(), 0usize..16), 0..10),
        probes in proptest::collection::vec(any::<u64>(), 1..8),
    ) {
        let keys = if seeds.is_empty() { Vec::new() } else { keys_of(&seeds, &close) };
        let set: BTreeMap<_, _> = keys.iter().enumerate().map(|(i, k)| (*k, leaf(i as u64))).collect();
        let leaves = sorted(&set);
        let root = root_sorted(&leaves);
        for k in keys.iter().take(10) {
            let p = prove(&leaves, k);
            prop_assert_eq!(p.verify(&root, k), Some(Membership::Present(set[k])));
            // Claiming another leaf hash for the key fails.
            let mut forged = p.clone();
            forged.terminal = Some((*k, leaf(u64::MAX)));
            prop_assert_eq!(forged.verify(&root, k), None);
            // A tampered sibling fails.
            if let Some(s) = p.siblings.first() {
                let mut t = p.clone();
                t.siblings[0] = blake2b_256(b"tamper", &[s]);
                prop_assert_eq!(t.verify(&root, k), None);
            }
        }
        for p in &probes {
            let k = key(p ^ 0x5eed);
            if set.contains_key(&k) {
                continue;
            }
            let proof = prove(&leaves, &k);
            prop_assert_eq!(proof.verify(&root, &k), Some(Membership::Absent));
            // The same proof does not show any present key absent.
            if let Some((other, h)) = proof.terminal {
                prop_assert_eq!(proof.verify(&root, &other), Some(Membership::Present(h)));
            }
        }
    }

    /// A multiproof gives the root before, and after any update of its touched leaves; opened,
    /// it also gives the root after removals.
    #[test]
    fn multiproofs_give_the_roots_before_and_after(
        seeds in proptest::collection::vec(any::<u64>(), 0..80),
        close in proptest::collection::vec((any::<u8>(), 0usize..16), 0..10),
        touch in proptest::collection::vec(any::<prop::sample::Index>(), 0..12),
        fresh in proptest::collection::vec(any::<u64>(), 0..6),
        remove in any::<bool>(),
    ) {
        let keys = if seeds.is_empty() { Vec::new() } else { keys_of(&seeds, &close) };
        let set: BTreeMap<_, _> = keys.iter().enumerate().map(|(i, k)| (*k, leaf(i as u64))).collect();
        let leaves = sorted(&set);
        let mut touched: Vec<[u8; 16]> = touch
            .iter()
            .filter(|_| !keys.is_empty())
            .map(|i| keys[i.index(keys.len())])
            .collect();
        touched.extend(fresh.iter().map(|f| key(f ^ 0xF4E5)));
        touched.sort_unstable();
        touched.dedup();
        let before: Vec<_> = touched.iter().map(|k| (*k, *set.get(k).unwrap_or(&SMT_EMPTY_LEAF))).collect();
        for open in [false, true] {
            let proof = MultiProof::build(&leaves, &touched, open);
            prop_assert_eq!(proof.root(&before), Ok(root_sorted(&leaves)));
            if open {
                prop_assert_eq!(proof.root_opened(&before), Ok(root_sorted(&leaves)));
            } else if proof.frontier.iter().any(|f| matches!(f.entry, Entry::Many { .. })) {
                prop_assert_eq!(proof.root_opened(&before), Err(ProofError::Opaque));
            }
            // Insert and overwrite every touched key.
            let after: Vec<_> = touched.iter().enumerate().map(|(i, k)| (*k, leaf(10_000 + i as u64))).collect();
            let mut post = set.clone();
            post.extend(after.iter().copied());
            prop_assert_eq!(proof.root(&after), Ok(root_sorted(&sorted(&post))));
            // Remove every touched key: sound with opened entries.
            if remove && open {
                let gone: Vec<_> = touched.iter().map(|k| (*k, SMT_EMPTY_LEAF)).collect();
                let mut post = set.clone();
                for k in &touched {
                    post.remove(k);
                }
                prop_assert_eq!(proof.root_opened(&gone), Ok(root_sorted(&sorted(&post))));
            }
        }
    }

    /// A frontier that lies does not give the root: a changed hash, a dropped or an extra entry,
    /// a leaf moved outside its prefix.
    #[test]
    fn tampered_multiproofs_are_refused(
        seeds in proptest::collection::vec(any::<u64>(), 8..60),
        touch in proptest::collection::vec(any::<prop::sample::Index>(), 1..6),
        pick in any::<prop::sample::Index>(),
    ) {
        let keys: Vec<_> = seeds.iter().map(|s| key(*s)).collect();
        let set: BTreeMap<_, _> = keys.iter().enumerate().map(|(i, k)| (*k, leaf(i as u64))).collect();
        let leaves = sorted(&set);
        let root = root_sorted(&leaves);
        let mut touched: Vec<_> = touch.iter().map(|i| keys[i.index(keys.len())]).collect();
        touched.sort_unstable();
        touched.dedup();
        let before: Vec<_> = touched.iter().map(|k| (*k, set[k])).collect();
        let proof = MultiProof::build(&leaves, &touched, false);
        prop_assume!(!proof.frontier.is_empty());
        let i = pick.index(proof.frontier.len());
        // A changed entry.
        let mut t = proof.clone();
        t.frontier[i].entry = match t.frontier[i].entry {
            Entry::One { key, .. } => Entry::One { key, leaf: leaf(u64::MAX) },
            Entry::Many { hash } => Entry::Many { hash: blake2b_256(b"tamper", &[&hash]) },
            Entry::Node { left, right } => Entry::Node { left: right, right: left },
        };
        prop_assert_ne!(t.root(&before), Ok(root));
        // A dropped entry.
        let mut t = proof.clone();
        t.frontier.remove(i);
        prop_assert_ne!(t.root(&before), Ok(root));
        if let Entry::One { key, leaf } = proof.frontier[i].entry {
            // Moved outside its prefix: refused outright.
            let mut t = proof.clone();
            let mut other = key;
            other[0] ^= 0x80;
            if proof.frontier[i].level > 0 {
                t.frontier[i].entry = Entry::One { key: other, leaf };
                prop_assert_eq!(t.root(&before), Err(ProofError::Shape));
            }
        }
        // An extra entry on a touched path.
        let mut t = proof.clone();
        t.frontier.push(v2::FrontierNode { level: 0, prefix: [0; 16], entry: Entry::Many { hash: root } });
        t.frontier.sort_unstable_by_key(|f| (f.prefix, f.level));
        prop_assert!(t.root(&before).is_err());
    }
}

/// An opaque node entry is bound by the root before when the touched subtree beside it is
/// empty, and not when a transition empties it: why a tree with removals carries opened entries.
#[test]
fn opaque_entries_are_bound_only_where_the_touched_side_is_empty() {
    let u = ([0x80u8; 16], leaf(1));
    let k = [0u8; 16];
    let honest = MultiProof::build(&[u], &[k], false);
    assert_eq!(
        honest.frontier,
        vec![v2::FrontierNode {
            level: 1,
            prefix: [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            entry: Entry::One {
                key: u.0,
                leaf: u.1
            }
        }]
    );
    let mut lie = honest.clone();
    lie.frontier[0].entry = Entry::Many {
        hash: leaf_node(&u.0, &u.1),
    };
    // k absent: the lie changes the root before, so it is refused.
    let absent = [(k, SMT_EMPTY_LEAF)];
    assert_eq!(honest.root(&absent), Ok(root_sorted(&[u])));
    assert_ne!(lie.root(&absent), Ok(root_sorted(&[u])));
    // k present before: the lie gives the right root before ...
    let present = [(k, leaf(2))];
    let both = [(k, leaf(2)), u];
    assert_eq!(lie.root(&present), Ok(root_sorted(&both)));
    // ... and a wrong root after k's removal. Opened entries refuse it.
    assert_ne!(lie.root(&absent), Ok(root_sorted(&[u])));
    assert_eq!(lie.root_opened(&present), Err(ProofError::Opaque));
}

/// The count behind `proposals/cipherclerk/0006`'s table: node hashes to recompute the root
/// after touching 8 random keys, v1 against v2.
#[test]
fn v2_hashes_a_fraction_of_v1s_paths() {
    let n = 1000u64;
    let set: BTreeMap<_, _> = (0..n).map(|i| (key(i), leaf(i))).collect();
    let leaves = sorted(&set);
    let touched: Vec<_> = (0..8).map(|i| key(i * 97)).collect();
    let proof = MultiProof::build(&leaves, &touched, false);
    let mut t: Vec<_> = touched.iter().map(|k| (*k, set[k])).collect();
    t.sort_unstable();
    // v2: one hash a combine of two non-empty sides, one a leaf node; bounded above by the
    // count of levels the recursion visits with a non-empty side.
    let mut hashes = 0usize;
    fn count(
        t: &[([u8; 16], [u8; 32])],
        f: &[v2::FrontierNode],
        level: usize,
        n: &mut usize,
    ) -> bool {
        if t.is_empty() {
            return !f.is_empty();
        }
        if t.len() == 1 && f.is_empty() {
            *n += 1;
            return true;
        }
        let ts = t.partition_point(|(k, _)| !compact_smt::bit(k, level));
        let fs = f.partition_point(|x| !compact_smt::bit(&x.prefix, level));
        let l = count(&t[..ts], &f[..fs], level + 1, n);
        let r = count(&t[ts..], &f[fs..], level + 1, n);
        if l && r {
            *n += 1;
        }
        l || r
    }
    count(&t, &proof.frontier, 0, &mut hashes);
    assert!(
        hashes < 100,
        "v2 recomputes {hashes} hashes for 8 keys in 1 000 leaves"
    );
    assert_eq!(proof.root(&t), Ok(root_sorted(&leaves)));
}

/// The wire form round-trips and costs about v1's 50 bytes an entry, not the enum's 84.
#[test]
fn packed_proofs_round_trip_at_v1s_size() {
    let set: BTreeMap<_, _> = (0..500).map(|i| (key(i), leaf(i))).collect();
    let leaves = sorted(&set);
    let touched: Vec<_> = (0..16).map(|i| key(i * 31)).collect();
    for open in [false, true] {
        let proof = MultiProof::build(&leaves, &touched, open);
        let packed = v2::PackedProof::from(&proof);
        assert_eq!(packed.unpack(), Ok(proof.clone()));
        let packed_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&packed)
            .unwrap()
            .len();
        let logical_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&proof).unwrap().len();
        let n = proof.frontier.len();
        assert!(
            packed_bytes < logical_bytes,
            "{packed_bytes} against {logical_bytes}"
        );
        if !open {
            assert!(
                packed_bytes <= 52 * n + 32,
                "{packed_bytes} B for {n} entries"
            );
        }
    }
    let mut bad = v2::PackedProof::from(&MultiProof::build(&leaves, &touched, true));
    bad.kinds.push(7);
    assert_eq!(bad.unpack(), Err(ProofError::Shape));
}
