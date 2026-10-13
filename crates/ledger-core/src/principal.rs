// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Principals and credential sets (`F-003` §5, `REQ-PN-1`, `REQ-AU-8`, `REQ-AU-20` model only).
//!
//! `ledger-core` verifies no request signature. Ingress (`F-009`) and the core's binding (`F-005`)
//! authenticate the call first, and hand the library a [`Caller`]: the initiating identity, the
//! credential it presented, and whom it acts for. [`resolve`] turns that into a [`Principal`], or
//! refuses with `ERR-CredentialInvalid` when the initiator cannot be determined (`REQ-PN-1`):
//!
//! - the identity is unknown, or closed;
//! - the credential is unknown, removed, or belongs to another identity;
//! - the caller claims to act for an identity that does not exist, or an agent claims to act for
//!   anyone but the identity it was opened for.
//!
//! An identity holds a **set** of credentials (`REQ-AU-20`). Any member may exercise the
//! identity's owner authority, unless it is scoped to a subset of accounts. Changing the set is
//! `F-012`'s. A member holds only a [`Verifier`], public material such as a public key, tagged
//! with its [`Scheme`] (`F-028`): no secret is held here. The types that carry verifier material implement no `Debug`, `Display` or
//! `Serialize`, so none of it can reach a log by accident (`REQ-AU-8`):
//!
//! ```compile_fail
//! // REQ-AU-8: a verifier cannot be formatted.
//! let v = ledger_core::principal::Verifier::new(vec![1, 2, 3]);
//! let _ = format!("{:?}", v);
//! ```

use alloc::vec::Vec;

use crate::id::{AccountId, CredentialId, IdentityId, Mint};
use crate::identity::{IdentityKind, IdentityStatus};
use crate::refusal::Refusal;
pub use crate::scheme::Scheme;
use crate::set::NonEmptySet;
use crate::state::State;

/// What a credential is checked against: public material only, never a secret. It names its
/// [`Scheme`] (`DEC-19`, `F-028`); the material is opaque here except for its shape
/// ([`Verifier::well_formed`]).
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Verifier {
    scheme: Scheme,
    material: Vec<u8>,
}

impl Verifier {
    /// A `key`-scheme verifier: the scheme of every verifier written before `F-028`.
    pub fn new(bytes: Vec<u8>) -> Verifier {
        Verifier::of(Scheme::Key, bytes)
    }

    /// A verifier of `scheme`.
    pub fn of(scheme: Scheme, material: Vec<u8>) -> Verifier {
        Verifier { scheme, material }
    }

    /// Its scheme.
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The material, for the boundary that checks proofs against it (`F-009`, `F-028`).
    pub fn as_bytes(&self) -> &[u8] {
        &self.material
    }

    /// Whether the material has its scheme's shape (`DEC-19`). The core refuses a verifier that
    /// does not, wherever one joins a set.
    pub fn well_formed(&self) -> bool {
        self.scheme.well_formed(&self.material)
    }

    /// Write it into a request digest or a signed message: the scheme's tag, then the material.
    /// The same bytes under another scheme are another verifier.
    pub fn canon_into(&self, c: &mut crate::lifecycle::keys::Canon) {
        c.u8(self.scheme.tag()).bytes(&self.material);
    }
}

/// What kind of member of the set a credential is (`REQ-AU-20`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CredentialMarker {
    /// An everyday credential.
    Ordinary,
    /// A holder-held recovery credential (`REQ-AU-18`, `F-012`), marked as such.
    RecoveryHeld,
    /// Imported by the operator from a prior system (`REQ-MG-7`, `F-028`). It proves nothing by
    /// being imported (`REQ-MG-8`).
    Imported,
}

/// Whether a credential works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CredentialStatus {
    /// It works.
    Active,
    /// It was removed from the set (`F-012`). Removed is absorbing.
    Removed,
}

/// A member of an identity's credential set.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    id: CredentialId,
    minted: Mint,
    identity: IdentityId,
    marker: CredentialMarker,
    scope: Option<NonEmptySet<AccountId>>,
    status: CredentialStatus,
    verifier: Verifier,
}

impl Credential {
    /// A new, active member of `identity`'s set, minted by `minted`. `scope` limits it to some of
    /// the identity's accounts; `None` leaves it unscoped.
    pub fn new(
        minted: Mint,
        identity: IdentityId,
        marker: CredentialMarker,
        scope: Option<NonEmptySet<AccountId>>,
        verifier: Verifier,
    ) -> Credential {
        Credential {
            id: CredentialId::mint(&minted.op, minted.ordinal),
            minted,
            identity,
            marker,
            scope,
            status: CredentialStatus::Active,
            verifier,
        }
    }

    /// Its identifier.
    pub const fn id(&self) -> CredentialId {
        self.id
    }

    /// The operation that minted it.
    pub const fn minted(&self) -> Mint {
        self.minted
    }

    /// The one identity it belongs to.
    pub const fn identity(&self) -> IdentityId {
        self.identity
    }

    /// What kind of member it is.
    pub const fn marker(&self) -> CredentialMarker {
        self.marker
    }

    /// The accounts it is limited to, if any.
    pub const fn scope(&self) -> Option<&NonEmptySet<AccountId>> {
        self.scope.as_ref()
    }

    /// Whether it works.
    pub const fn status(&self) -> CredentialStatus {
        self.status
    }

    /// Its verifier.
    pub const fn verifier(&self) -> &Verifier {
        &self.verifier
    }

