// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Integrating parties and the client key's domain (`REQ-ID-7`, `REQ-OP-10`, amendment 0011 R4,
//! `OQ-65`; `F-009` T-009-16).
//!
//! An **integrating party** is an organisation that integrates aledgent (SPEC §2). Like any
//! organisation it holds an identity in the book, and that identity is its identifier: a
//! [`PartyId`] is the party's organisation [`IdentityId`], minted like every identifier, so it
//! carries no caller text and no personal data (`INV-17`). There is no second identifier kind.
//!
//! The operator registers a party (`register_integrating_party`) with the principals tied to it,
//! and ties further principals later (`tie_principal`). A principal is tied to at most one party,
//! and a tie is never undone. These are the deployment's own configuration: the operator's
//! policy authority alone makes them, and no grant can carry them to another principal
//! (`REQ-ID-7`), so neither has an operator-surface kind.
//!
//! **The client key's domain** ([`KeyDomain`], `REQ-OP-10`) is the party a principal is tied to,
//! or the principal itself where it is tied to none. Within a party, the request a key names
//! includes its initiating principal, so the same key from another of its principals is another
//! request, and is refused with `ERR-ClientKeyReused`.

use alloc::collections::BTreeSet;

use crate::account::Time;
use crate::export::canon::{Canonical, Dec, DecodeError, Enc};
use crate::id::{IdentityId, OperationId};

/// An integrating party's identifier: its organisation identity's (`REQ-ID-7`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PartyId(IdentityId);

impl PartyId {
    /// The party whose organisation identity is `identity`.
    pub const fn of(identity: IdentityId) -> PartyId {
        PartyId(identity)
    }

    /// Its organisation identity.
    pub const fn identity(self) -> IdentityId {
        self.0
    }

    /// Its surface form: the identity's `did:aledgent:id:…`.
    pub fn to_did(self) -> alloc::string::String {
        self.0.to_did()
    }

    /// Parse the surface form of an identity identifier.
    pub fn parse(s: &str) -> Result<PartyId, crate::id::IdError> {
        IdentityId::parse(s).map(PartyId)
    }
}

/// The uniqueness domain of a client key (`REQ-OP-10`): the integrating party the initiating
/// principal is tied to, or the principal itself where it is tied to none (`REQ-ID-7`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyDomain {
    /// An integrating party: every principal tied to it shares the domain.
    Party(PartyId),
    /// A principal tied to no party, alone in its domain.
    Principal(IdentityId),
}

impl KeyDomain {
    /// The domain of `principal`, tied to `party` or to none.
    pub const fn of(principal: IdentityId, party: Option<PartyId>) -> KeyDomain {
        match party {
            Some(p) => KeyDomain::Party(p),
            None => KeyDomain::Principal(principal),
        }
    }
}

/// A registered integrating party (`REQ-ID-7`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegratingParty {
    /// The party.
    pub party: PartyId,
    /// The operation that registered it.
    pub registered_by: OperationId,
    /// When.
    pub registered_at: Time,
}

/// A principal tied to an integrating party (`REQ-ID-7`). Never undone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartyTie {
    /// The principal.
    pub principal: IdentityId,
    /// The party it is tied to.
    pub party: PartyId,
    /// The operation that tied it: the party's registration, or a `tie_principal`.
    pub tied_by: OperationId,
    /// When.
    pub tied_at: Time,
}

/// `register_integrating_party`'s request (SPEC §8.1).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RegisterIntegratingParty {
    /// The party: an active organisation identity, registered as no party before.
    pub party: PartyId,
    /// The principals tied to it on registration, each tied to no party. May be empty.
    pub principals: BTreeSet<IdentityId>,
}

/// One registered party and the principals tied to it, as the operator lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartyView {
    /// The party.
    pub party: IntegratingParty,
    /// Its principals, with when each was tied.
    pub principals: alloc::vec::Vec<PartyTie>,
}

impl Canonical for PartyId {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.0);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(PartyId(d.get()?))
    }
}

impl Canonical for IntegratingParty {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.party)
            .put(&self.registered_by)
            .put(&self.registered_at);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(IntegratingParty {
            party: d.get()?,
            registered_by: d.get()?,
            registered_at: d.get()?,
        })
    }
}

impl Canonical for PartyTie {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.principal)
            .put(&self.party)
            .put(&self.tied_by)
            .put(&self.tied_at);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(PartyTie {
            principal: d.get()?,
            party: d.get()?,
            tied_by: d.get()?,
            tied_at: d.get()?,
        })
    }
}
