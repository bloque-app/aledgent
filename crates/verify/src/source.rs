// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The public read the verifier needs: an anchor, and the deployment's published clerk key, each
//! read at a **finalized** block of the settlement layer (`REQ-PR-8`: an anchor that has not
//! reached finality establishes nothing).
//!
//! [`AnchorSource`] is the whole of it. The node read (`crate::rpc`, feature `rpc`) is one
//! implementation; [`MemAnchors`] is a fake for tests and for a read saved earlier.
//!
//! ## Venues
//!
//! A deployment's anchors live in a **venue** on the settlement layer ([`Venue`], `F-027`,
//! `DEC-17`): `F-019`'s settlement pallet, or `F-026`'s settlement contract, and later the Spaces
//! pallet. A deployment can change venue; every anchor records the venue it lives in, and a
//! disclosure names it, so the verifier reads each anchor where it was stored, whichever venue
//! the deployment uses today.
//!
//! ## Anchor sequences
//!
//! The settlement contract's governance may start a new **anchor sequence** on a live instance for
//! a re-founded book (amendment 0039 `[PENDING AMENDMENT 0039]`): the next anchor is then numbered
//! from a base it chose, so two books can hold anchors of the same number in one venue.
//! [`AnchorSource::anchors`] answers for every sequence that used a number, each named by its
//! [`Sequence`], and says which ones it could not read; the verifier lets the disclosure's
//! commitment decide which book's anchor it is (`crate::check`), and never answers with the wrong
//! book's anchor or with "not final" for a sequence it could not read.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Where a deployment's anchors are stored on the settlement layer (`F-027` T-027-04, `DEC-17`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Venue {
    /// `F-019`'s settlement pallet: `AledgentSettlement::Anchors(instance, number)`, and the
    /// clerk key on `Instances(instance)`.
    Pallet {
        /// The deployment's instance in the pallet.
        instance: u32,
    },
    /// `F-026`'s settlement contract on Kreivo: the anchors and the clerk key are in the
    /// contract's own storage: its latest anchor in place (`latest_anchor`), earlier ones as of
    /// the finalized blocks at which they were the latest (`crate::rpc`), and `registration`.
    /// Its anchors may run in more than one [`Sequence`] (amendment 0039).
    Contract {
        /// The contract's account.
        account: [u8; 32],
    },
}

impl Venue {
    /// Its one-byte tag in the disclosure envelope.
    pub const fn tag(&self) -> u8 {
        match self {
            Venue::Pallet { .. } => 1,
            Venue::Contract { .. } => 2,
        }
    }
}

/// An anchor as the settlement layer stores it, read at a finalized block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicAnchor {
    /// The commitment, exactly as it was submitted.
    pub commitment: [u8; 32],
    /// The block that stored it.
    pub stored_at: u64,
    /// The finalized block it was read at.
    pub read_at: u64,
}

/// An anchor sequence of a venue (amendment 0039 `[PENDING AMENDMENT 0039]`): the run of anchor
/// numbers one book follows there. Sequence 0 is the one the venue began with; governance may
/// start another for a re-founded book, whose anchors are numbered from `base + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sequence {
    /// 0 for the venue's first sequence; `n` for the `n`-th one governance started.
    pub epoch: u32,
    /// The number the sequence starts after: its first anchor is `base + 1`.
    pub base: u64,
    /// The 32-byte name of the book the sequence anchors, as governance gave it when it started
    /// the sequence; `None` for sequence 0, which no one named.
    pub book: Option<[u8; 32]>,
}

impl Sequence {
    /// The sequence every venue begins with, and the only one a venue that was never reset has.
    pub const FIRST: Sequence = Sequence {
        epoch: 0,
        base: 0,
        book: None,
    };
}

