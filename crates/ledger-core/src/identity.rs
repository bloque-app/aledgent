// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Identities (`F-003` §3, SPEC §5.1, `REQ-AG-1`).
//!
//! An identity is a person, an organisation or an agent. Only an agent names the identity it acts
//! for, and it must (`REQ-AG-1`). An identity carries no name, handle, contact or other personal
//! data: nothing here needs rewriting when personal data held elsewhere is erased (`INV-17`,
//! `AC-I4.2`).

use crate::id::{IdentityId, Mint};
use crate::refusal::Refusal;

/// What an identity is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum IdentityKind {
    /// A natural person.
    Person,
    /// An organisation.
    Organisation,
    /// An agent: a principal with its own credential, acting for a named identity (`REQ-AG-1`).
    Agent,
}

/// Whether an identity may transact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum IdentityStatus {
    /// It may transact.
    Active,
    /// It may not transact (`ERR-IdentityRestricted`). It can be reinstated.
    Restricted,
    /// It is closed for good. Closed is absorbing.
    Closed,
}

/// An identity (SPEC §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    id: IdentityId,
    minted: Mint,
    kind: IdentityKind,
    status: IdentityStatus,
    acts_for: Option<IdentityId>,
}

impl Identity {
    /// A new, active identity minted by `minted`. An agent must name the identity it acts for, and
    /// only an agent may: otherwise `ERR-NotAuthorised` (no authority covers opening it).
    pub fn new(
        minted: Mint,
        kind: IdentityKind,
        acts_for: Option<IdentityId>,
    ) -> Result<Identity, Refusal> {
        match (kind, acts_for) {
            (IdentityKind::Agent, None) => return Err(Refusal::NotAuthorised),
            (IdentityKind::Person | IdentityKind::Organisation, Some(_)) => {
                return Err(Refusal::NotAuthorised);
            }
            _ => {}
        }
        Ok(Identity {
            id: IdentityId::mint(&minted.op, minted.ordinal),
            minted,
            kind,
            status: IdentityStatus::Active,
            acts_for,
        })
    }

    /// Its identifier.
    pub const fn id(&self) -> IdentityId {
        self.id
    }

    /// The operation that minted it.
    pub const fn minted(&self) -> Mint {
        self.minted
    }

    /// Its kind.
    pub const fn kind(&self) -> IdentityKind {
        self.kind
    }

    /// Its status.
    pub const fn status(&self) -> IdentityStatus {
        self.status
    }

    /// For an agent, the identity it acts for.
    pub const fn acts_for(&self) -> Option<IdentityId> {
        self.acts_for
    }

    /// `Ok` if the identity may transact, else `ERR-IdentityRestricted`.
    pub const fn require_active(&self) -> Result<(), Refusal> {
        match self.status {
            IdentityStatus::Active => Ok(()),
            IdentityStatus::Restricted | IdentityStatus::Closed => Err(Refusal::IdentityRestricted),
        }
    }

    /// The same identity with another status. The store decides whether the change is allowed.
    pub(crate) const fn with_status(mut self, status: IdentityStatus) -> Identity {
        self.status = status;
        self
    }

    /// The same record under an identifier that is not its minting: something only a raw store
    /// write could attempt, and the store refuses (`INV-17`). Tests and the conformance suite only
    /// (feature `conformance-ports`, `REQ-CS-5`).
    #[cfg(any(test, feature = "conformance-ports"))]
    pub fn forged(mut self, id: IdentityId) -> Identity {
        self.id = id;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::OperationId;
    use ledger_kernel::posting::OperationRef;

    fn mint(n: u8) -> Mint {
        Mint::new(OperationId::from_ref(OperationRef([n; 16])), 0)
    }

    #[test]
    fn req_ag_1_only_an_agent_acts_for_another_and_it_must() {
        let person = Identity::new(mint(1), IdentityKind::Person, None).unwrap();
        assert_eq!(person.id(), IdentityId::mint(&mint(1).op, 0));
        assert_eq!(person.status(), IdentityStatus::Active);
        let agent = Identity::new(mint(2), IdentityKind::Agent, Some(person.id())).unwrap();
        assert_eq!(agent.acts_for(), Some(person.id()));
        assert_ne!(agent.id(), person.id());
        assert_eq!(
            Identity::new(mint(3), IdentityKind::Agent, None),
            Err(Refusal::NotAuthorised)
        );
        assert_eq!(
            Identity::new(mint(4), IdentityKind::Organisation, Some(person.id())),
            Err(Refusal::NotAuthorised)
        );
    }

    #[test]
    fn err_identity_restricted_unless_active() {
        let p = Identity::new(mint(1), IdentityKind::Person, None).unwrap();
        assert_eq!(p.require_active(), Ok(()));
        for s in [IdentityStatus::Restricted, IdentityStatus::Closed] {
            assert_eq!(
                p.with_status(s).require_active(),
                Err(Refusal::IdentityRestricted)
            );
        }
    }
}
