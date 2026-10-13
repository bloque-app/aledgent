// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A consistent cut of the book across its shards (amendment 0015, `REQ-RT-14`), for anchors
//! (`INV-19`) and for an asset's binding (`REQ-AS-8`).
//!
//! **Stream heads.** Every round of a shard commits, with everything else, a [`Heads`] record:
//! the shard's audit sequence (one per round, `audit/<n>`), its kernel commit and root, the last
//! principals version it applied, and, for every peer, the last credit sequence it **enqueued**
//! for that peer, the last it **applied** from it, and the sequence through which that peer's
//! acknowledgement let it **trim** its outbox. The record travels to the anchor's assembler as an
//! audit message (`REQ-AT-2`, `REQ-RT-13`): exactly once, in sequence, gaps visible.
//!
//! **The cut condition.** One record per shard, and one record of the registry, form a
//! consistent cut when, for every ordered pair of shards (source *i*, destination *j*):
//!
//! ```text
//!     trimmed_i[j]  ≤  applied_j[i]  ≤  enqueued_i[j]
//! ```
//!
//! and every shard's principals version is at most the registry's version in the cut. The right
//! inequality is the one that matters for value: nothing is applied in the cut that was not sent
//! in it, so no transfer is counted at its payee while its payer still holds it (**no double
//! count**). The credits `applied_j[i] + 1 ..= enqueued_i[j]` are in transit at the cut; their
//! value is in the two shards' in-transit positions, which the kernel roots commit to, and
//! nowhere else (**no lost value**). The left inequality keeps those credits in the source's
//! outbox at its record, so the in-transit value can be listed credit by credit (`NFR-9`'s second
//! axis).
//!
//! **Assembling** ([`Assembler`]). The anchor's assembler keeps each shard's records since the
//! last anchored cut and finds the **greatest consistent cut** at or below the latest records: it
//! starts from the latest of each, and while a pair breaks the condition it moves back the record
//! that is ahead (the destination when it applied more than the source's record enqueued, the
//! source when it trimmed past what the destination's record applied, a shard whose principals
//! version is past the registry's). Every move goes back, every consistent cut below the start
//! stays below the current one, and the last anchored cut is consistent, so the search ends at
//! the greatest consistent cut and never below the last anchor. Under load it trails the latest
//! records by about one relay hop; it never waits for the book to be idle.
//!
//! **Binding** ([`bind_amount`]). While an asset is being bound its issuance is refused in every
//! shard that has applied the registry's `Binding` change; each shard reports, in the commit that
//! applies it, its issuance member's position (a [`Share`]). The settlement side takes the
//! greatest consistent cut in which every shard has applied that change, and the amount the
//! settlement layer must hold is the negation of the shares' sum: the book's outstanding balance
//! of the asset at that cut, in transit included, counted once.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use cipher_clerk::crypto::blake2b_256;
use serde::{Deserialize, Serialize};

use crate::anchor::{self, Leaf, RegistryRoot, ShardRoot};
use crate::model::ShardId;

const HEADS: &[u8] = b"aledgent/cut/heads/v1";

/// One shard's stream heads at one of its commits (module documentation).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heads {
    /// The shard.
    pub shard: ShardId,
    /// Its audit sequence: rounds committed, 0 at its founding.
    pub seq: u64,
    /// Its kernel's last commit (0: none).
    pub commit: u64,
    /// Its kernel's composite root.
    pub root: [u8; 32],
    /// The last principals version it applied.
    pub feed: u64,
    /// Per peer, the last credit sequence enqueued for it (peers with none left out).
    pub enqueued: Vec<(ShardId, u64)>,
    /// Per peer, the last credit sequence applied from it.
    pub applied: Vec<(ShardId, u64)>,
    /// Per peer, the sequence through which its outbox to that peer is trimmed.
    pub trimmed: Vec<(ShardId, u64)>,
}

fn head_of(v: &[(ShardId, u64)], peer: ShardId) -> u64 {
    v.binary_search_by_key(&peer, |(p, _)| *p)
        .map(|i| v[i].1)
        .unwrap_or(0)
}

