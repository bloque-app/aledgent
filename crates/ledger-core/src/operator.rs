// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Operator operations: the records of `CTR-OPS` and their store rules (`F-013` §4–§8,
//! `REQ-OS-1`–`REQ-OS-4`, `REQ-AU-5`, `REQ-RC-2`, `REQ-SA-1`).
//!
//! - [`OperatorOpKind`]: every operator capability is its own kind. There is no "all" kind, and a
//!   grant must name a strict subset of the kinds, so a catch-all cannot be expressed
//!   (`REQ-OS-1`, `REQ-AU-5`).
//! - [`OperatorGrant`]: operator-surface authority, held by a principal for a non-empty set of
//!   kinds. The operator's organisation identity makes these grants with its policy authority
//!   (`DEP-8`); using an operator command needs a live grant naming its kind, whoever the
//!   principal is.
//! - [`PendingReversal`]: a proposed reversal, until a second, distinct principal approves it
//!   (`REQ-OS-3`). The store itself refuses an approval by the proposer.
//! - [`ResolutionCase`]: a `Needs-Review` operation's resolution (`REQ-RC-5`, amendment 0006 as
//!   refined on 2026-09-29): the evidence authorities submitted for the settlement adapter to
//!   verify, what it found, and a resolution one principal proposed without proof, until a second,
//!   distinct principal agrees.
//! - [`Incident`]: the operator incident channel, paged by `F-018` (`REQ-RC-2`).
//! - Integrating parties and the principals tied to them ([`crate::party`], `REQ-ID-7`): a party
//!   is registered once, and a principal is tied to at most one party, never untied.
//!
//! The adapter's declared support and the evidence `resolve_operation` cites are `F-010`'s
//! records, in the settlement store ([`crate::settlement`]).
//!
//! The store rules are enforced by `OpsState::apply`, inside the service store's
//! all-or-nothing batches ([`crate::state::State`]).

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

use crate::account::Time;
use crate::authority::AuthorityUsed;
use crate::export::canon::{Canonical, Dec, DecodeError, Enc};
use crate::id::{GrantId, IdentityId, Mint, OperationId};
use crate::leaves::{LeafHost, LeafMap, Leaves, kind};
use crate::lifecycle::{Resolution, State};
use crate::party::{IntegratingParty, PartyId, PartyTie};
use crate::set::NonEmptySet;

/// One operator capability (`REQ-OS-1`, `REQ-OS-2`). Encumbrance is deferred (`DEF-11`, `F-024`);
/// recovery is `F-012`'s, with its own authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OperatorOpKind {
    /// Resolve a `Needs-Review` operation.
    ResolveOperation,
    /// Resume an open operation past its deadline (`F-007` §9).
    Resume,
    /// Propose a reversal.
    ProposeReversal,
    /// Approve a reversal proposed by someone else.
    ApproveReversal,
    /// Freeze an account.
    FreezeAccount,
    /// Unfreeze an account.
    UnfreezeAccount,
    /// Register an asset.
    RegisterAsset,
    /// Deprecate an asset.
    DeprecateAsset,
    /// Grant and revoke issuance authority.
    IssuanceAuthority,
    /// Declare the settlement adapter's supported operations.
    DeclareAdapterSupport,
    /// Read through the privileged-read mechanism (`REQ-PV-4`).
    PrivilegedRead,
    /// Import a whole export into an empty deployment (`F-014`, `REQ-RC-3`).
    ImportBook,
    /// Load opening balances (`F-014`, `REQ-RC-4`).
    LoadOpeningBalances,
    /// Create a checkpoint on demand (`F-017`, `REQ-PR-1`).
    AnchorCheckpoint,
    /// Compare the book with the settlement layer's positions on request (`F-017`, `REQ-PR-5`).
    CompareSettlement,
    /// Issue a disclosure under a legal obligation, without the subject's authorisation, audited
    /// with a justification (`F-017`, `REQ-DS-5`).
    LegalDisclosure,
    /// Read the operational signals: counts and ages, never a ledger fact (`F-018`, `NFR-11`,
    /// `REQ-PV-5`).
    ReadSignals,
    /// Import credentials into an identity's set (`F-028`, `REQ-MG-7`, amendment 0010).
    ImportCredentials,
    /// Open identities: the **identities principal**'s role (`REQ-ID-6`, amendment 0025). Its
    /// holder opens identities, and ledger accounts for any identity, as the operator does. Only
    /// the operator grants it.
    OpenIdentity,
}

