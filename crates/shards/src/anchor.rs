// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! One anchor over every shard's root, at one consistent cut (`INV-19`, `REQ-PR-1`, `REQ-RT-14`,
//! `DEC-22`, `DEC-23`): the root of roots.
//!
//! The assembler ([`crate::cut::Assembler`]) picks one record per shard and the registry's at a
//! consistent cut; the anchor commits to all of them: a binary Merkle tree whose leaves are, in
//! shard order, `blake2b-256("aledgent/anchor/shard/v3" ‖ shard ‖ seq ‖ commit ‖ root ‖ heads ‖
//! construction ‖ code ‖ authority)` (the shard's audit sequence, kernel commit and root, the
//! digest of its stream heads, and, since `v3` (`T-037-03`, `DEC-26` (a)), the construction of its
//! state commitment, its code version and its upgrade-authority mode's digest), then
//! `blake2b-256("aledgent/anchor/registry/v1" ‖ seq ‖ version ‖ digest)`, padded with the zero
//! hash to a power of two, nodes `blake2b-256("aledgent/anchor/node/v1" ‖ left ‖ right)`. A
//! disclosure proves a fact against its shard's root (CipherClerk's tree) and that root against
//! the anchor ([`Proof`]); the heads in the leaf let a verifier check that the cut was consistent.
//! One book, one sequence of anchors (`INV-25`).

use alloc::vec::Vec;
use cipher_clerk::crypto::blake2b_256;
use serde::{Deserialize, Serialize};

use crate::model::ShardId;

const SHARD: &[u8] = b"aledgent/anchor/shard/v3";
const REGISTRY: &[u8] = b"aledgent/anchor/registry/v1";
const NODE: &[u8] = b"aledgent/anchor/node/v1";

/// One shard's committed state at the cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardRoot {
    /// The shard.
    pub shard: ShardId,
    /// Its audit sequence at the cut: the trail holds it up to here (`INV-19`).
    pub seq: u64,
    /// The kernel commit the root is of.
    pub commit: u64,
    /// The kernel's composite root.
    pub root: [u8; 32],
    /// The digest of its stream heads at that commit ([`crate::cut::Heads::digest`]).
    pub heads: [u8; 32],
    /// The construction of its state commitment (`REQ-PR-10`, `T-037-03`).
    pub construction: u8,
    /// Its code version (`REQ-RT-15`, `T-037-03`).
    pub code: [u8; 32],
    /// The digest of its upgrade-authority mode (`REQ-RT-18`, `T-037-03`).
    pub authority: [u8; 32],
}

/// The registry's state at the cut: the principals Agent's version, and the digest of its feed
/// through that version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRoot {
    /// The principals Agent's audit sequence (its invocations committed).
    pub seq: u64,
    /// The registry's version.
    pub version: u64,
    /// The running digest of its feed through `version`.
    pub digest: [u8; 32],
}

/// One leaf of an anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Leaf {
    /// A shard's root.
    Shard(ShardRoot),
    /// The registry's.
    Registry(RegistryRoot),
}

fn leaf(l: &Leaf) -> [u8; 32] {
    match l {
        Leaf::Shard(r) => blake2b_256(
            SHARD,
            &[
                &r.shard.0.to_be_bytes(),
                &r.seq.to_be_bytes(),
                &r.commit.to_be_bytes(),
                &r.root,
                &r.heads,
                &[r.construction],
                &r.code,
                &r.authority,
            ],
        ),
        Leaf::Registry(r) => blake2b_256(
            REGISTRY,
            &[&r.seq.to_be_bytes(), &r.version.to_be_bytes(), &r.digest],
        ),
    }
}

fn node(l: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    blake2b_256(NODE, &[l, r])
}

fn levels(leaves: &[Leaf]) -> Vec<Vec<[u8; 32]>> {
    let mut level: Vec<[u8; 32]> = leaves.iter().map(leaf).collect();
    let width = level.len().max(1).next_power_of_two();
    level.resize(width, [0; 32]);
    let mut out = alloc::vec![level];
    while out.last().expect("one level").len() > 1 {
        let prev = out.last().expect("one level");
        let next = prev.chunks(2).map(|p| node(&p[0], &p[1])).collect();
        out.push(next);
    }
    out
}