impl Heads {
    /// A shard's record at its founding: no round, no message.
    pub fn genesis(shard: ShardId, root: [u8; 32]) -> Self {
        Heads {
            shard,
            root,
            ..Heads::default()
        }
    }

    /// The last credit sequence enqueued for `to`.
    pub fn enqueued_to(&self, to: ShardId) -> u64 {
        head_of(&self.enqueued, to)
    }

    /// The last credit sequence applied from `from`.
    pub fn applied_from(&self, from: ShardId) -> u64 {
        head_of(&self.applied, from)
    }

    /// The sequence through which the outbox to `to` is trimmed.
    pub fn trimmed_to(&self, to: ShardId) -> u64 {
        head_of(&self.trimmed, to)
    }

    /// A digest of the heads, which the anchor commits to beside the root.
    pub fn digest(&self) -> [u8; 32] {
        let mut b = Vec::with_capacity(16 + 30 * self.enqueued.len());
        b.extend_from_slice(&self.feed.to_be_bytes());
        for v in [&self.enqueued, &self.applied, &self.trimmed] {
            b.extend_from_slice(&(v.len() as u32).to_be_bytes());
            for (p, s) in v {
                b.extend_from_slice(&p.0.to_be_bytes());
                b.extend_from_slice(&s.to_be_bytes());
            }
        }
        blake2b_256(HEADS, &[&b])
    }

    /// This record's leaf in an anchor.
    pub fn shard_root(&self) -> ShardRoot {
        ShardRoot {
            shard: self.shard,
            seq: self.seq,
            commit: self.commit,
            root: self.root,
            heads: self.digest(),
        }
    }
}

/// A shard's issuance position in an asset being bound, reported in the commit that applied the
/// registry's `Binding` change at `version` (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    /// The asset.
    pub asset: u32,
    /// The registry version of the `Binding` change.
    pub version: u64,
    /// The shard's issuance member's position (signed, as the kernel nets it).
    pub issued: i128,
}

/// Why a set of records is not a consistent cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inconsistency {
    /// Not one record per shard, in shard order.
    Shape,
    /// The destination applied a credit its source's record had not enqueued: counted twice.
    Ahead {
        /// The source.
        from: ShardId,
        /// The destination.
        to: ShardId,
        /// The destination's applied head.
        applied: u64,
        /// The source's enqueued head.
        enqueued: u64,
    },
    /// The source trimmed credits its destination's record had not applied: not listable.
    Trimmed {
        /// The source.
        from: ShardId,
        /// The destination.
        to: ShardId,
        /// The source's trim head.
        trimmed: u64,
        /// The destination's applied head.
        applied: u64,
    },
    /// A shard applied a principals change the cut's registry does not hold.
    Registry {
        /// The shard.
        shard: ShardId,
        /// Its principals version.
        feed: u64,
        /// The registry's version in the cut.
        version: u64,
    },
}

/// One record per shard and the registry's: a cut of the book.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cut {
    /// One record per shard, in shard order.
    pub shards: Vec<Heads>,
    /// The registry's record.
    pub registry: RegistryRoot,
}

/// The first pair (in a fixed order) that breaks the cut condition, if any.
fn violation(shards: &[&Heads], registry: &RegistryRoot) -> Option<Inconsistency> {
    for j in shards {
        if j.feed > registry.version {
            return Some(Inconsistency::Registry {
                shard: j.shard,
                feed: j.feed,
                version: registry.version,
            });
        }
    }
    for i in shards {
        for j in shards {
            if i.shard == j.shard {
                continue;
            }
            let applied = j.applied_from(i.shard);
            let enqueued = i.enqueued_to(j.shard);
            if applied > enqueued {
                return Some(Inconsistency::Ahead {
                    from: i.shard,
                    to: j.shard,
                    applied,
                    enqueued,
                });
            }
            let trimmed = i.trimmed_to(j.shard);
            if trimmed > applied {
                return Some(Inconsistency::Trimmed {
                    from: i.shard,
                    to: j.shard,
                    trimmed,
                    applied,
                });
            }
        }
    }
    None
}