impl OperatorOpKind {
    /// Every kind.
    pub const ALL: [OperatorOpKind; 19] = [
        OperatorOpKind::ResolveOperation,
        OperatorOpKind::Resume,
        OperatorOpKind::ProposeReversal,
        OperatorOpKind::ApproveReversal,
        OperatorOpKind::FreezeAccount,
        OperatorOpKind::UnfreezeAccount,
        OperatorOpKind::RegisterAsset,
        OperatorOpKind::DeprecateAsset,
        OperatorOpKind::IssuanceAuthority,
        OperatorOpKind::DeclareAdapterSupport,
        OperatorOpKind::PrivilegedRead,
        OperatorOpKind::ImportBook,
        OperatorOpKind::LoadOpeningBalances,
        OperatorOpKind::AnchorCheckpoint,
        OperatorOpKind::CompareSettlement,
        OperatorOpKind::LegalDisclosure,
        OperatorOpKind::ReadSignals,
        OperatorOpKind::ImportCredentials,
        OperatorOpKind::OpenIdentity,
    ];

    /// A stable one-byte code. Never reorder.
    pub const fn code(self) -> u8 {
        match self {
            OperatorOpKind::ResolveOperation => 1,
            OperatorOpKind::Resume => 2,
            OperatorOpKind::ProposeReversal => 3,
            OperatorOpKind::ApproveReversal => 4,
            OperatorOpKind::FreezeAccount => 5,
            OperatorOpKind::UnfreezeAccount => 6,
            OperatorOpKind::RegisterAsset => 7,
            OperatorOpKind::DeprecateAsset => 8,
            OperatorOpKind::IssuanceAuthority => 9,
            OperatorOpKind::DeclareAdapterSupport => 10,
            OperatorOpKind::PrivilegedRead => 11,
            OperatorOpKind::ImportBook => 12,
            OperatorOpKind::LoadOpeningBalances => 13,
            OperatorOpKind::AnchorCheckpoint => 14,
            OperatorOpKind::CompareSettlement => 15,
            OperatorOpKind::LegalDisclosure => 16,
            OperatorOpKind::ReadSignals => 17,
            OperatorOpKind::ImportCredentials => 18,
            OperatorOpKind::OpenIdentity => 19,
        }
    }
}

/// Why an operator-surface grant cannot be used now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantState {
    /// It can be used.
    Live,
    /// It was revoked (`ERR-AuthorityRevoked`).
    Revoked,
    /// It expired (`ERR-NotAuthorised`).
    Lapsed,
}

/// Operator-surface authority: a principal may run the operator commands of `kinds`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatorGrant {
    id: GrantId,
    minted: Mint,
    grantor: IdentityId,
    principal: IdentityId,
    kinds: NonEmptySet<OperatorOpKind>,
    granted_at: Time,
    expires: Option<Time>,
    revoked: Option<Time>,
}

impl OperatorGrant {
    /// A new grant minted by `minted`.
    pub fn new(
        minted: Mint,
        grantor: IdentityId,
        principal: IdentityId,
        kinds: NonEmptySet<OperatorOpKind>,
        now: Time,
        expires: Option<Time>,
    ) -> OperatorGrant {
        OperatorGrant {
            id: GrantId::mint(&minted.op, minted.ordinal),
            minted,
            grantor,
            principal,
            kinds,
            granted_at: now,
            expires,
            revoked: None,
        }
    }