/// The anchor over `leaves`: every shard's root in shard order, then the registry's.
pub fn anchor(leaves: &[Leaf]) -> [u8; 32] {
    levels(leaves).last().expect("a root level")[0]
}

/// A leaf's path to the anchor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    /// The leaf's position.
    pub index: usize,
    /// The siblings, from the leaf up.
    pub siblings: Vec<[u8; 32]>,
}

/// The proof of `leaves[index]` against [`anchor`]`(leaves)`.
pub fn prove(leaves: &[Leaf], index: usize) -> Proof {
    let lv = levels(leaves);
    let mut siblings = Vec::new();
    let mut i = index;
    for level in &lv[..lv.len() - 1] {
        siblings.push(level[i ^ 1]);
        i /= 2;
    }
    Proof { index, siblings }
}

/// Whether `l` is in the anchor `anchor` at `proof`'s position.
pub fn verify(anchor: &[u8; 32], l: &Leaf, proof: &Proof) -> bool {
    let mut h = leaf(l);
    let mut i = proof.index;
    for s in &proof.siblings {
        h = if i % 2 == 0 { node(&h, s) } else { node(s, &h) };
        i /= 2;
    }
    &h == anchor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: u16) -> Vec<Leaf> {
        let mut v: Vec<Leaf> = (0..n)
            .map(|i| {
                Leaf::Shard(ShardRoot {
                    shard: ShardId(i),
                    seq: u64::from(i) + 1,
                    commit: u64::from(i) * 3,
                    root: [i as u8; 32],
                    heads: [0xA0 | i as u8; 32],
                    construction: 2,
                    code: [0xC0 | i as u8; 32],
                    authority: [0xD0; 32],
                })
            })
            .collect();
        v.push(Leaf::Registry(RegistryRoot {
            seq: 2,
            version: 9,
            digest: [0x5E; 32],
        }));
        v
    }

    #[test]
    fn inv_19_every_shard_root_and_the_registry_prove_against_the_anchor() {
        for n in [1u16, 2, 3, 4, 8] {
            let l = leaves(n);
            let a = anchor(&l);
            for (i, x) in l.iter().enumerate() {
                let p = prove(&l, i);
                assert!(verify(&a, x, &p));
                let forged = match *x {
                    Leaf::Shard(mut r) => {
                        r.seq += 1;
                        Leaf::Shard(r)
                    }
                    Leaf::Registry(mut r) => {
                        r.version += 1;
                        Leaf::Registry(r)
                    }
                };
                assert!(!verify(&a, &forged, &p), "a revised leaf does not prove");
            }
        }
    }

    #[test]
    fn inv_19_the_anchor_changes_with_any_shard_or_its_heads() {
        let l = leaves(4);
        let mut s = l.clone();
        if let Leaf::Shard(r) = &mut s[2] {
            r.heads[0] ^= 1;
        }
        assert_ne!(anchor(&l), anchor(&s));
    }

    /// `T-037-03`: a shard's leaf binds its code, construction and mode, so the anchor over every
    /// shard does: another code, construction or mode for one shard is another anchor, and its
    /// leaf does not prove against the first.
    #[test]
    fn t037_03_the_shard_leaf_binds_code_construction_and_mode() {
        let l = leaves(4);
        let a = anchor(&l);
        let p = prove(&l, 2);
        for f in [
            |r: &mut ShardRoot| r.code[0] ^= 1,
            |r: &mut ShardRoot| r.construction ^= 3,
            |r: &mut ShardRoot| r.authority[0] ^= 1,
        ] {
            let mut s = l.clone();
            if let Leaf::Shard(r) = &mut s[2] {
                f(r);
            }
            assert_ne!(anchor(&s), a);
            assert!(!verify(&a, &s[2], &p));
        }
    }
}