/// What could not be read about a number, so that the verifier can say "unknown" rather than
/// "not final" (amendment 0039).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unread {
    /// This sequence holds an anchor of the number (its numbers run past it), and the anchor
    /// could not be read: it is no longer that sequence's latest, and the node keeps no state
    /// from when it was.
    Anchor {
        /// The sequence.
        sequence: Sequence,
        /// Why.
        why: String,
    },
    /// The sequences before `epoch` could not be identified, because their records are in state
    /// the node no longer keeps: whether any of them holds the number is unknown.
    Before {
        /// The first sequence that was identified; every earlier one is unknown.
        epoch: u32,
        /// The last anchor before `epoch` started, if any: the latest anchor of the book before.
        last_anchor: Option<u64>,
        /// Why.
        why: String,
    },
}

/// Every final anchor of one number in a venue, one per [`Sequence`] that used the number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumberedAnchors {
    /// The venue's current sequence at the finalized head.
    pub current_epoch: u32,
    /// The anchors found: the current sequence's first, then earlier ones, latest first.
    pub found: Vec<(Sequence, PublicAnchor)>,
    /// What could not be read. Empty when every sequence that may hold the number was read.
    pub unread: Vec<Unread>,
}

impl NumberedAnchors {
    /// A venue that was never reset: its first sequence only.
    pub fn first_sequence(anchor: Option<PublicAnchor>) -> NumberedAnchors {
        NumberedAnchors {
            current_epoch: 0,
            found: anchor
                .map(|a| vec![(Sequence::FIRST, a)])
                .unwrap_or_default(),
            unread: Vec::new(),
        }
    }

    /// The one anchor of the number, for a caller that cannot tell sequences apart: `None` if no
    /// sequence holds it, and an error if more than one does or one could not be read, rather than
    /// a guess.
    pub fn single(&self) -> Result<Option<PublicAnchor>, String> {
        if let Some(u) = self.unread.first() {
            return Err(match u {
                Unread::Anchor { sequence, why } => alloc::format!(
                    "anchor sequence {} may hold this number and could not be read: {why}",
                    sequence.epoch
                ),
                Unread::Before { epoch, why, .. } => alloc::format!(
                    "the anchor sequences before {epoch} could not be identified: {why}"
                ),
            });
        }
        match self.found.as_slice() {
            [] => Ok(None),
            [(_, a)] => Ok(Some(*a)),
            more => Err(alloc::format!(
                "{} anchor sequences hold this number (books re-founded on one venue); read them \
                 with `AnchorSource::anchors`",
                more.len()
            )),
        }
    }
}

/// A public read of the settlement layer at its finalized head.
pub trait AnchorSource {
    /// The anchor `number` stored in `venue`, if it is final at the finalized head: stored at the
    /// head, or, in a venue that keeps only its latest anchor, stored at a finalized block and
    /// superseded since. In a venue whose anchors run in more than one [`Sequence`], an error when
    /// the number is ambiguous ([`NumberedAnchors::single`]); [`AnchorSource::anchors`] reads them
    /// all.
    fn anchor(&self, venue: &Venue, number: u64) -> Result<Option<PublicAnchor>, String>;

    /// Every final anchor numbered `number` in `venue`, one per [`Sequence`] that used the number,
    /// and what could not be read (amendment 0039). A source that knows of one sequence only
    /// answers with [`AnchorSource::anchor`]'s anchor in [`Sequence::FIRST`].
    fn anchors(&self, venue: &Venue, number: u64) -> Result<NumberedAnchors, String> {
        self.anchor(venue, number)
            .map(NumberedAnchors::first_sequence)
    }

    /// The clerk key the deployment publishes in `venue` (`F-017` accepted proposal: the anchor
    /// and the key come from one public source), if any.
    fn attestation_key(&self, venue: &Venue) -> Result<Option<[u8; 32]>, String>;
}

/// An in-memory [`AnchorSource`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemAnchors {
    anchors: BTreeMap<(Venue, u64), PublicAnchor>,
    numbered: BTreeMap<(Venue, u64), NumberedAnchors>,
    keys: BTreeMap<Venue, [u8; 32]>,
    unreadable: Option<String>,
}