    /// The same credential with another status. The store decides whether the change is allowed.
    pub(crate) fn with_status(mut self, status: CredentialStatus) -> Credential {
        self.status = status;
        self
    }
}

/// Who is calling, as authenticated before the library (`F-005`, `F-009`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Caller {
    /// The initiating identity.
    pub principal: IdentityId,
    /// The credential it presented.
    pub credential: CredentialId,
    /// The identity it acts for, where that is different (`REQ-PN-1`).
    pub on_behalf_of: Option<IdentityId>,
}

impl Caller {
    /// An identity acting for itself with one of its credentials.
    pub const fn new(principal: IdentityId, credential: CredentialId) -> Caller {
        Caller {
            principal,
            credential,
            on_behalf_of: None,
        }
    }

    /// The same caller, acting for `other`.
    pub const fn on_behalf_of(mut self, other: IdentityId) -> Caller {
        self.on_behalf_of = Some(other);
        self
    }
}

/// A caller whose initiator has been determined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// The initiating identity.
    pub identity: IdentityId,
    /// Its kind.
    pub kind: IdentityKind,
    /// The credential used.
    pub credential: CredentialId,
    /// The accounts the credential is limited to, if any.
    pub scope: Option<NonEmptySet<AccountId>>,
    /// Whom it acts for: as claimed, or, for an agent, the identity it acts for.
    pub on_behalf_of: Option<IdentityId>,
}

impl Principal {
    /// Whether the credential used may act on `account` at all.
    pub fn in_scope(&self, account: &AccountId) -> bool {
        self.scope.as_ref().is_none_or(|s| s.contains(account))
    }

    /// Whether the credential used is unscoped.
    pub const fn unscoped(&self) -> bool {
        self.scope.is_none()
    }
}

/// Determine the initiator (`REQ-PN-1`), or refuse. See the module documentation.
///
/// The settlement adapter's principal — the identity the settlement configuration names as the
/// authoritative source — is refused here with `ERR-NotAuthorised` (`F-016`, `REQ-SA-8`). Every
/// command and query resolves its caller through this function, so the adapter can reach none
/// of them: its three capabilities (acknowledging an instruction, recording observations,
/// declaring itself) go through [`resolve_adapter`] and nothing else does.
pub fn resolve(state: &State, caller: &Caller) -> Result<Principal, Refusal> {
    let p = resolve_any(state, caller)?;
    if is_adapter(state, &p.identity) {
        return Err(Refusal::NotAuthorised);
    }
    Ok(p)
}

/// Whether `identity` is the settlement adapter's principal (`F-016`).
pub fn is_adapter(state: &State, identity: &IdentityId) -> bool {
    state
        .settlement()
        .config()
        .is_some_and(|c| c.authoritative == crate::settlement::SourceId::of_identity(identity))
}

/// Resolve the settlement adapter's principal, and only it (`F-016`): the credential checks of
/// [`resolve`], then `ERR-NotAuthorised` unless the identity is the configured authoritative
/// source. Acting on behalf of anyone is refused: the adapter acts for no one.
pub fn resolve_adapter(state: &State, caller: &Caller) -> Result<Principal, Refusal> {
    let p = resolve_any(state, caller)?;
    if !is_adapter(state, &p.identity) || p.on_behalf_of.is_some() {
        return Err(Refusal::NotAuthorised);
    }
    Ok(p)
}

fn resolve_any(state: &State, caller: &Caller) -> Result<Principal, Refusal> {
    // A seeded book takes nothing but its seeding until the seed is sealed (`F-036`,
    // `REQ-RC-6`). The check reads the seed's record, so the core Agent's replay decides it on
    // the same witnessed leaf as the host.
    if state.spec_open() {
        return Err(Refusal::InitialStateNotSealed);
    }
    resolve_unsealed(state, caller)
}

/// [`resolve`]'s credential checks without the seal's guard: for reads that must work while a
/// seed is being applied (the operator's signals, `F-018`), never for a command.
pub fn resolve_unsealed(state: &State, caller: &Caller) -> Result<Principal, Refusal> {
    let identity = state
        .identity(&caller.principal)
        .ok_or(Refusal::CredentialInvalid)?;
    let credential = state
        .credential(&caller.credential)
        .ok_or(Refusal::CredentialInvalid)?;
    if credential.identity() != identity.id() || credential.status() != CredentialStatus::Active {
        return Err(Refusal::CredentialInvalid);
    }
    match identity.status() {
        IdentityStatus::Active => {}
        IdentityStatus::Restricted => return Err(Refusal::IdentityRestricted),
        IdentityStatus::Closed => return Err(Refusal::CredentialInvalid),
    }
    let on_behalf_of = match (identity.kind(), identity.acts_for(), caller.on_behalf_of) {
        (IdentityKind::Agent, Some(principal), None) => Some(principal),
        (IdentityKind::Agent, Some(principal), Some(claimed)) if claimed == principal => {
            Some(principal)
        }
        (IdentityKind::Agent, _, _) => return Err(Refusal::CredentialInvalid),
        (_, _, Some(claimed)) => {
            state.identity(&claimed).ok_or(Refusal::CredentialInvalid)?;
            Some(claimed)
        }
        (_, _, None) => None,
    };
    Ok(Principal {
        identity: identity.id(),
        kind: identity.kind(),
        credential: credential.id(),
        scope: credential.scope().cloned(),
        on_behalf_of,
    })
}
