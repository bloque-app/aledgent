// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The service tree inside the Agent (`F-005`, `F-006` decision 5, `DEC-4`, `REQ-RT-3`).
//!
//! The Agent holds the service tree's root, not its leaves. Each input brings a
//! [`ServiceWitness`]: the service leaves the input reads and writes, as they stand, the kinds it
//! reads whole, and one multiproof of all of them against the root. The Agent
//!
//! - checks that the leaves and the proof give exactly its root, and that a kind claimed whole
//!   has no other leaf (every kind is one subtree of the tree: [`ledger_core::delta::service_key`]);
//! - builds a partial service store from the leaves (`ledger_core::state::State::partial`),
//!   which fails closed on anything else (`ledger_core::leaves`);
//! - runs the input, and computes the root after from the same proof, over every leaf it was
//!   given or created as it stands.
//!
//! Leaves cross as `(kind, canonical key, canonical value)`, and a leaf's hash is its
//! [`ledger_core::delta::library_leaf`]'s. The proof is the frontier of CipherClerk's multiproof
//! (`BatchProof`), as `F-006`'s leaf store computes it; the root is computed here, so that the
//! Agent also sees which frontier node stands where a whole kind claims to have nothing.
//!
//! **Root versions** (`DEC-25`). In a v2 build the proof is
//! [`ServiceWitness::compact`], a `compact_smt::v2::MultiProof` with every node entry opened
//! (the service tree removes leaves, so a node's kind must be bound for the root after), and
//! `frontier` is empty.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use cipher_clerk::merkle::{SMT_DEPTH, build_empty_chain, smt_node_hash};
use ledger_core::delta::{library_key, library_leaf_hash};
use ledger_core::state::LeafEntry;
use ledger_kernel::compact_smt::Version;
use ledger_kernel::compact_smt::v2::{
    self, Entry, FrontierNode, MultiProof, PackedProof, ProofError,
};
use serde::{Deserialize, Serialize};

/// One node of the multiproof's frontier: the hash of an untouched, non-empty subtree beside a
/// touched path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Key bits fixed by the path to the subtree.
    pub level: u16,
    /// Those bits, most significant first.
    pub prefix: [u8; 16],
    /// The subtree's hash.
    pub hash: [u8; 32],
}

/// The service leaves one input reads and writes, and their proof (module documentation).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceWitness {
    /// Every witnessed leaf, as it stands: kind, canonical key, canonical value if present.
    pub leaves: Vec<LeafEntry>,
    /// The kinds witnessed whole: every leaf of each is in `leaves`.
    pub whole: Vec<u8>,
    /// The frontier of the multiproof of every leaf's key and of each whole kind's
    /// [`probe`], sorted by `(level, prefix)`. v1 only.
    pub frontier: Vec<Node>,
    /// The same multiproof in v2, its node entries opened. v2 only.
    pub compact: Option<PackedProof>,
}

/// Why a service witness is refused before anything runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceRefusal {
    /// A leaf given twice, a frontier out of order or repeated, a kind claimed whole twice.
    Shape,
    /// The leaves and the proof do not give the Agent's service root.
    Root,
    /// A kind claimed whole has a leaf the witness does not give.
    NotWhole(u8),
}

/// The key the proof of a whole kind always covers: the first key of the kind's subtree, so the
/// subtree is on a proven path even when the kind has no leaf.
pub fn probe(kind: u8) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[0] = kind;
    k
}

/// The service-tree keys a witness of `leaves` and `whole` proves: every leaf's key and every
/// whole kind's probe, sorted, without repeats. The host builds the frontier over these.
pub fn proven_keys(leaves: &[LeafEntry], whole: &[u8]) -> Vec<[u8; 16]> {
    let keys: BTreeSet<[u8; 16]> = leaves
        .iter()
        .map(|(kind, key, _)| library_key(*kind, key))
        .chain(whole.iter().map(|k| probe(*k)))
        .collect();
    keys.into_iter().collect()
}

type Empty = [[u8; 32]; SMT_DEPTH + 1];

/// The frontier by position: `(level, prefix)` to the subtree's hash.
type Frontier = BTreeMap<(u16, [u8; 16]), [u8; 32]>;

