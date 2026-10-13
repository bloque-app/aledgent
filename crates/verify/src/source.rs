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

use alloc::collections::BTreeMap;
use alloc::string::String;

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

/// A public read of the settlement layer at its finalized head.
pub trait AnchorSource {
    /// The anchor `number` stored in `venue`, if it is final at the finalized head: stored at the
    /// head, or, in a venue that keeps only its latest anchor, stored at a finalized block and
    /// superseded since.
    fn anchor(&self, venue: &Venue, number: u64) -> Result<Option<PublicAnchor>, String>;

    /// The clerk key the deployment publishes in `venue` (`F-017` accepted proposal: the anchor
    /// and the key come from one public source), if any.
    fn attestation_key(&self, venue: &Venue) -> Result<Option<[u8; 32]>, String>;
}

/// An in-memory [`AnchorSource`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemAnchors {
    anchors: BTreeMap<(Venue, u64), PublicAnchor>,
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
        match &self.unreadable {
            Some(why) => Err(why.clone()),
            None => Ok(self.anchors.get(&(*venue, number)).copied()),
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