    /// Its identifier.
    pub const fn id(&self) -> GrantId {
        self.id
    }

    /// The operation that minted it.
    pub const fn minted(&self) -> Mint {
        self.minted
    }

    /// Who granted it.
    pub const fn grantor(&self) -> IdentityId {
        self.grantor
    }

    /// Who holds it.
    pub const fn principal(&self) -> IdentityId {
        self.principal
    }

    /// What it may be used for.
    pub const fn kinds(&self) -> &NonEmptySet<OperatorOpKind> {
        &self.kinds
    }

    /// When it was granted.
    pub const fn granted_at(&self) -> Time {
        self.granted_at
    }

    /// When it stops working.
    pub const fn expires(&self) -> Option<Time> {
        self.expires
    }

    /// When it was revoked.
    pub const fn revoked(&self) -> Option<Time> {
        self.revoked
    }

    /// Whether it can be used at `now`.
    pub fn state(&self, now: Time) -> GrantState {
        if self.revoked.is_some() {
            GrantState::Revoked
        } else if self.expires.is_some_and(|t| now >= t) {
            GrantState::Lapsed
        } else {
            GrantState::Live
        }
    }

    /// The same record under an identifier that is not its minting: something only a raw store
    /// write could attempt, and the store refuses (`INV-17`). Tests and the conformance suite
    /// only (`REQ-CS-5`).
    #[cfg(any(test, feature = "conformance-ports"))]
    pub fn forged(mut self, id: GrantId) -> OperatorGrant {
        self.id = id;
        self
    }
}

/// A proposed reversal (`REQ-RC-2`, `REQ-OS-3`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReversal {
    /// The proposal's own reference: the operation reference of `propose_reversal`.
    pub proposal: OperationId,
    /// The operation reversed.
    pub op: OperationId,
    /// The state it was in when proposed: `Settled`, or `Needs-Review` (Proposal, `F-013`
    /// Open points).
    pub from: State,
    /// The postings reversed, by sequence number (`REQ-PO-3`).
    pub reverses: Vec<u64>,
    /// Who proposed it, under which authority.
    pub proposer: AuthorityUsed,
    /// When.
    pub proposed_at: Time,
    /// Who approved it, and when. Never the proposer.
    pub approval: Option<(AuthorityUsed, Time)>,
}

/// What the settlement adapter found when it verified submitted evidence (`REQ-RC-5`, step 2).
/// Only a finding of no proof is kept: a proof resolves the operation there and then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finding {
    /// The adapter fetched and checked the evidence, and the records it reported prove nothing.
    Unprovable,
}

/// Evidence an authority submitted for a `Needs-Review` operation (`REQ-RC-5`, step 2): the
/// settlement layer's block and transaction the adapter is to fetch and verify on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceSubmission {
    /// The submission's reference: the operation reference of `submit_evidence`.
    pub id: OperationId,
    /// The operation it is for.
    pub op: OperationId,
    /// The settlement layer's block the transaction is in.
    pub block: u64,
    /// The transaction's hash.
    pub transaction: [u8; 32],
    /// Who submitted it, under which authority.
    pub by: AuthorityUsed,
    /// When.
    pub at: Time,
    /// What the adapter found, once it checked.
    pub finding: Option<Finding>,
}

/// A resolution one principal proposed without proof (`REQ-RC-5`, step 3), until a second,
/// distinct principal agrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposedResolution {
    /// What it resolves to.
    pub target: Resolution,
    /// Who proposed it, under which authority.
    pub by: AuthorityUsed,
    /// When.
    pub at: Time,
}

/// A `Needs-Review` operation's resolution case (`REQ-RC-5`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionCase {
    /// The operation.
    pub op: OperationId,
    /// The evidence submitted, in order.
    pub evidence: Vec<EvidenceSubmission>,
    /// A resolution proposed without proof, if one is.
    pub proposed: Option<ProposedResolution>,
}