/// The root over `touched` (sorted by key, unique) with `frontier`; counts the frontier nodes
/// used, and flags a non-empty one inside a whole kind's subtree.
struct Rooter<'a> {
    frontier: &'a Frontier,
    whole: &'a BTreeSet<u8>,
    empty: &'a Empty,
    used: usize,
    not_whole: Option<u8>,
}

impl Rooter<'_> {
    fn root(
        &mut self,
        touched: &[([u8; 16], [u8; 32])],
        prefix: [u8; 16],
        level: usize,
    ) -> [u8; 32] {
        let height = SMT_DEPTH - level;
        if touched.is_empty() {
            return match self.frontier.get(&(level as u16, prefix)) {
                Some(h) => {
                    self.used += 1;
                    if level >= 8 && self.whole.contains(&prefix[0]) && *h != self.empty[height] {
                        self.not_whole.get_or_insert(prefix[0]);
                    }
                    *h
                }
                None => self.empty[height],
            };
        }
        if height == 0 {
            return touched[0].1;
        }
        let byte = level / 8;
        let bit = 7 - (level % 8);
        let split = touched.partition_point(|(k, _)| (k[byte] >> bit) & 1 == 0);
        let (left, right) = touched.split_at(split);
        let mut prefix_r = prefix;
        prefix_r[byte] |= 1 << bit;
        let l = self.root(left, prefix, level + 1);
        let r = self.root(right, prefix_r, level + 1);
        smt_node_hash(&l, &r)
    }
}

impl ServiceWitness {
    fn frontier_map(&self) -> Result<Frontier, ServiceRefusal> {
        let mut map = BTreeMap::new();
        for n in &self.frontier {
            if usize::from(n.level) > SMT_DEPTH || map.insert((n.level, n.prefix), n.hash).is_some()
            {
                return Err(ServiceRefusal::Shape);
            }
        }
        Ok(map)
    }

    /// The root the proof gives over `leaves` (the witnessed ones, or the same keys and new ones
    /// in whole kinds, as they stand after a run), with whether every frontier node was used and
    /// which whole kind, if any, has a leaf the witness does not give.
    fn compute(
        &self,
        leaves: &[LeafEntry],
    ) -> Result<([u8; 32], usize, Option<u8>), ServiceRefusal> {
        let frontier = self.frontier_map()?;
        let whole: BTreeSet<u8> = self.whole.iter().copied().collect();
        let mut touched: BTreeMap<[u8; 16], [u8; 32]> = BTreeMap::new();
        for (kind, key, value) in leaves {
            if touched
                .insert(
                    library_key(*kind, key),
                    library_leaf_hash(*kind, key, value.as_deref()),
                )
                .is_some()
            {
                return Err(ServiceRefusal::Shape);
            }
        }
        for k in &whole {
            touched
                .entry(probe(*k))
                .or_insert(cipher_clerk::merkle::SMT_EMPTY_LEAF);
        }
        let touched: Vec<([u8; 16], [u8; 32])> = touched.into_iter().collect();
        if Version::ACTIVE == Version::V2 {
            return self.compute_v2(&touched, &whole);
        }
        let empty = build_empty_chain();
        let mut r = Rooter {
            frontier: &frontier,
            whole: &whole,
            empty: &empty,
            used: 0,
            not_whole: None,
        };
        let root = r.root(&touched, [0; 16], 0);
        Ok((root, r.used, r.not_whole))
    }

    /// [`ServiceWitness::compute`] in v2: the opened multiproof's root, every entry used; an
    /// entry inside a whole kind's subtree is a leaf the witness does not give.
    fn compute_v2(
        &self,
        touched: &[([u8; 16], [u8; 32])],
        whole: &BTreeSet<u8>,
    ) -> Result<([u8; 32], usize, Option<u8>), ServiceRefusal> {
        if !self.frontier.is_empty() {
            return Err(ServiceRefusal::Shape);
        }
        let proof = match &self.compact {
            Some(p) => p.unpack().map_err(|_| ServiceRefusal::Shape)?,
            None => MultiProof::default(),
        };
        let root = proof
            .root_opened(touched)
            .map_err(|_| ServiceRefusal::Shape)?;
        let not_whole = proof
            .frontier
            .iter()
            .find(|f| f.level >= 8 && whole.contains(&f.prefix[0]))
            .map(|f| f.prefix[0]);
        Ok((root, 0, not_whole))
    }