impl Cut {
    /// Whether this is a consistent cut (module documentation).
    pub fn check(&self) -> Result<(), Inconsistency> {
        if self
            .shards
            .iter()
            .enumerate()
            .any(|(i, h)| usize::from(h.shard.0) != i)
        {
            return Err(Inconsistency::Shape);
        }
        let refs: Vec<&Heads> = self.shards.iter().collect();
        match violation(&refs, &self.registry) {
            Some(v) => Err(v),
            None => Ok(()),
        }
    }

    /// The credits from `from` to `to` in transit at this cut: `(applied, enqueued]`.
    pub fn in_transit(&self, from: ShardId, to: ShardId) -> (u64, u64) {
        let applied = self.shards[usize::from(to.0)].applied_from(from);
        let enqueued = self.shards[usize::from(from.0)].enqueued_to(to);
        (applied, enqueued)
    }

    /// The anchor's leaves: every shard's root with its heads, then the registry's.
    pub fn leaves(&self) -> Vec<Leaf> {
        let mut out: Vec<Leaf> = self
            .shards
            .iter()
            .map(|h| Leaf::Shard(h.shard_root()))
            .collect();
        out.push(Leaf::Registry(self.registry));
        out
    }

    /// The root of roots over this cut (`INV-19`).
    pub fn anchor(&self) -> [u8; 32] {
        anchor::anchor(&self.leaves())
    }

    /// Every shard's root at this cut.
    pub fn roots(&self) -> Vec<ShardRoot> {
        self.shards.iter().map(Heads::shard_root).collect()
    }

    /// Whether every shard's record satisfies `pred`.
    pub fn all(&self, pred: impl Fn(&Heads) -> bool) -> bool {
        self.shards.iter().all(pred)
    }

    /// Whether this cut is at or after `other` in every shard and in the registry.
    pub fn dominates(&self, other: &Cut) -> bool {
        self.shards.len() == other.shards.len()
            && self
                .shards
                .iter()
                .zip(&other.shards)
                .all(|(a, b)| a.seq >= b.seq)
            && self.registry.seq >= other.registry.seq
    }
}

/// One audit stream's records, applied exactly once and in sequence (`REQ-RT-13`).
#[derive(Clone, Debug, Default)]
struct Stream<T> {
    /// Applied records, ascending; the first is the floor (the last anchored cut's).
    applied: Vec<(u64, T)>,
    /// Records past a gap, waiting.
    waiting: BTreeMap<u64, T>,
}

impl<T> Stream<T> {
    fn new(seq: u64, first: T) -> Self {
        Stream {
            applied: alloc::vec![(seq, first)],
            waiting: BTreeMap::new(),
        }
    }

    fn head(&self) -> u64 {
        self.applied.last().expect("a floor").0
    }

    /// Deliver record `seq`: a repeat changes nothing, a record after a gap waits. Returns how
    /// many records were applied.
    fn deliver(&mut self, seq: u64, record: T) -> usize {
        if seq <= self.head() {
            return 0;
        }
        self.waiting.entry(seq).or_insert(record);
        let mut n = 0;
        while let Some(r) = self.waiting.remove(&(self.head() + 1)) {
            let s = self.head() + 1;
            self.applied.push((s, r));
            n += 1;
        }
        n
    }

    /// Keep records from `seq` on (`seq` is applied).
    fn settle(&mut self, seq: u64) {
        let i = self.applied.partition_point(|(s, _)| *s < seq);
        self.applied.drain(..i);
    }
}

/// The anchor's assembler: each writer's audit stream, and the greatest consistent cut over them
/// (module documentation).
#[derive(Clone, Debug)]
pub struct Assembler {
    shards: Vec<Stream<Heads>>,
    registry: Stream<RegistryRoot>,
}

impl Assembler {
    /// An assembler whose floor is each shard's founding record and the registry's.
    pub fn new(genesis: Vec<Heads>, registry: RegistryRoot) -> Self {
        Assembler {
            shards: genesis.into_iter().map(|h| Stream::new(h.seq, h)).collect(),
            registry: Stream::new(registry.seq, registry),
        }
    }