impl ResolutionCase {
    /// Whether the adapter checked submitted evidence and found no proof: step 3 is open.
    pub fn unprovable(&self) -> bool {
        self.evidence
            .iter()
            .any(|e| e.finding == Some(Finding::Unprovable))
    }
}

/// What an incident is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IncidentKind {
    /// A reversal was proposed (`REQ-RC-2`).
    ReversalProposed,
    /// The audit store's head does not match what the core emitted (`F-013` §3).
    AuditGap,
    /// A `Needs-Review` operation was resolved without mechanical proof, by two principals
    /// (`REQ-RC-5`, step 3).
    ResolvedWithoutProof,
}

/// One item on the operator incident channel. `F-018` pages on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Incident {
    /// Its stable identity, `H(kind, key)`: raising the same incident twice is recognisable.
    pub id: [u8; 32],
    /// What it is about.
    pub kind: IncidentKind,
    /// The operation it concerns, if one.
    pub op: Option<OperationId>,
    /// When it was raised.
    pub at: Time,
}

impl Incident {
    /// The incident of `kind` identified by `key` (a proposal's reference, a sequence number).
    pub fn new(kind: IncidentKind, key: &[u8], op: Option<OperationId>, at: Time) -> Incident {
        let code = match kind {
            IncidentKind::ReversalProposed => 1u8,
            IncidentKind::AuditGap => 2u8,
            IncidentKind::ResolvedWithoutProof => 3u8,
        };
        Incident {
            id: blake2b_256(b"aledgent/incident/v1", &[&[code], key]),
            kind,
            op,
            at,
        }
    }
}

/// One change to the operator records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpsWrite {
    /// Insert an operator-surface grant.
    PutGrant(OperatorGrant),
    /// Revoke one.
    Revoke(GrantId, Time),
    /// Record a proposed reversal.
    Propose(PendingReversal),
    /// Approve a proposed reversal.
    Approve {
        /// The proposal.
        proposal: OperationId,
        /// Who approves.
        approver: AuthorityUsed,
        /// When.
        at: Time,
    },
    /// Raise an incident.
    Incident(Incident),
    /// Record evidence an authority submitted for a `Needs-Review` operation (`REQ-RC-5`).
    SubmitEvidence(EvidenceSubmission),
    /// Record that the adapter checked a submission and found no proof.
    EvidenceFound {
        /// The operation.
        op: OperationId,
        /// The submission.
        id: OperationId,
        /// What it found.
        finding: Finding,
    },
    /// Record a resolution proposed without proof, by its first principal.
    ProposeResolution {
        /// The operation.
        op: OperationId,
        /// The proposal.
        proposed: ProposedResolution,
    },
    /// Register an integrating party (`REQ-ID-7`).
    RegisterParty(IntegratingParty),
    /// Tie a principal to a registered party (`REQ-ID-7`).
    Tie(PartyTie),
}

/// Why the store refused an operator write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpsRefusal {
    /// A grant's identifier is not its minting (`INV-17`).
    NotMinted,
    /// A record with this identifier exists.
    Exists,
    /// The record named does not exist.
    Missing,
    /// A principal or grantor it names does not exist.
    Dangling,
    /// A second revocation, or an approval of an approved proposal.
    Absorbing,
    /// A grant naming every kind: a catch-all (`REQ-OS-1`).
    CatchAll,
    /// The approver is the proposer (`REQ-OS-3`).
    SameApprover,
    /// The principal is tied to a party already (`REQ-ID-7`).
    Tied,
    /// A second live holder of the identities principal's role, or the role in a grant with other
    /// kinds (amendment 0025, `a0025-q1`).
    SecondIdentitiesPrincipal,
}

/// How to undo one operator write.
#[derive(Clone, Debug)]
pub(crate) enum OpsUndo {
    RemoveGrant(GrantId),
    RestoreGrant(OperatorGrant),
    RemoveProposal(OperationId),
    RestoreProposal(PendingReversal),
    RemoveIncident([u8; 32]),
    RestoreCase(OperationId, Option<ResolutionCase>),
    RemoveParty(PartyId),
    Untie(IdentityId),
}