    /// Check the witness against the Agent's service root (module documentation).
    pub fn verify(&self, root: &[u8; 32]) -> Result<(), ServiceRefusal> {
        let whole: BTreeSet<u8> = self.whole.iter().copied().collect();
        if whole.len() != self.whole.len() || whole.contains(&0) {
            return Err(ServiceRefusal::Shape);
        }
        let (got, used, not_whole) = self.compute(&self.leaves)?;
        if used != self.frontier.len() {
            return Err(ServiceRefusal::Shape);
        }
        if got != *root {
            return Err(ServiceRefusal::Root);
        }
        if let Some(k) = not_whole {
            return Err(ServiceRefusal::NotWhole(k));
        }
        Ok(())
    }

    /// The root after a run: the same proof over `leaves`, every leaf the run was given or
    /// created as it stands (`State::witnessed_leaves`). New leaves may only be in whole kinds,
    /// whose subtrees the proof shows empty but for the given leaves.
    pub fn root_after(&self, leaves: &[LeafEntry]) -> Result<[u8; 32], ServiceRefusal> {
        let given: BTreeSet<[u8; 16]> = self
            .leaves
            .iter()
            .map(|(kind, key, _)| library_key(*kind, key))
            .collect();
        let whole: BTreeSet<u8> = self.whole.iter().copied().collect();
        for (kind, key, _) in leaves {
            if !given.contains(&library_key(*kind, key)) && !whole.contains(kind) {
                return Err(ServiceRefusal::Shape);
            }
        }
        Ok(self.compute(leaves)?.0)
    }
}

impl ServiceWitness {
    /// What a host that withholds leaf `index` sends: the leaf taken out, and the subtree it
    /// stood alone in folded into one frontier node, so the proof still gives the same root. The
    /// Agent's store then never shows that leaf, and a read of it fails closed. For tests of the
    /// Agent's refusals; a driver never calls it.
    #[doc(hidden)]
    pub fn conceal(&mut self, index: usize) {
        let (kind, key, value) = self.leaves.remove(index);
        let k = library_key(kind, &key);
        let others = proven_keys(&self.leaves, &self.whole);
        if Version::ACTIVE == Version::V2 {
            return self.conceal_v2(k, library_leaf_hash(kind, &key, value.as_deref()), &others);
        }
        let common = |a: &[u8; 16], b: &[u8; 16]| -> usize {
            (0..SMT_DEPTH)
                .find(|i| (a[i / 8] >> (7 - i % 8)) & 1 != (b[i / 8] >> (7 - i % 8)) & 1)
                .unwrap_or(SMT_DEPTH)
        };
        let level = others
            .iter()
            .map(|o| common(&k, o) + 1)
            .max()
            .unwrap_or(0)
            .min(SMT_DEPTH);
        let mut prefix = [0u8; 16];
        for i in 0..level {
            prefix[i / 8] |= k[i / 8] & (1 << (7 - i % 8));
        }
        let inside = |n: &Node| {
            usize::from(n.level) >= level
                && (0..level).all(|i| {
                    (n.prefix[i / 8] >> (7 - i % 8)) & 1 == (prefix[i / 8] >> (7 - i % 8)) & 1
                })
        };
        let folded: Frontier = self
            .frontier
            .iter()
            .filter(|n| inside(n))
            .map(|n| ((n.level, n.prefix), n.hash))
            .collect();
        let empty = build_empty_chain();
        let whole = BTreeSet::new();
        let mut r = Rooter {
            frontier: &folded,
            whole: &whole,
            empty: &empty,
            used: 0,
            not_whole: None,
        };
        let hash = r.root(
            &[(k, library_leaf_hash(kind, &key, value.as_deref()))],
            prefix,
            level,
        );
        self.frontier.retain(|n| !inside(n));
        self.frontier.push(Node {
            level: level as u16,
            prefix,
            hash,
        });
        self.frontier.sort_by_key(|n| (n.level, n.prefix));
    }
}

