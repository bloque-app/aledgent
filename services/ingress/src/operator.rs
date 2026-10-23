// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Operator commands (`CTR-OPS`, `F-013`): validated into `ledger-core`'s types and routed. Their
//! semantics — authority, audit, what each may change — are `F-013`'s; ingress takes them in on
//! the operator surface (their client keys are a domain of their own, `DEP-10`), logs them like any
//! command, and answers with the core's decision.
//!
//! Not routed: the privileged read (`REQ-PV-4`), which needs the audit sink the core holds and
//! has no owning feature yet.

use std::collections::BTreeSet;

use ledger_core::Refusal;
use ledger_core::account::Time;
use ledger_core::asset::{AssetId, AssetKind, AssetLocation};
use ledger_core::authority::OperationKind;
use ledger_core::id::{AccountId, GrantId, IdentityId, OperationId};
use ledger_core::ledger::GrantRequest;
use ledger_core::ledger::operator::{
    RegisterAsset, ResolutionEvidence, Resolve, ReviewRef, SubmitEvidence,
};
use ledger_core::lifecycle::Resolution;
use ledger_core::operator::OperatorOpKind;
use ledger_core::set::NonEmptySet;
use ledger_core::settlement::Coordinate;

use crate::command::{self, op_kind};
use crate::custody;
use crate::wire::{
    AdapterLegBody, AdapterSupportBody, CoordinateBody, GrantIssuanceBody, GrantOperatorBody,
    OperatorCommand, OperatorOpKindBody, RegisterAssetBody, ResolutionBody, ResolutionEvidenceBody,
};
use ledger_core::custody::{PendingId, Signature};
use ledger_core::ledger::custody::{ImportCredentials, ImportedCredential, InitiateRecovery};
use ledger_core::party::{PartyId, RegisterIntegratingParty};

/// An operator command, validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperatorOp {
    /// Freeze (`true`) or unfreeze an account.
    Freeze {
        /// The account.
        account: AccountId,
        /// Freeze, or unfreeze.
        frozen: bool,
    },
    /// Resume an open operation.
    Resume(OperationId),
    /// Resolve a `Needs-Review` operation.
    Resolve(Resolve),
    /// Submit evidence for a `Needs-Review` operation (`REQ-RC-5`, step 2).
    SubmitEvidence(SubmitEvidence),
    /// Propose a reversal.
    ProposeReversal(OperationId),
    /// Approve a proposal.
    ApproveReversal(OperationId),
    /// Register an asset.
    RegisterAsset(RegisterAsset),
    /// Deprecate an asset.
    DeprecateAsset(AssetId, ReviewRef),
    /// `bind_asset` (amendment 0011, `REQ-AS-8`).
    BindAsset(AssetId, AssetLocation),
    /// Grant issuance authority.
    GrantIssuance(GrantRequest),
    /// Revoke issuance authority.
    RevokeIssuance(GrantId),
    /// Grant operator authority.
    GrantOperator {
        /// Who.
        principal: IdentityId,
        /// Which capabilities.
        kinds: NonEmptySet<OperatorOpKind>,
        /// Until when.
        expires: Option<Time>,
    },
    /// Revoke operator authority.
    RevokeOperator(GrantId),
    /// Declare the adapter's support.
    DeclareAdapterSupport(BTreeSet<(OperationKind, AssetId)>),
    /// Initiate a recovery (`F-012`).
    InitiateRecovery(InitiateRecovery),
    /// Complete a pending recovery with the recovery authority's approval (`F-012`).
    CompleteRecovery(PendingId, Signature),
    /// Import credentials into an identity's set (`F-028`, `REQ-MG-7`).
    ImportCredentials(ImportCredentials),
    /// Register an integrating party (`REQ-ID-7`).
    RegisterIntegratingParty(RegisterIntegratingParty),
    /// Tie a principal to a registered party (`REQ-ID-7`).
    TiePrincipal {
        /// The party.
        party: PartyId,
        /// The principal.
        principal: IdentityId,
    },
    /// Name an account by an alias (`REQ-RC-7`, amendment 0024).
    RegisterAlias {
        /// The account.
        account: AccountId,
        /// The source.
        source: ledger_core::opening::OpeningSource,
        /// The name within it.
        name: String,
    },
    /// The writer's code becomes `code` from input `from` (`REQ-RT-16`, `T-037-04`).
    Upgrade {
        /// The code version.
        code: [u8; 32],
        /// The input it applies from.
        from: u64,
    },
    /// Move the scheduled upgrade to input `from` (`REQ-RT-20`, `T-037-15`).
    RescheduleUpgrade {
        /// The input it now applies from.
        from: u64,
    },
    /// Cancel the scheduled upgrade (`REQ-RT-20`, `T-037-15`).
    CancelUpgrade,
}