/// The operator records, part of the service store.
///
/// Each collection is leaves of the service tree (`crate::leaves`).
#[derive(Clone, Debug, Default)]
pub struct OpsState {
    grants: Leaves<GrantId, OperatorGrant, { kind::OPERATOR_GRANT }>,
    by_principal: Leaves<IdentityId, BTreeSet<GrantId>, { kind::OPERATOR_GRANTS_BY_PRINCIPAL }>,
    reversals: Leaves<OperationId, PendingReversal, { kind::REVERSAL }>,
    incidents: Leaves<[u8; 32], Incident, { kind::OPERATOR_INCIDENT }>,
    resolutions: Leaves<OperationId, ResolutionCase, { kind::RESOLUTION_CASE }>,
    parties: Leaves<PartyId, IntegratingParty, { kind::INTEGRATING_PARTY }>,
    /// Each principal's tie, by principal: at most one, so the key is the principal.
    ties: Leaves<IdentityId, PartyTie, { kind::PARTY_TIE }>,
}

impl LeafHost for OpsState {
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>) {
        out.push(&self.grants);
        out.push(&self.by_principal);
        out.push(&self.reversals);
        out.push(&self.incidents);
        out.push(&self.resolutions);
        out.push(&self.parties);
        out.push(&self.ties);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.grants);
        out.push(&mut self.by_principal);
        out.push(&mut self.reversals);
        out.push(&mut self.incidents);
        out.push(&mut self.resolutions);
        out.push(&mut self.parties);
        out.push(&mut self.ties);
    }
}

/// An operator-surface grant as a leaf: its terms and when it was revoked; its identifier is its
/// minting's.
impl Canonical for OperatorGrant {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.minted)
            .put(&self.grantor)
            .put(&self.principal)
            .put(&self.kinds)
            .put(&self.granted_at)
            .put(&self.expires)
            .put(&self.revoked);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let minted: Mint = d.get()?;
        Ok(OperatorGrant {
            id: GrantId::mint(&minted.op, minted.ordinal),
            minted,
            grantor: d.get()?,
            principal: d.get()?,
            kinds: d.get()?,
            granted_at: d.get()?,
            expires: d.get()?,
            revoked: d.get()?,
        })
    }
}

impl OpsState {
    /// Empty records.
    pub fn new() -> OpsState {
        OpsState::default()
    }