impl MemAnchors {
    /// Nothing stored.
    pub fn new() -> MemAnchors {
        MemAnchors::default()
    }

    /// Store an anchor in `venue`.
    pub fn put_in(&mut self, venue: Venue, number: u64, anchor: PublicAnchor) -> &mut Self {
        self.anchors.insert((venue, number), anchor);
        self
    }

    /// Store an anchor in the settlement pallet, under `instance`.
    pub fn put(&mut self, instance: u32, number: u64, anchor: PublicAnchor) -> &mut Self {
        self.put_in(Venue::Pallet { instance }, number, anchor)
    }

    /// What [`AnchorSource::anchors`] answers for `number` in `venue`, as a venue whose anchors
    /// run in more than one [`Sequence`] would (amendment 0039).
    pub fn put_numbered(&mut self, venue: Venue, number: u64, read: NumberedAnchors) -> &mut Self {
        self.numbered.insert((venue, number), read);
        self
    }

    /// Publish a clerk key in `venue`.
    pub fn publish_key_in(&mut self, venue: Venue, key: [u8; 32]) -> &mut Self {
        self.keys.insert(venue, key);
        self
    }

    /// Publish a clerk key on an instance's record in the settlement pallet.
    pub fn publish_key(&mut self, instance: u32, key: [u8; 32]) -> &mut Self {
        self.publish_key_in(Venue::Pallet { instance }, key)
    }

    /// Make every read fail with `why`, as a node that cannot be reached would.
    pub fn fail(&mut self, why: &str) -> &mut Self {
        self.unreadable = Some(String::from(why));
        self
    }
}

impl AnchorSource for MemAnchors {
    fn anchor(&self, venue: &Venue, number: u64) -> Result<Option<PublicAnchor>, String> {
        match self.numbered.get(&(*venue, number)) {
            Some(read) => self.anchors(venue, number).and_then(|_| read.single()),
            None => match &self.unreadable {
                Some(why) => Err(why.clone()),
                None => Ok(self.anchors.get(&(*venue, number)).copied()),
            },
        }
    }

    fn anchors(&self, venue: &Venue, number: u64) -> Result<NumberedAnchors, String> {
        if let Some(why) = &self.unreadable {
            return Err(why.clone());
        }
        match self.numbered.get(&(*venue, number)) {
            Some(read) => Ok(read.clone()),
            None => Ok(NumberedAnchors::first_sequence(
                self.anchors.get(&(*venue, number)).copied(),
            )),
        }
    }

    fn attestation_key(&self, venue: &Venue) -> Result<Option<[u8; 32]>, String> {
        match &self.unreadable {
            Some(why) => Err(why.clone()),
            None => Ok(self.keys.get(venue).copied()),
        }
    }
}

impl<T: AnchorSource + ?Sized> AnchorSource for &T {
    fn anchor(&self, venue: &Venue, number: u64) -> Result<Option<PublicAnchor>, String> {
        (**self).anchor(venue, number)
    }

    fn anchors(&self, venue: &Venue, number: u64) -> Result<NumberedAnchors, String> {
        (**self).anchors(venue, number)
    }

    fn attestation_key(&self, venue: &Venue) -> Result<Option<[u8; 32]>, String> {
        (**self).attestation_key(venue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_027_04_an_anchor_is_found_in_the_venue_it_was_stored_in() {
        let a = PublicAnchor {
            commitment: [1; 32],
            stored_at: 3,
            read_at: 4,
        };
        let contract = Venue::Contract { account: [9; 32] };
        let mut m = MemAnchors::new();
        m.put(1, 1, a).put_in(contract, 2, a);
        assert_eq!(m.anchor(&Venue::Pallet { instance: 1 }, 1), Ok(Some(a)));
        assert_eq!(m.anchor(&contract, 2), Ok(Some(a)));
        // Each in its own venue only.
        assert_eq!(m.anchor(&contract, 1), Ok(None));
        assert_eq!(m.anchor(&Venue::Pallet { instance: 1 }, 2), Ok(None));
    }
}
