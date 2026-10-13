// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The seed's commitment to the pre-cutover history (`F-036`, owner ruling
//! `seed-history-commitment`, 2026-10-08).
//!
//! History is **not replayed** (`REQ-RC-4`) and is **not** among the seed's records. The seed's
//! header carries a [`HistoryCommit`]: a Merkle root over one leaf per legacy transaction record of
//! the frozen bundle, the leaf count, the bundle's own digest and the scheme's version. The book
//! keeps the commit as genesis's seed delivered it and never reads history. Anyone holding a
//! record and its [`HistoryProof`] checks it against the commit ([`verify()`]); the commit is in turn
//! bound to the book's genesis through the seed digest (`crate::chainspec::verify_header`).
//!
//! **Leaf.** One record is one line of the bundle's canonical JSON Lines (rows sorted by primary key, keys by name, amounts as decimal strings, UTF-8), without its
//! line feed, tagged with its table ([`HistoryTable`]):
//! `leaf = blake2b-256("aledgent/history/leaf/v1" ‖ table code ‖ line)`. The table code is one
//! byte, so no two (table, line) pairs share an input.
//!
//! **Tree.** The leaves are sorted by their hash, ascending, so the root does not depend on the
//! order the files were read in; two equal leaves are refused (a record twice is a defect of the
//! bundle, not something to hide). Nodes are `blake2b-256("aledgent/history/node/v1" ‖ left ‖
//! right)`, and an odd node is carried up unchanged (`crate::chainspec::merkle`). The leaf and node
//! domains differ, so a node can never pass for a leaf.
//!
//! **No history.** Zero leaves have the root [`empty_root`] =
//! `blake2b-256("aledgent/history/empty/v1")`, and no record verifies against it.
//!
//! Only the root and the count reach the book's inputs: no record, so no personal data, ever
//! crosses into a replayed input (`INV-17`, `REQ-RT-3`).

use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

use super::merkle;
use crate::export::canon::{Canonical, Dec, DecodeError, Enc};

/// The scheme this module implements.
pub const HISTORY_SCHEME: u16 = 1;

const LEAF_DOMAIN: &[u8] = b"aledgent/history/leaf/v1";
const NODE_DOMAIN: &[u8] = b"aledgent/history/node/v1";
const EMPTY_DOMAIN: &[u8] = b"aledgent/history/empty/v1";

/// The root of no history: `blake2b-256("aledgent/history/empty/v1")`.
pub fn empty_root() -> [u8; 32] {
    blake2b_256(EMPTY_DOMAIN, &[])
}

/// The bundle table a record comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum HistoryTable {
    /// A transaction of the prior ledger (`transactions`, per ledger).
    FormanceTransaction,
    /// A settlement record of the prior system.
    SettlementRecord,
}

impl HistoryTable {
    /// Its one-byte code in a leaf. Stable.
    pub const fn code(self) -> u8 {
        match self {
            HistoryTable::FormanceTransaction => 1,
            HistoryTable::SettlementRecord => 2,
        }
    }

    /// The table of a code.
    pub fn from_code(c: u8) -> Option<HistoryTable> {
        match c {
            1 => Some(HistoryTable::FormanceTransaction),
            2 => Some(HistoryTable::SettlementRecord),
            _ => None,
        }
    }
}

/// One legacy record: its table and its canonical line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRecord {
    /// Its table.
    pub table: HistoryTable,
    /// The line, as the bundle holds it, without the line feed.
    pub line: Vec<u8>,
}

impl HistoryRecord {
    /// Its leaf (module documentation).
    pub fn leaf(&self) -> [u8; 32] {
        blake2b_256(LEAF_DOMAIN, &[&[self.table.code()], &self.line])
    }
}

/// What the seed's header commits about history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HistoryCommit {
    /// The tree's root, or [`empty_root`] for no record.
    pub root: [u8; 32],
    /// How many leaves.
    pub leaf_count: u64,
    /// The bundle's digest: the SHA-256 of its signed manifest, as
    /// published.
    pub bundle_digest: [u8; 32],
    /// [`HISTORY_SCHEME`].
    pub scheme_version: u16,
}