    /// Apply one write under the rules. `known` says whether an identity exists; `taken` whether
    /// a grant identifier is used by another kind of grant.
    pub(crate) fn apply(
        &mut self,
        w: &OpsWrite,
        known: impl Fn(&IdentityId) -> bool,
        taken: impl Fn(&GrantId) -> bool,
    ) -> Result<OpsUndo, OpsRefusal> {
        match w {
            OpsWrite::PutGrant(g) => {
                if g.id != GrantId::mint(&g.minted.op, g.minted.ordinal) {
                    return Err(OpsRefusal::NotMinted);
                }
                if self.grants.contains_key(&g.id) || taken(&g.id) {
                    return Err(OpsRefusal::Exists);
                }
                if !known(&g.principal) || !known(&g.grantor) {
                    return Err(OpsRefusal::Dangling);
                }
                if g.revoked.is_some() {
                    return Err(OpsRefusal::Absorbing);
                }
                if g.kinds.len() >= OperatorOpKind::ALL.len() {
                    return Err(OpsRefusal::CatchAll);
                }
                if g.kinds.contains(&OperatorOpKind::OpenIdentity)
                    && (g.kinds.len() != 1
                        || self.grants.values().any(|h| {
                            h.revoked.is_none() && h.kinds.contains(&OperatorOpKind::OpenIdentity)
                        }))
                {
                    return Err(OpsRefusal::SecondIdentitiesPrincipal);
                }
                self.by_principal.entry_or_default(g.principal).insert(g.id);
                self.grants.insert(g.id, g.clone());
                Ok(OpsUndo::RemoveGrant(g.id))
            }
            OpsWrite::Revoke(id, at) => {
                let before = self.grants.get(id).ok_or(OpsRefusal::Missing)?.clone();
                if before.revoked.is_some() {
                    return Err(OpsRefusal::Absorbing);
                }
                let mut after = before.clone();
                after.revoked = Some(*at);
                self.grants.insert(*id, after);
                Ok(OpsUndo::RestoreGrant(before))
            }
            OpsWrite::Propose(p) => {
                if self.reversals.contains_key(&p.proposal) {
                    return Err(OpsRefusal::Exists);
                }
                if p.approval.is_some() {
                    return Err(OpsRefusal::Absorbing);
                }
                if !known(&p.proposer.principal) {
                    return Err(OpsRefusal::Dangling);
                }
                self.reversals.insert(p.proposal, p.clone());
                Ok(OpsUndo::RemoveProposal(p.proposal))
            }
            OpsWrite::Approve {
                proposal,
                approver,
                at,
            } => {
                let before = self
                    .reversals
                    .get(proposal)
                    .ok_or(OpsRefusal::Missing)?
                    .clone();
                if before.approval.is_some() {
                    return Err(OpsRefusal::Absorbing);
                }
                if approver.principal == before.proposer.principal {
                    return Err(OpsRefusal::SameApprover);
                }
                if !known(&approver.principal) {
                    return Err(OpsRefusal::Dangling);
                }
                let mut after = before.clone();
                after.approval = Some((*approver, *at));
                self.reversals.insert(*proposal, after);
                Ok(OpsUndo::RestoreProposal(before))
            }
            OpsWrite::Incident(i) => {
                if self.incidents.contains_key(&i.id) {
                    return Err(OpsRefusal::Exists);
                }
                self.incidents.insert(i.id, *i);
                Ok(OpsUndo::RemoveIncident(i.id))
            }
            OpsWrite::SubmitEvidence(e) => {
                if !known(&e.by.principal) {
                    return Err(OpsRefusal::Dangling);
                }
                if e.finding.is_some() {
                    return Err(OpsRefusal::Absorbing);
                }
                let before = self.resolutions.get(&e.op).cloned();
                let mut case = before.clone().unwrap_or(ResolutionCase {
                    op: e.op,
                    evidence: Vec::new(),
                    proposed: None,
                });
                if case.evidence.iter().any(|x| x.id == e.id) {
                    return Err(OpsRefusal::Exists);
                }
                case.evidence.push(e.clone());
                self.resolutions.insert(e.op, case);
                Ok(OpsUndo::RestoreCase(e.op, before))
            }
            OpsWrite::EvidenceFound { op, id, finding } => {
                let before = self
                    .resolutions
                    .get(op)
                    .cloned()
                    .ok_or(OpsRefusal::Missing)?;
                let mut case = before.clone();
                let e = case
                    .evidence
                    .iter_mut()
                    .find(|x| x.id == *id)
                    .ok_or(OpsRefusal::Missing)?;
                if e.finding.is_some() {
                    return Err(OpsRefusal::Absorbing);
                }
                e.finding = Some(*finding);
                self.resolutions.insert(*op, case);
                Ok(OpsUndo::RestoreCase(*op, Some(before)))
            }
            OpsWrite::ProposeResolution { op, proposed } => {
                if !known(&proposed.by.principal) {
                    return Err(OpsRefusal::Dangling);
                }
                let before = self
                    .resolutions
                    .get(op)
                    .cloned()
                    .ok_or(OpsRefusal::Missing)?;
                if before.proposed.is_some() {
                    return Err(OpsRefusal::Exists);
                }
                let mut case = before.clone();
                case.proposed = Some(*proposed);
                self.resolutions.insert(*op, case);
                Ok(OpsUndo::RestoreCase(*op, Some(before)))
            }
            OpsWrite::RegisterParty(p) => {
                if self.parties.contains_key(&p.party) {
                    return Err(OpsRefusal::Exists);
                }
                if !known(&p.party.identity()) {
                    return Err(OpsRefusal::Dangling);
                }
                self.parties.insert(p.party, *p);
                Ok(OpsUndo::RemoveParty(p.party))
            }
            OpsWrite::Tie(t) => {
                if !self.parties.contains_key(&t.party) {
                    return Err(OpsRefusal::Missing);
                }
                if !known(&t.principal) {
                    return Err(OpsRefusal::Dangling);
                }
                if self.ties.contains_key(&t.principal) {
                    return Err(OpsRefusal::Tied);
                }
                self.ties.insert(t.principal, *t);
                Ok(OpsUndo::Untie(t.principal))
            }
        }
    }