impl ServiceWitness {
    /// [`ServiceWitness::conceal`] in v2: the entries in the subtree the leaf stands alone in
    /// (beside every other proven key), with the leaf, become one opened entry.
    fn conceal_v2(&mut self, k: [u8; 16], leaf: [u8; 32], others: &[[u8; 16]]) {
        let level = others
            .iter()
            .map(|o| ledger_kernel::compact_smt::common_prefix(&k, o) + 1)
            .max()
            .unwrap_or(0)
            .min(SMT_DEPTH);
        let prefix = ledger_kernel::compact_smt::mask(&k, level);
        let mut proof = match &self.compact {
            Some(p) => p.unpack().expect("a proof the store built"),
            None => MultiProof::default(),
        };
        let inside = |f: &FrontierNode| {
            usize::from(f.level) >= level
                && ledger_kernel::compact_smt::mask(&f.prefix, level) == prefix
        };
        let folded: Vec<FrontierNode> = proof
            .frontier
            .iter()
            .filter(|f| inside(f))
            .copied()
            .collect();
        let touched = [(k, leaf)];
        let entry = if folded.is_empty() {
            Entry::One { key: k, leaf }
        } else {
            let side = |right: bool| -> Result<[u8; 32], ProofError> {
                let t: Vec<_> = touched
                    .iter()
                    .filter(|(x, _)| ledger_kernel::compact_smt::bit(x, level) == right)
                    .copied()
                    .collect();
                let f: Vec<_> = folded
                    .iter()
                    .filter(|x| ledger_kernel::compact_smt::bit(&x.prefix, level) == right)
                    .copied()
                    .collect();
                Ok(v2::summary_with(&t, &f, level + 1)?.hash())
            };
            Entry::Node {
                left: side(false).expect("a proof the store built"),
                right: side(true).expect("a proof the store built"),
            }
        };
        proof.frontier.retain(|f| !inside(f));
        proof.frontier.push(FrontierNode {
            level: level as u16,
            prefix,
            entry,
        });
        proof.frontier.sort_unstable_by_key(|f| (f.prefix, f.level));
        self.compact = Some((&proof).into());
    }
}

/// The service root of a whole store's leaves: the root of a tree holding exactly them.
pub fn root_of(leaves: &[LeafEntry]) -> [u8; 32] {
    let w = ServiceWitness::default();
    w.compute(leaves).map(|(r, _, _)| r).unwrap_or_else(|_| {
        // Leaves of a whole store never repeat a key; a repeat is a key collision.
        [0xFF; 32]
    })
}

// Held to CipherClerk's v1 tree; `v2_tests` holds a v2 build's witness to its own.
#[cfg(all(test, feature = "smt-v1"))]
mod tests {
    use super::*;
    use alloc::vec;
    use cipher_clerk::merkle::SparseMerkleTree;
    use ledger_core::delta::library_leaf;

    fn entry(kind: u8, n: u8, v: Option<u8>) -> LeafEntry {
        (kind, vec![n; 3], v.map(|v| vec![v; 5]))
    }

    fn put(t: &mut SparseMerkleTree, (kind, key, value): &LeafEntry) {
        match value {
            Some(v) => t.insert(
                library_key(*kind, key),
                &library_leaf(*kind, key, v).leaf_content(),
            ),
            None => t.remove(&library_key(*kind, key)),
        }
    }

    fn tree(leaves: &[LeafEntry]) -> SparseMerkleTree {
        let mut t = SparseMerkleTree::new();
        for l in leaves {
            put(&mut t, l);
        }
        t
    }

    /// The frontier of `keys` over the tree of `all`, as CipherClerk's `BatchProof::build`
    /// defines it: every non-empty subtree with no key of `keys`, beside a path to one.
    fn frontier_of(all: &[LeafEntry], keys: &[[u8; 16]]) -> Vec<Node> {
        let mut full: Vec<([u8; 16], [u8; 32])> = all
            .iter()
            .filter(|l| l.2.is_some())
            .map(|(k, key, v)| {
                (
                    library_key(*k, key),
                    library_leaf_hash(*k, key, v.as_deref()),
                )
            })
            .collect();
        full.sort();
        let empty = build_empty_chain();
        let mut out = Vec::new();
        fn rec(
            keys: &[[u8; 16]],
            full: &[([u8; 16], [u8; 32])],
            prefix: [u8; 16],
            level: usize,
            empty: &Empty,
            out: &mut Vec<Node>,
        ) {
            if keys.is_empty() {
                if !full.is_empty() {
                    let frontier = BTreeMap::new();
                    let whole = BTreeSet::new();
                    let mut r = Rooter {
                        frontier: &frontier,
                        whole: &whole,
                        empty,
                        used: 0,
                        not_whole: None,
                    };
                    out.push(Node {
                        level: level as u16,
                        prefix,
                        hash: r.root(full, prefix, level),
                    });
                }
                return;
            }
            if level == SMT_DEPTH {
                return;
            }
            let (byte, bit) = (level / 8, 7 - (level % 8));
            let side = |k: &[u8; 16]| (k[byte] >> bit) & 1 == 0;
            let ks = keys.partition_point(side);
            let fs = full.partition_point(|(k, _)| side(k));
            let mut prefix_r = prefix;
            prefix_r[byte] |= 1 << bit;
            rec(&keys[..ks], &full[..fs], prefix, level + 1, empty, out);
            rec(&keys[ks..], &full[fs..], prefix_r, level + 1, empty, out);
        }
        rec(keys, &full, [0; 16], 0, &empty, &mut out);
        out.sort_by_key(|n| (n.level, n.prefix));
        out
    }