/// A record's inclusion proof.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HistoryProof {
    /// [`HISTORY_SCHEME`].
    pub scheme_version: u16,
    /// The leaf's position among the sorted leaves.
    pub index: u64,
    /// How many leaves the tree has.
    pub leaf_count: u64,
    /// The siblings, from the leaf up.
    pub siblings: Vec<[u8; 32]>,
}

/// Why history did not build or a proof did not verify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryError {
    /// Two records have the same leaf.
    Duplicate,
    /// The proof or the commit is of another scheme.
    Scheme,
    /// The proof's count is not the commit's.
    Count,
    /// The record does not reach the committed root through the proof.
    NotIncluded,
}

/// The history tree of a set of records.
#[derive(Clone, Debug)]
pub struct HistoryTree {
    leaves: Vec<[u8; 32]>,
}

impl HistoryTree {
    /// The tree of `records`, in any order.
    pub fn new<'a>(
        records: impl IntoIterator<Item = &'a HistoryRecord>,
    ) -> Result<Self, HistoryError> {
        let mut leaves: Vec<[u8; 32]> = records.into_iter().map(HistoryRecord::leaf).collect();
        leaves.sort_unstable();
        if leaves.windows(2).any(|w| w[0] == w[1]) {
            return Err(HistoryError::Duplicate);
        }
        Ok(HistoryTree { leaves })
    }

    /// The tree of leaves already computed (the tool streams large files), in any order.
    pub fn from_leaves(mut leaves: Vec<[u8; 32]>) -> Result<Self, HistoryError> {
        leaves.sort_unstable();
        if leaves.windows(2).any(|w| w[0] == w[1]) {
            return Err(HistoryError::Duplicate);
        }
        Ok(HistoryTree { leaves })
    }

    /// Its root ([`empty_root`] for no record).
    pub fn root(&self) -> [u8; 32] {
        merkle::root(NODE_DOMAIN, &self.leaves).unwrap_or_else(empty_root)
    }

    /// How many leaves.
    pub fn len(&self) -> u64 {
        self.leaves.len() as u64
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// The commit of this tree, for the bundle whose digest is `bundle_digest`.
    pub fn commit(&self, bundle_digest: [u8; 32]) -> HistoryCommit {
        HistoryCommit {
            root: self.root(),
            leaf_count: self.len(),
            bundle_digest,
            scheme_version: HISTORY_SCHEME,
        }
    }

    /// The proof of `record`, if the tree holds it.
    pub fn prove(&self, record: &HistoryRecord) -> Option<HistoryProof> {
        let leaf = record.leaf();
        let index = self.leaves.binary_search(&leaf).ok()?;
        Some(HistoryProof {
            scheme_version: HISTORY_SCHEME,
            index: index as u64,
            leaf_count: self.len(),
            siblings: merkle::path(NODE_DOMAIN, &self.leaves, index)?,
        })
    }
}

/// Check `record` against `commit` through `proof` (module documentation).
pub fn verify(
    record: &HistoryRecord,
    proof: &HistoryProof,
    commit: &HistoryCommit,
) -> Result<(), HistoryError> {
    if proof.scheme_version != HISTORY_SCHEME || commit.scheme_version != HISTORY_SCHEME {
        return Err(HistoryError::Scheme);
    }
    if proof.leaf_count != commit.leaf_count {
        return Err(HistoryError::Count);
    }
    match merkle::fold(
        NODE_DOMAIN,
        record.leaf(),
        proof.index,
        proof.leaf_count,
        &proof.siblings,
    ) {
        Some(root) if root == commit.root => Ok(()),
        _ => Err(HistoryError::NotIncluded),
    }
}