    pub(crate) fn undo(&mut self, u: OpsUndo) {
        match u {
            OpsUndo::RemoveGrant(id) => {
                if let Some(g) = self.grants.remove(&id) {
                    self.by_principal.remove_from(&g.principal, &id);
                }
            }
            OpsUndo::RestoreGrant(g) => {
                self.grants.insert(g.id, g);
            }
            OpsUndo::RemoveProposal(id) => {
                self.reversals.remove(&id);
            }
            OpsUndo::RestoreProposal(p) => {
                self.reversals.insert(p.proposal, p);
            }
            OpsUndo::RemoveIncident(id) => {
                self.incidents.remove(&id);
            }
            OpsUndo::RestoreCase(op, before) => match before {
                Some(c) => {
                    self.resolutions.insert(op, c);
                }
                None => {
                    self.resolutions.remove(&op);
                }
            },
            OpsUndo::RemoveParty(p) => {
                self.parties.remove(&p);
            }
            OpsUndo::Untie(p) => {
                self.ties.remove(&p);
            }
        }
    }

    /// An operator-surface grant.
    pub fn grant(&self, id: &GrantId) -> Option<&OperatorGrant> {
        self.grants.get(id)
    }

    /// Every operator-surface grant, by id.
    pub fn grants(&self) -> impl Iterator<Item = &OperatorGrant> {
        self.grants.values()
    }

    /// Every operator-surface grant a principal holds.
    pub fn grants_of(&self, principal: &IdentityId) -> impl Iterator<Item = &OperatorGrant> {
        self.by_principal
            .get(principal)
            .into_iter()
            .flatten()
            .filter_map(|id| self.grants.get(id))
    }

    /// A proposed reversal.
    pub fn reversal(&self, proposal: &OperationId) -> Option<&PendingReversal> {
        self.reversals.get(proposal)
    }

    /// Every proposed reversal, approved or not.
    pub fn reversals(&self) -> impl Iterator<Item = &PendingReversal> {
        self.reversals.values()
    }

    /// Every incident raised.
    pub fn incidents(&self) -> impl Iterator<Item = &Incident> {
        self.incidents.values()
    }

    /// A `Needs-Review` operation's resolution case, if evidence was submitted for it.
    pub fn resolution(&self, op: &OperationId) -> Option<&ResolutionCase> {
        self.resolutions.get(op)
    }

    /// Every resolution case.
    pub fn resolutions(&self) -> impl Iterator<Item = &ResolutionCase> {
        self.resolutions.values()
    }

    /// A registered integrating party (`REQ-ID-7`).
    pub fn party(&self, party: &PartyId) -> Option<&IntegratingParty> {
        self.parties.get(party)
    }

    /// Every registered integrating party, by identifier.
    pub fn parties(&self) -> impl Iterator<Item = &IntegratingParty> {
        self.parties.values()
    }

    /// The tie of a principal to its integrating party, if it has one (`REQ-ID-7`).
    pub fn tie(&self, principal: &IdentityId) -> Option<&PartyTie> {
        self.ties.get(principal)
    }

    /// Every tie, by principal.
    pub fn ties(&self) -> impl Iterator<Item = &PartyTie> {
        self.ties.values()
    }
}