impl OperatorOp {
    /// Which input of the writer's code this is, if it is one (`T-037-04`): the door stages the
    /// code of an upgrade, and gives a way out priority, only to a caller the core says may.
    pub fn code_input(&self) -> Option<ledger_core::ledger::operator::CodeInput> {
        use ledger_core::ledger::operator::CodeInput;
        match self {
            OperatorOp::Upgrade { .. } => Some(CodeInput::Upgrade),
            OperatorOp::RescheduleUpgrade { .. } | OperatorOp::CancelUpgrade => {
                Some(CodeInput::WayOut)
            }
            _ => None,
        }
    }
}

/// An alias's source as the wire names it: 16 bytes as 32 hex digits, or a name of at most 16
/// ASCII bytes, zero-padded (the migration's `system-role`). Anything else is
/// `ERR-MalformedCommand`.
pub fn alias_source(s: &str) -> Result<ledger_core::opening::OpeningSource, Refusal> {
    if s.len() == 32
        && let Some(b) = hex::decode(s)
            .ok()
            .and_then(|v| <[u8; 16]>::try_from(v).ok())
    {
        return Ok(ledger_core::opening::OpeningSource(b));
    }
    if s.is_empty() || s.len() > 16 || !s.bytes().all(|c| c.is_ascii_graphic()) {
        return Err(Refusal::MalformedCommand);
    }
    let mut b = [0u8; 16];
    b[..s.len()].copy_from_slice(s.as_bytes());
    Ok(ledger_core::opening::OpeningSource(b))
}

/// An alias's name: at most [`ledger_core::ledger::operator::MAX_ALIAS_REFERENCE`] bytes, not
/// empty.
pub fn alias_name(s: &str) -> Result<String, Refusal> {
    if s.is_empty() || s.len() > ledger_core::ledger::operator::MAX_ALIAS_REFERENCE {
        return Err(Refusal::MalformedCommand);
    }
    Ok(s.to_owned())
}

/// A validated operator command and its justification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Justified {
    /// The command.
    pub op: OperatorOp,
    /// Why.
    pub justification: Vec<u8>,
}

fn account(s: &str) -> Result<AccountId, Refusal> {
    AccountId::parse(s).map_err(|_| Refusal::UnknownAccount)
}

fn operation(s: &str) -> Result<OperationId, Refusal> {
    OperationId::parse(s).map_err(|_| Refusal::UnknownOperation)
}