    #[test]
    fn inv_2_the_service_root_is_cipherclerks_tree_root() {
        let all = vec![
            entry(3, 1, Some(1)),
            entry(3, 2, Some(2)),
            entry(4, 1, Some(3)),
            entry(10, 9, Some(4)),
        ];
        let t = tree(&all);
        assert_eq!(root_of(&all), t.root());
        let given = vec![entry(3, 2, Some(2)), entry(4, 7, None)];
        let w = ServiceWitness {
            leaves: given.clone(),
            whole: Vec::new(),
            frontier: frontier_of(&all, &proven_keys(&given, &[])),
            compact: None,
        };
        w.verify(&t.root()).unwrap();
        // Tampered, stale, or a key the proof does not cover.
        let mut bad = w.clone();
        bad.leaves[0].2 = Some(vec![9; 5]);
        assert_eq!(bad.verify(&t.root()), Err(ServiceRefusal::Root));
        assert_eq!(w.verify(&[1; 32]), Err(ServiceRefusal::Root));
        let after = vec![entry(3, 2, Some(8)), entry(4, 7, Some(1))];
        let mut t2 = t.clone();
        for l in &after {
            put(&mut t2, l);
        }
        assert_eq!(w.root_after(&after).unwrap(), t2.root());
        assert_eq!(
            w.root_after(&[entry(3, 5, Some(1))]),
            Err(ServiceRefusal::Shape)
        );
    }

    #[test]
    fn a_kind_witnessed_whole_has_no_other_leaf() {
        let all = vec![
            entry(3, 1, Some(1)),
            entry(3, 2, Some(2)),
            entry(4, 1, Some(3)),
        ];
        let t = tree(&all);
        let whole = vec![entry(3, 1, Some(1)), entry(3, 2, Some(2))];
        let w = ServiceWitness {
            leaves: whole.clone(),
            whole: vec![3],
            frontier: frontier_of(&all, &proven_keys(&whole, &[3])),
            compact: None,
        };
        w.verify(&t.root()).unwrap();
        // A new leaf in a whole kind: the proof covers it.
        let mut t2 = t.clone();
        put(&mut t2, &entry(3, 7, Some(7)));
        let mut after = whole.clone();
        after.push(entry(3, 7, Some(7)));
        assert_eq!(w.root_after(&after).unwrap(), t2.root());
        // One leaf of the kind withheld.
        let part = vec![entry(3, 1, Some(1))];
        let w = ServiceWitness {
            leaves: part.clone(),
            whole: vec![3],
            frontier: frontier_of(&all, &proven_keys(&part, &[3])),
            compact: None,
        };
        assert_eq!(w.verify(&t.root()), Err(ServiceRefusal::NotWhole(3)));
        // A kind with no leaf at all, whole.
        let w = ServiceWitness {
            leaves: Vec::new(),
            whole: vec![9],
            frontier: frontier_of(&all, &proven_keys(&[], &[9])),
            compact: None,
        };
        w.verify(&t.root()).unwrap();
    }