    /// Deliver a shard's record, in any order and any number of times. Returns how many records
    /// it let apply.
    pub fn deliver(&mut self, h: Heads) -> usize {
        let i = usize::from(h.shard.0);
        let seq = h.seq;
        self.shards[i].deliver(seq, h)
    }

    /// Deliver the registry's record.
    pub fn deliver_registry(&mut self, r: RegistryRoot) -> usize {
        self.registry.deliver(r.seq, r)
    }

    /// The last record applied from `shard`: what its audit stream may trim through.
    pub fn applied(&self, shard: ShardId) -> u64 {
        self.shards[usize::from(shard.0)].head()
    }

    /// The last registry record applied.
    pub fn registry_applied(&self) -> u64 {
        self.registry.head()
    }

    /// Each writer whose stream waits at a gap, with its last applied record and how many wait
    /// behind the gap (`REQ-AT-2`: a gap is visible, never silent).
    pub fn gaps(&self) -> Vec<(ShardId, u64, usize)> {
        self.shards
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.waiting.is_empty())
            .map(|(i, s)| (ShardId(i as u16), s.head(), s.waiting.len()))
            .collect()
    }

    /// The greatest consistent cut at or below the latest records applied (module documentation).
    pub fn cut(&self) -> Cut {
        let mut idx: Vec<usize> = self.shards.iter().map(|s| s.applied.len() - 1).collect();
        let registry = self.registry.applied.last().expect("a floor").1;
        loop {
            let cur: Vec<&Heads> = self
                .shards
                .iter()
                .zip(&idx)
                .map(|(s, i)| &s.applied[*i].1)
                .collect();
            let Some(v) = violation(&cur, &registry) else {
                break;
            };
            let back = match v {
                Inconsistency::Ahead { to, .. } => to,
                Inconsistency::Trimmed { from, .. } => from,
                Inconsistency::Registry { shard, .. } => shard,
                Inconsistency::Shape => unreachable!("records are per shard"),
            };
            let b = usize::from(back.0);
            // The floor is consistent and below the start, so it is never passed.
            idx[b] = idx[b]
                .checked_sub(1)
                .expect("the last anchored cut bounds the search");
        }
        Cut {
            shards: self
                .shards
                .iter()
                .zip(&idx)
                .map(|(s, i)| s.applied[*i].1.clone())
                .collect(),
            registry,
        }
    }

    /// The latest record applied from every writer, as one cut: what an anchor taken without the
    /// cut condition would commit to. It need not be consistent ([`Cut::check`]).
    pub fn latest(&self) -> Cut {
        Cut {
            shards: self
                .shards
                .iter()
                .map(|s| s.applied.last().expect("a floor").1.clone())
                .collect(),
            registry: self.registry.applied.last().expect("a floor").1,
        }
    }

    /// The greatest consistent cut, if every shard's record in it satisfies `pred`; otherwise
    /// none yet (a later delivery may give one).
    pub fn cut_where(&self, pred: impl Fn(&Heads) -> bool) -> Option<Cut> {
        let c = self.cut();
        c.all(pred).then_some(c)
    }

    /// `cut` was anchored: it becomes the floor, and older records are dropped.
    pub fn settle(&mut self, cut: &Cut) {
        for (s, h) in self.shards.iter_mut().zip(&cut.shards) {
            s.settle(h.seq);
        }
        self.registry.settle(cut.registry.seq);
    }

    /// Records kept, over every writer.
    pub fn kept(&self) -> usize {
        self.shards.iter().map(|s| s.applied.len()).sum::<usize>() + self.registry.applied.len()
    }
}

/// Why a binding's amount cannot be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindRefusal {
    /// The cut is not consistent.
    Inconsistent(Inconsistency),
    /// A shard in the cut has not applied the `Binding` change yet.
    NotFrozen(ShardId),
    /// A shard's share for this asset and version has not arrived.
    NoShare(ShardId),
    /// The outstanding balance is negative or does not fit: never bindable.
    Range,
}