/// An integrating party's identifier: its organisation identity's (`REQ-ID-7`).
fn party(s: &str) -> Result<PartyId, Refusal> {
    PartyId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

fn grant(s: &str) -> Result<GrantId, Refusal> {
    GrantId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

fn review(s: &str) -> Result<ReviewRef, Refusal> {
    hex::decode(s)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .map(ReviewRef)
        .ok_or(Refusal::MalformedCommand)
}

fn asset_kind(s: &str) -> Result<AssetKind, Refusal> {
    Ok(match s {
        "fiat" => AssetKind::Fiat,
        "stable" => AssetKind::Stable,
        "native" => AssetKind::Native,
        "other" => AssetKind::Other,
        _ => return Err(Refusal::MalformedCommand),
    })
}

fn asset_kind_name(k: AssetKind) -> &'static str {
    match k {
        AssetKind::Fiat => "fiat",
        AssetKind::Stable => "stable",
        AssetKind::Native => "native",
        AssetKind::Other => "other",
    }
}

const KINDS: [(OperatorOpKindBody, OperatorOpKind); 19] = [
    (
        OperatorOpKindBody::ResolveOperation,
        OperatorOpKind::ResolveOperation,
    ),
    (OperatorOpKindBody::Resume, OperatorOpKind::Resume),
    (
        OperatorOpKindBody::ProposeReversal,
        OperatorOpKind::ProposeReversal,
    ),
    (
        OperatorOpKindBody::ApproveReversal,
        OperatorOpKind::ApproveReversal,
    ),
    (
        OperatorOpKindBody::FreezeAccount,
        OperatorOpKind::FreezeAccount,
    ),
    (
        OperatorOpKindBody::UnfreezeAccount,
        OperatorOpKind::UnfreezeAccount,
    ),
    (
        OperatorOpKindBody::RegisterAsset,
        OperatorOpKind::RegisterAsset,
    ),
    (
        OperatorOpKindBody::DeprecateAsset,
        OperatorOpKind::DeprecateAsset,
    ),
    (
        OperatorOpKindBody::IssuanceAuthority,
        OperatorOpKind::IssuanceAuthority,
    ),
    (
        OperatorOpKindBody::DeclareAdapterSupport,
        OperatorOpKind::DeclareAdapterSupport,
    ),
    (
        OperatorOpKindBody::PrivilegedRead,
        OperatorOpKind::PrivilegedRead,
    ),
    (OperatorOpKindBody::ImportBook, OperatorOpKind::ImportBook),
    (
        OperatorOpKindBody::LoadOpeningBalances,
        OperatorOpKind::LoadOpeningBalances,
    ),
    (
        OperatorOpKindBody::AnchorCheckpoint,
        OperatorOpKind::AnchorCheckpoint,
    ),
    (
        OperatorOpKindBody::CompareSettlement,
        OperatorOpKind::CompareSettlement,
    ),
    (
        OperatorOpKindBody::LegalDisclosure,
        OperatorOpKind::LegalDisclosure,
    ),
    (OperatorOpKindBody::ReadSignals, OperatorOpKind::ReadSignals),
    (
        OperatorOpKindBody::ImportCredentials,
        OperatorOpKind::ImportCredentials,
    ),
    (
        OperatorOpKindBody::OpenIdentity,
        OperatorOpKind::OpenIdentity,
    ),
];

fn kind_of(b: OperatorOpKindBody) -> OperatorOpKind {
    KINDS
        .iter()
        .find(|(w, _)| *w == b)
        .map_or(OperatorOpKind::Resume, |(_, k)| *k)
}

fn kind_body(k: OperatorOpKind) -> OperatorOpKindBody {
    KINDS
        .iter()
        .find(|(_, c)| *c == k)
        .map_or(OperatorOpKindBody::Resume, |(w, _)| *w)
}

impl Justified {
    /// Validate a wire operator command.
    pub fn parse(c: &OperatorCommand) -> Result<Justified, Refusal> {
        let j = |s: &String| s.as_bytes().to_vec();
        let (op, justification) = match c {
            OperatorCommand::FreezeAccount {
                account: a,
                justification,
            } => (
                OperatorOp::Freeze {
                    account: account(a)?,
                    frozen: true,
                },
                j(justification),
            ),
            OperatorCommand::UnfreezeAccount {
                account: a,
                justification,
            } => (
                OperatorOp::Freeze {
                    account: account(a)?,
                    frozen: false,
                },
                j(justification),
            ),
            OperatorCommand::Resume {
                operation: o,
                justification,
            } => (OperatorOp::Resume(operation(o)?), j(justification)),
            OperatorCommand::Resolve {
                operation: o,
                target,
                evidence,
                justification,
            } => (
                OperatorOp::Resolve(Resolve {
                    op: operation(o)?,
                    target: match target {
                        ResolutionBody::Settle => Resolution::Settle,
                        ResolutionBody::Release => Resolution::Release,
                    },
                    evidence: match evidence {
                        ResolutionEvidenceBody::NonInclusion => ResolutionEvidence::NonInclusion,
                        ResolutionEvidenceBody::NoProof => ResolutionEvidence::NoProof,
                        ResolutionEvidenceBody::Record(c) => {
                            ResolutionEvidence::Record(Coordinate {
                                instance: c.instance,
                                seq: c.seq,
                                digest: hex::decode(&c.digest)
                                    .ok()
                                    .and_then(|v| <[u8; 32]>::try_from(v).ok())
                                    .ok_or(Refusal::EvidenceInsufficient)?,
                            })
                        }
                    },
                }),
                j(justification),
            ),
            OperatorCommand::SubmitEvidence {
                operation: o,
                block,
                transaction,
                justification,
            } => (
                OperatorOp::SubmitEvidence(SubmitEvidence {
                    op: operation(o)?,
                    block: *block,
                    transaction: hex::decode(transaction)
                        .ok()
                        .and_then(|v| <[u8; 32]>::try_from(v).ok())
                        .ok_or(Refusal::EvidenceInsufficient)?,
                }),
                j(justification),
            ),
            OperatorCommand::ProposeReversal {
                operation: o,
                justification,
            } => (OperatorOp::ProposeReversal(operation(o)?), j(justification)),
            OperatorCommand::ApproveReversal {
                proposal,
                justification,
            } => (
                OperatorOp::ApproveReversal(operation(proposal)?),
                j(justification),
            ),
            OperatorCommand::RegisterAsset(b) => (
                OperatorOp::RegisterAsset(RegisterAsset {
                    id: AssetId::parse(&b.id)?,
                    precision: b.precision,
                    kind: asset_kind(&b.kind)?,
                    review: review(&b.review)?,
                }),
                j(&b.justification),
            ),
            OperatorCommand::DeprecateAsset {
                asset,
                review: r,
                justification,
            } => (
                OperatorOp::DeprecateAsset(AssetId::parse(asset)?, review(r)?),
                j(justification),
            ),
            OperatorCommand::BindAsset {
                asset,
                location,
                justification,
            } => (
                OperatorOp::BindAsset(AssetId::parse(asset)?, AssetLocation::parse(location)?),
                j(justification),
            ),
            OperatorCommand::GrantIssuance(b) => {
                let parsed = command::Command::parse(&crate::wire::CommandBody::GrantAuthority(
                    b.grant.clone(),
                ))?;
                let command::Command::GrantAuthority(g) = parsed else {
                    return Err(Refusal::NotAuthorised);
                };
                (OperatorOp::GrantIssuance(g), j(&b.justification))
            }
            OperatorCommand::RevokeIssuance {
                grant: g,
                justification,
            } => (OperatorOp::RevokeIssuance(grant(g)?), j(justification)),
            OperatorCommand::GrantOperator(b) => (
                OperatorOp::GrantOperator {
                    principal: command::identity_id(&b.principal)?,
                    kinds: NonEmptySet::new(b.kinds.iter().copied().map(kind_of).collect())
                        .ok_or(Refusal::MalformedCommand)?,
                    expires: b.expires,
                },
                j(&b.justification),
            ),
            OperatorCommand::RevokeOperator {
                grant: g,
                justification,
            } => (OperatorOp::RevokeOperator(grant(g)?), j(justification)),
            OperatorCommand::DeclareAdapterSupport(b) => {
                let mut legs = BTreeSet::new();
                for l in &b.legs {
                    legs.insert((op_kind(l.kind), AssetId::parse(&l.asset)?));
                }
                (OperatorOp::DeclareAdapterSupport(legs), j(&b.justification))
            }
            OperatorCommand::InitiateRecovery {
                account: a,
                verifier,
                scheme,
                scope,
                justification,
            } => (
                OperatorOp::InitiateRecovery(custody::initiate(
                    a,
                    scheme.as_deref(),
                    verifier,
                    scope.as_ref(),
                )?),
                j(justification),
            ),
            OperatorCommand::ImportCredentials {
                identity,
                credentials,
                justification,
            } => (
                OperatorOp::ImportCredentials(ImportCredentials {
                    identity: custody::identity(identity)?,
                    credentials: credentials
                        .iter()
                        .map(|c| {
                            Ok(ImportedCredential {
                                verifier: custody::verifier_of(Some(&c.scheme), &c.verifier)?,
                                scope: custody::scope(c.scope.as_ref())?,
                            })
                        })
                        .collect::<Result<_, Refusal>>()?,
                }),
                j(justification),
            ),
            OperatorCommand::RegisterIntegratingParty {
                party: p,
                principals,
                justification,
            } => {
                let mut set = BTreeSet::new();
                for s in principals {
                    // A principal named twice is a malformed request, not a set.
                    if !set.insert(command::identity_id(s)?) {
                        return Err(Refusal::MalformedCommand);
                    }
                }
                (
                    OperatorOp::RegisterIntegratingParty(RegisterIntegratingParty {
                        party: party(p)?,
                        principals: set,
                    }),
                    j(justification),
                )
            }
            OperatorCommand::TiePrincipal {
                party: p,
                principal,
                justification,
            } => (
                OperatorOp::TiePrincipal {
                    party: party(p)?,
                    principal: command::identity_id(principal)?,
                },
                j(justification),
            ),
            OperatorCommand::RegisterAlias {
                account: a,
                source,
                name,
                justification,
            } => (
                OperatorOp::RegisterAlias {
                    account: account(a)?,
                    source: alias_source(source)?,
                    name: alias_name(name)?,
                },
                j(justification),
            ),
            OperatorCommand::CompleteRecovery {
                pending,
                approval,
                justification,
            } => (
                OperatorOp::CompleteRecovery(
                    custody::pending(pending)?,
                    custody::signature(approval, Refusal::MalformedCommand)?,
                ),
                j(justification),
            ),
            OperatorCommand::Upgrade {
                code,
                from,
                justification,
            } => (
                OperatorOp::Upgrade {
                    code: review(code)?.0,
                    from: *from,
                },
                j(justification),
            ),
            OperatorCommand::RescheduleUpgrade {
                from,
                justification,
            } => (
                OperatorOp::RescheduleUpgrade { from: *from },
                j(justification),
            ),
            OperatorCommand::CancelUpgrade { justification } => {
                (OperatorOp::CancelUpgrade, j(justification))
            }
        };
        Ok(Justified { op, justification })
    }

    /// The canonical wire form.
    pub fn to_body(&self) -> OperatorCommand {
        let justification = String::from_utf8_lossy(&self.justification).into_owned();
        match &self.op {
            OperatorOp::Freeze {
                account,
                frozen: true,
            } => OperatorCommand::FreezeAccount {
                account: account.to_did(),
                justification,
            },
            OperatorOp::Freeze {
                account,
                frozen: false,
            } => OperatorCommand::UnfreezeAccount {
                account: account.to_did(),
                justification,
            },
            OperatorOp::Resume(o) => OperatorCommand::Resume {
                operation: o.to_did(),
                justification,
            },
            OperatorOp::Resolve(r) => OperatorCommand::Resolve {
                operation: r.op.to_did(),
                target: match r.target {
                    Resolution::Settle => ResolutionBody::Settle,
                    Resolution::Release => ResolutionBody::Release,
                },
                evidence: match &r.evidence {
                    ResolutionEvidence::NonInclusion => ResolutionEvidenceBody::NonInclusion,
                    ResolutionEvidence::NoProof => ResolutionEvidenceBody::NoProof,
                    ResolutionEvidence::Record(c) => {
                        ResolutionEvidenceBody::Record(CoordinateBody {
                            instance: c.instance,
                            seq: c.seq,
                            digest: hex::encode(c.digest),
                        })
                    }
                },
                justification,
            },
            OperatorOp::SubmitEvidence(e) => OperatorCommand::SubmitEvidence {
                operation: e.op.to_did(),
                block: e.block,
                transaction: hex::encode(e.transaction),
                justification,
            },
            OperatorOp::ProposeReversal(o) => OperatorCommand::ProposeReversal {
                operation: o.to_did(),
                justification,
            },
            OperatorOp::ApproveReversal(p) => OperatorCommand::ApproveReversal {
                proposal: p.to_did(),
                justification,
            },
            OperatorOp::RegisterAsset(r) => OperatorCommand::RegisterAsset(RegisterAssetBody {
                id: r.id.as_str().to_owned(),
                precision: r.precision,
                kind: asset_kind_name(r.kind).to_owned(),
                review: hex::encode(r.review.0),
                justification,
            }),
            OperatorOp::DeprecateAsset(a, r) => OperatorCommand::DeprecateAsset {
                asset: a.as_str().to_owned(),
                review: hex::encode(r.0),
                justification,
            },
            OperatorOp::BindAsset(a, l) => OperatorCommand::BindAsset {
                asset: a.as_str().to_owned(),
                location: l.as_str().to_owned(),
                justification,
            },
            OperatorOp::GrantIssuance(g) => {
                let body = command::Command::GrantAuthority(g.clone()).to_body();
                let crate::wire::CommandBody::GrantAuthority(grant) = body else {
                    unreachable!("a grant renders as a grant")
                };
                OperatorCommand::GrantIssuance(GrantIssuanceBody {
                    grant,
                    justification,
                })
            }
            OperatorOp::RevokeIssuance(g) => OperatorCommand::RevokeIssuance {
                grant: g.to_did(),
                justification,
            },
            OperatorOp::GrantOperator {
                principal,
                kinds,
                expires,
            } => OperatorCommand::GrantOperator(GrantOperatorBody {
                principal: principal.to_did(),
                kinds: kinds.iter().copied().map(kind_body).collect(),
                expires: *expires,
                justification,
            }),
            OperatorOp::RevokeOperator(g) => OperatorCommand::RevokeOperator {
                grant: g.to_did(),
                justification,
            },
            OperatorOp::DeclareAdapterSupport(legs) => {
                OperatorCommand::DeclareAdapterSupport(AdapterSupportBody {
                    legs: legs
                        .iter()
                        .map(|(k, a)| AdapterLegBody {
                            kind: command::op_kind_body(*k),
                            asset: a.as_str().to_owned(),
                        })
                        .collect(),
                    justification,
                })
            }
            OperatorOp::InitiateRecovery(r) => OperatorCommand::InitiateRecovery {
                account: r.account.to_did(),
                verifier: hex::encode(r.verifier.as_bytes()),
                scheme: custody::scheme_body(&r.verifier),
                scope: custody::scope_body(r.scope.as_ref()),
                justification,
            },
            OperatorOp::ImportCredentials(r) => OperatorCommand::ImportCredentials {
                identity: r.identity.to_did(),
                credentials: r
                    .credentials
                    .iter()
                    .map(|c| crate::wire::VerifierBody {
                        scheme: c.verifier.scheme().name().to_owned(),
                        verifier: hex::encode(c.verifier.as_bytes()),
                        scope: custody::scope_body(c.scope.as_ref()),
                    })
                    .collect(),
                justification,
            },
            OperatorOp::RegisterIntegratingParty(r) => OperatorCommand::RegisterIntegratingParty {
                party: r.party.to_did(),
                principals: r.principals.iter().map(|p| p.to_did()).collect(),
                justification,
            },
            OperatorOp::TiePrincipal { party, principal } => OperatorCommand::TiePrincipal {
                party: party.to_did(),
                principal: principal.to_did(),
                justification,
            },
            OperatorOp::RegisterAlias {
                account,
                source,
                name,
            } => OperatorCommand::RegisterAlias {
                account: account.to_did(),
                source: hex::encode(source.0),
                name: name.clone(),
                justification,
            },
            OperatorOp::CompleteRecovery(p, sig) => OperatorCommand::CompleteRecovery {
                pending: p.0.to_did(),
                approval: custody::signature_hex(sig),
                justification,
            },
            OperatorOp::Upgrade { code, from } => OperatorCommand::Upgrade {
                code: hex::encode(code),
                from: *from,
                justification,
            },
            OperatorOp::RescheduleUpgrade { from } => OperatorCommand::RescheduleUpgrade {
                from: *from,
                justification,
            },
            OperatorOp::CancelUpgrade => OperatorCommand::CancelUpgrade { justification },
        }
    }

    /// The operation the reply names: the one it acts on, or the minted reference for a command
    /// that makes something new.
    pub fn operation(&self, minted: OperationId) -> OperationId {
        match &self.op {
            OperatorOp::Resume(o) | OperatorOp::ProposeReversal(o) => *o,
            OperatorOp::Resolve(r) => r.op,
            _ => minted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operator_capability_has_a_wire_name() {
        assert_eq!(KINDS.len(), OperatorOpKind::ALL.len());
        for k in OperatorOpKind::ALL {
            assert_eq!(kind_of(kind_body(k)), k);
        }
    }

    #[test]
    fn an_operator_command_round_trips() {
        let c = OperatorCommand::Resolve {
            operation: OperationId::from_ref(ledger_kernel::posting::OperationRef([4; 16]))
                .to_did(),
            target: ResolutionBody::Release,
            evidence: ResolutionEvidenceBody::Record(CoordinateBody {
                instance: 1,
                seq: 2,
                digest: "ab".repeat(32),
            }),
            justification: "reviewed".into(),
        };
        let j = Justified::parse(&c).unwrap();
        assert_eq!(j.to_body(), c);
        let json = serde_json::to_value(crate::wire::CommandBody::Operator(c.clone())).unwrap();
        assert_eq!(json["command"], "operator");
        assert_eq!(json["op"], "resolve");
        assert_eq!(
            serde_json::from_value::<crate::wire::CommandBody>(json).unwrap(),
            crate::wire::CommandBody::Operator(c)
        );
    }

    /// `REQ-RC-5`: evidence for the settlement adapter to verify, and a release without proof
    /// (step 3), both round-trip through the canonical form.
    #[test]
    fn evidence_and_a_release_without_proof_round_trip() {
        let operation =
            OperationId::from_ref(ledger_kernel::posting::OperationRef([5; 16])).to_did();
        for c in [
            OperatorCommand::SubmitEvidence {
                operation: operation.clone(),
                block: 77,
                transaction: "cd".repeat(32),
                justification: "block 77 shows it".into(),
            },
            OperatorCommand::Resolve {
                operation: operation.clone(),
                target: ResolutionBody::Release,
                evidence: ResolutionEvidenceBody::NoProof,
                justification: "no payout, agreed".into(),
            },
        ] {
            let j = Justified::parse(&c).unwrap();
            assert_eq!(j.to_body(), c);
        }
        // A transaction hash that is not 32 bytes is refused.
        let bad = OperatorCommand::SubmitEvidence {
            operation,
            block: 1,
            transaction: "cd".into(),
            justification: "x".into(),
        };
        assert_eq!(
            Justified::parse(&bad).map(|_| ()),
            Err(Refusal::EvidenceInsufficient)
        );
    }
}