    #[test]
    fn a_concealed_leaf_still_proves_the_root_and_is_not_given() {
        let all: Vec<LeafEntry> = (0..40u8).map(|n| entry(3 + n % 3, n, Some(n))).collect();
        let t = tree(&all);
        let given: Vec<LeafEntry> = all.iter().step_by(7).cloned().collect();
        let mut w = ServiceWitness {
            leaves: given.clone(),
            whole: Vec::new(),
            frontier: frontier_of(&all, &proven_keys(&given, &[])),
            compact: None,
        };
        w.verify(&t.root()).unwrap();
        let hidden = w.leaves[2].clone();
        w.conceal(2);
        w.verify(&t.root()).unwrap();
        assert!(!w.leaves.contains(&hidden));
        // Concealed from a kind claimed whole, it is caught.
        let kind4: Vec<LeafEntry> = all.iter().filter(|l| l.0 == 4).cloned().collect();
        let mut w = ServiceWitness {
            leaves: kind4.clone(),
            whole: vec![4],
            frontier: frontier_of(&all, &proven_keys(&kind4, &[4])),
            compact: None,
        };
        w.verify(&t.root()).unwrap();
        w.conceal(0);
        assert_eq!(w.verify(&t.root()), Err(ServiceRefusal::NotWhole(4)));
    }
}

/// `DEC-25`: a v2 build's service witness against `compact_smt::v2`'s tree.
#[cfg(all(test, not(feature = "smt-v1")))]
mod v2_tests {
    use super::*;
    use alloc::vec;

    fn entry(kind: u8, n: u8, v: Option<u8>) -> LeafEntry {
        (kind, vec![n; 3], v.map(|v| vec![v; 5]))
    }

    fn sorted(all: &[LeafEntry]) -> Vec<([u8; 16], [u8; 32])> {
        let mut v: Vec<_> = all
            .iter()
            .filter(|l| l.2.is_some())
            .map(|(k, key, v)| {
                (
                    library_key(*k, key),
                    library_leaf_hash(*k, key, v.as_deref()),
                )
            })
            .collect();
        v.sort();
        v
    }

    fn witness(all: &[LeafEntry], given: &[LeafEntry], whole: &[u8]) -> ServiceWitness {
        let keys = proven_keys(given, whole);
        ServiceWitness {
            leaves: given.to_vec(),
            whole: whole.to_vec(),
            frontier: Vec::new(),
            compact: Some((&MultiProof::build(&sorted(all), &keys, true)).into()),
        }
    }

    #[test]
    fn v2_service_witnesses_prove_the_root_and_whole_kinds() {
        let mut all: Vec<LeafEntry> = (0..40u8).map(|n| entry(3 + n % 3, n, Some(n))).collect();
        all.push(entry(9, 1, Some(1)));
        let root = v2::root_sorted(&sorted(&all));
        assert_eq!(root_of(&all), root);
        // Two leaves given, kind 9 whole.
        let given = vec![all[4].clone(), all[9].clone(), all[40].clone()];
        let w = witness(&all, &given, &[9]);
        assert_eq!(w.verify(&root), Ok(()));
        // After: one leaf changed, one removed, one new in the whole kind.
        let after = vec![
            entry(all[4].0, 4, Some(99)),
            (all[9].0, all[9].1.clone(), None),
            all[40].clone(),
            entry(9, 2, Some(7)),
        ];
        let mut post: Vec<LeafEntry> = all.clone();
        post[4] = after[0].clone();
        post[9] = after[1].clone();
        post.push(after[3].clone());
        assert_eq!(w.root_after(&after), Ok(v2::root_sorted(&sorted(&post))));
        // Kind 4 claimed whole while a leaf of it is not given.
        let w = witness(&all, &[all[4].clone()], &[all[4].0]);
        assert_eq!(w.verify(&root), Err(ServiceRefusal::NotWhole(all[4].0)));
        // A concealed leaf still proves the root, and is not given.
        let mut w = witness(&all, &given, &[9]);
        w.conceal(1);
        assert_eq!(w.verify(&root), Ok(()));
        assert_eq!(w.leaves.len(), 2);
        // A tampered entry gives another root.
        let mut w = witness(&all, &given, &[9]);
        let mut p = w.compact.as_ref().unwrap().unpack().unwrap();
        if let Some(f) = p.frontier.first_mut() {
            f.entry = Entry::Node {
                left: [1; 32],
                right: [2; 32],
            };
        }
        w.compact = Some((&p).into());
        assert!(w.verify(&root).is_err());
    }
}