/// The amount a binding of `asset` at registry `version` asks the settlement layer to hold: the
/// negation of the shards' issuance positions, at a consistent cut in which every shard has
/// applied the `Binding` change (module documentation). `shares` holds each shard's share.
pub fn bind_amount(
    cut: &Cut,
    shares: &BTreeMap<ShardId, Share>,
    asset: u32,
    version: u64,
) -> Result<u64, BindRefusal> {
    cut.check().map_err(BindRefusal::Inconsistent)?;
    let mut issued: i128 = 0;
    for h in &cut.shards {
        if h.feed < version {
            return Err(BindRefusal::NotFrozen(h.shard));
        }
        let s = shares
            .get(&h.shard)
            .filter(|s| s.asset == asset && s.version == version)
            .ok_or(BindRefusal::NoShare(h.shard))?;
        issued = issued.checked_add(s.issued).ok_or(BindRefusal::Range)?;
    }
    u64::try_from(issued.checked_neg().ok_or(BindRefusal::Range)?).map_err(|_| BindRefusal::Range)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(
        shard: u16,
        seq: u64,
        enq: &[(u16, u64)],
        app: &[(u16, u64)],
        trim: &[(u16, u64)],
    ) -> Heads {
        let v = |x: &[(u16, u64)]| x.iter().map(|(p, s)| (ShardId(*p), *s)).collect();
        Heads {
            shard: ShardId(shard),
            seq,
            commit: seq,
            root: [shard as u8; 32],
            feed: 0,
            enqueued: v(enq),
            applied: v(app),
            trimmed: v(trim),
        }
    }

    fn reg() -> RegistryRoot {
        RegistryRoot {
            seq: 0,
            version: 0,
            digest: [0; 32],
        }
    }

    #[test]
    fn req_rt_14_applied_beyond_enqueued_is_not_a_cut() {
        let c = Cut {
            shards: alloc::vec![h(0, 1, &[(1, 2)], &[], &[]), h(1, 1, &[], &[(0, 3)], &[])],
            registry: reg(),
        };
        assert!(matches!(
            c.check(),
            Err(Inconsistency::Ahead {
                applied: 3,
                enqueued: 2,
                ..
            })
        ));
        let c = Cut {
            shards: alloc::vec![
                h(0, 1, &[(1, 3)], &[], &[(1, 3)]),
                h(1, 1, &[], &[(0, 2)], &[])
            ],
            registry: reg(),
        };
        assert!(matches!(
            c.check(),
            Err(Inconsistency::Trimmed {
                trimmed: 3,
                applied: 2,
                ..
            })
        ));
    }

    #[test]
    fn req_rt_14_the_assembler_moves_back_the_record_that_is_ahead() {
        let mut a = Assembler::new(
            alloc::vec![
                Heads::genesis(ShardId(0), [0; 32]),
                Heads::genesis(ShardId(1), [1; 32])
            ],
            reg(),
        );
        // Shard 0 enqueues 2 credits for 1 at seq 1; shard 1 applies them at seq 2 (seq 1 idle).
        a.deliver(h(0, 1, &[(1, 2)], &[], &[]));
        a.deliver(h(1, 1, &[], &[], &[]));
        a.deliver(h(1, 2, &[], &[(0, 2)], &[]));
        // Shard 0's later record, enqueuing more and trimming through 2, arrives out of order.
        a.deliver(h(0, 3, &[(1, 5)], &[], &[(1, 2)]));
        let c = a.cut();
        c.check().expect("consistent");
        assert_eq!((c.shards[0].seq, c.shards[1].seq), (1, 2));
        a.deliver(h(0, 2, &[(1, 4)], &[], &[]));
        let c = a.cut();
        assert_eq!((c.shards[0].seq, c.shards[1].seq), (3, 2));
        assert_eq!(c.in_transit(ShardId(0), ShardId(1)), (2, 5));
        a.settle(&c);
        assert_eq!(a.kept(), 3);
        // The destination catches up: the cut follows it.
        a.deliver(h(1, 3, &[], &[(0, 5)], &[]));
        let c = a.cut();
        assert_eq!((c.shards[0].seq, c.shards[1].seq), (3, 3));
    }
}