impl Canonical for HistoryCommit {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.root)
            .put(&self.leaf_count)
            .put(&self.bundle_digest)
            .put(&self.scheme_version);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(HistoryCommit {
            root: d.get()?,
            leaf_count: d.get()?,
            bundle_digest: d.get()?,
            scheme_version: d.get()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(table: HistoryTable, n: u32) -> HistoryRecord {
        HistoryRecord {
            table,
            line: alloc::format!("{{\"amount\":\"{n}.00\",\"id\":{n}}}").into_bytes(),
        }
    }

    fn records(n: u32) -> Vec<HistoryRecord> {
        (0..n)
            .map(|i| {
                let t = if i % 3 == 0 {
                    HistoryTable::SettlementRecord
                } else {
                    HistoryTable::FormanceTransaction
                };
                rec(t, i)
            })
            .collect()
    }

    #[test]
    fn the_root_is_deterministic_whatever_the_order_read() {
        let a = records(50);
        let mut b = a.clone();
        b.reverse();
        let ta = HistoryTree::new(&a).unwrap();
        let tb = HistoryTree::new(&b).unwrap();
        assert_eq!(ta.root(), tb.root());
        assert_eq!(ta.commit([1; 32]), tb.commit([1; 32]));
        // One record more, or one changed, changes it.
        let mut c = a.clone();
        c.push(rec(HistoryTable::FormanceTransaction, 999));
        assert_ne!(HistoryTree::new(&c).unwrap().root(), ta.root());
        // The table is part of the leaf.
        let x = rec(HistoryTable::FormanceTransaction, 1);
        let y = rec(HistoryTable::SettlementRecord, 1);
        assert_ne!(x.leaf(), y.leaf());
    }

    #[test]
    fn every_record_proves_and_a_tampered_one_does_not() {
        let rs = records(37);
        let t = HistoryTree::new(&rs).unwrap();
        let commit = t.commit([9; 32]);
        for r in &rs {
            let p = t.prove(r).unwrap();
            assert_eq!(verify(r, &p, &commit), Ok(()));
        }
        let r = &rs[5];
        let p = t.prove(r).unwrap();
        // A tampered record.
        let mut bad = r.clone();
        bad.line[2] ^= 1;
        assert_eq!(verify(&bad, &p, &commit), Err(HistoryError::NotIncluded));
        // The same line under the other table.
        let other = HistoryRecord {
            table: match r.table {
                HistoryTable::FormanceTransaction => HistoryTable::SettlementRecord,
                HistoryTable::SettlementRecord => HistoryTable::FormanceTransaction,
            },
            line: r.line.clone(),
        };
        assert_eq!(verify(&other, &p, &commit), Err(HistoryError::NotIncluded));
        // A wrong index.
        let mut wrong = p.clone();
        wrong.index = (p.index + 1) % p.leaf_count;
        assert_eq!(verify(r, &wrong, &commit), Err(HistoryError::NotIncluded));
        // An index out of range, a count not the commit's, another scheme.
        let mut out = p.clone();
        out.index = p.leaf_count;
        assert_eq!(verify(r, &out, &commit), Err(HistoryError::NotIncluded));
        let mut count = p.clone();
        count.leaf_count += 1;
        assert_eq!(verify(r, &count, &commit), Err(HistoryError::Count));
        let mut scheme = p;
        scheme.scheme_version = 2;
        assert_eq!(verify(r, &scheme, &commit), Err(HistoryError::Scheme));
        // A record the tree does not hold has no proof.
        assert!(
            t.prove(&rec(HistoryTable::SettlementRecord, 4_000))
                .is_none()
        );
    }

    #[test]
    fn no_history_has_the_empty_root_and_nothing_verifies_against_it() {
        let t = HistoryTree::new(&[]).unwrap();
        assert!(t.is_empty());
        assert_eq!(t.root(), blake2b_256(b"aledgent/history/empty/v1", &[]));
        let commit = t.commit([0; 32]);
        assert_eq!(commit.leaf_count, 0);
        let r = rec(HistoryTable::FormanceTransaction, 1);
        let p = HistoryProof {
            scheme_version: HISTORY_SCHEME,
            index: 0,
            leaf_count: 0,
            siblings: Vec::new(),
        };
        assert_eq!(verify(&r, &p, &commit), Err(HistoryError::NotIncluded));
        // One record's root is its leaf, and is not the empty root.
        let one = HistoryTree::new(core::slice::from_ref(&r)).unwrap();
        assert_eq!(one.root(), r.leaf());
        assert_ne!(one.root(), t.root());
    }

    #[test]
    fn a_record_twice_is_refused() {
        let r = rec(HistoryTable::FormanceTransaction, 1);
        assert_eq!(
            HistoryTree::new(&[r.clone(), r]).unwrap_err(),
            HistoryError::Duplicate
        );
    }
}
