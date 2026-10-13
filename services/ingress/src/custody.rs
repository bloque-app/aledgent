// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Custody switching, credential sets and recovery through ingress (`F-012`): the holder's
//! commands on the command surface (`CTR-CMD`), `initiate_recovery` and `complete_recovery` on the
//! operator surface (`CTR-OPS`), and the reads a holder needs before owned custody — the recovery
//! mechanisms and their digest — and after it — the account's custody view.
//!
//! Validation is the shape only, as for every command ([`crate::command`]): identifiers parse,
//! hex decodes to its length. Everything else is the core's (`ledger-core`'s
//! [`ledger_core::ledger::custody`]).

use ledger_core::Refusal;
use ledger_core::account::Custody;
use ledger_core::custody::{
    ChangeKind, ChangeState, Mechanism, MechanismsDigest, PendingId, RecoveryMechanisms, Signature,
};
use ledger_core::id::{AccountId, CredentialId, IdentityId, OperationId};
use ledger_core::ledger::custody::{
    CredentialProof, CustodyView, InitiateRecovery, Proof, ReclaimCustody, RegisterCredential,
};
use ledger_core::principal::{CredentialMarker, Scheme, Verifier};
use ledger_core::set::NonEmptySet;

use crate::wire::{
    AccountCommand, CommandBody, CredentialMarkerBody, CustodyBody, CustodyViewBody, MechanismBody,
    NameSuccessorCommand, PendingChangeBody, PendingCommand, ReclaimCustodyCommand,
    RecoveryMechanismsBody, RegisterCredentialBody, RemoveCredentialCommand,
};

/// A holder's custody command, validated.
#[derive(Clone, PartialEq, Eq)]
pub enum CustodyCommand {
    /// `register_credential`.
    RegisterCredential(RegisterCredential),
    /// `remove_credential`.
    RemoveCredential(CredentialId),
    /// `delegate_custody`.
    DelegateCustody(AccountId),
    /// `reclaim_custody`.
    ReclaimCustody(ReclaimCustody),
    /// `cancel_recovery`.
    CancelRecovery(PendingId),
    /// `name_successor`.
    NameSuccessor(AccountId, Option<IdentityId>),
    /// `claim_succession`.
    ClaimSuccession(AccountId),
}

impl core::fmt::Debug for CustodyCommand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // A verifier is never printed (`REQ-AU-8`).
        match self {
            CustodyCommand::RegisterCredential(r) => f
                .debug_struct("RegisterCredential")
                .field("marker", &r.marker)
                .field("scope", &r.scope)
                .finish_non_exhaustive(),
            CustodyCommand::RemoveCredential(c) => {
                f.debug_tuple("RemoveCredential").field(c).finish()
            }
            CustodyCommand::DelegateCustody(a) => {
                f.debug_tuple("DelegateCustody").field(a).finish()
            }
            CustodyCommand::ReclaimCustody(r) => f.debug_tuple("ReclaimCustody").field(r).finish(),
            CustodyCommand::CancelRecovery(p) => f.debug_tuple("CancelRecovery").field(p).finish(),
            CustodyCommand::NameSuccessor(a, s) => {
                f.debug_tuple("NameSuccessor").field(a).field(s).finish()
            }
            CustodyCommand::ClaimSuccession(a) => {
                f.debug_tuple("ClaimSuccession").field(a).finish()
            }
        }
    }
}

fn account(s: &str) -> Result<AccountId, Refusal> {
    AccountId::parse(s).map_err(|_| Refusal::UnknownAccount)
}

fn credential(s: &str) -> Result<CredentialId, Refusal> {
    CredentialId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

/// An identity: `did:aledgent:id:…`.
pub fn identity(s: &str) -> Result<IdentityId, Refusal> {
    IdentityId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

/// A pending change's identity: an operation reference.
pub fn pending(s: &str) -> Result<PendingId, Refusal> {
    OperationId::parse(s)
        .map(PendingId)
        .map_err(|_| Refusal::UnknownOperation)
}

/// A `key` verifier: hex, public material.
pub fn verifier(s: &str) -> Result<Verifier, Refusal> {
    hex::decode(s)
        .map(Verifier::new)
        .map_err(|_| Refusal::MalformedCommand)
}

/// A verifier of the scheme named (`key` when none): hex, public material (`DEC-19`, `F-028`). An
/// unknown scheme is `ERR-CredentialInvalid`; the core checks the material's shape.
pub fn verifier_of(scheme: Option<&str>, s: &str) -> Result<Verifier, Refusal> {
    let scheme = match scheme {
        None => Scheme::Key,
        Some(name) => Scheme::parse(name).ok_or(Refusal::CredentialInvalid)?,
    };
    hex::decode(s)
        .map(|b| Verifier::of(scheme, b))
        .map_err(|_| Refusal::MalformedCommand)
}

/// A verifier's scheme as the wire names it: absent for `key`, so the canonical form of every
/// command written before `F-028` is unchanged.
pub fn scheme_body(v: &Verifier) -> Option<String> {
    (v.scheme() != Scheme::Key).then(|| v.scheme().name().to_owned())
}

/// A signature: 64 bytes of hex, `R ‖ s`. Anything else fails closed.
pub fn signature(s: &str, bad: Refusal) -> Result<Signature, Refusal> {
    let bytes: [u8; 64] = hex::decode(s)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or(bad)?;
    let mut r = [0u8; 32];
    let mut z = [0u8; 32];
    r.copy_from_slice(&bytes[..32]);
    z.copy_from_slice(&bytes[32..]);
    Ok(Signature { r, s: z })
}

/// A signature's hex form.
pub fn signature_hex(sig: &Signature) -> String {
    let mut b = [0u8; 64];
    b[..32].copy_from_slice(&sig.r);
    b[32..].copy_from_slice(&sig.s);
    hex::encode(b)
}

/// A mechanisms digest: 32 bytes of hex.
pub fn digest(s: &str) -> Result<MechanismsDigest, Refusal> {
    hex::decode(s)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .map(MechanismsDigest)
        .ok_or(Refusal::MalformedCommand)
}

/// A scope: a non-empty list of accounts, or none.
pub fn scope(list: Option<&Vec<String>>) -> Result<Option<NonEmptySet<AccountId>>, Refusal> {
    match list {
        None => Ok(None),
        Some(l) => {
            let set = l.iter().map(|a| account(a)).collect::<Result<_, _>>()?;
            NonEmptySet::new(set)
                .map(Some)
                .ok_or(Refusal::MalformedCommand)
        }
    }
}

/// A scope's wire form.
pub fn scope_body(s: Option<&NonEmptySet<AccountId>>) -> Option<Vec<String>> {
    s.map(|s| s.iter().map(|a| a.to_did()).collect())
}

impl CustodyCommand {
    /// Validate a wire command, if it is one of these.
    pub fn parse(body: &CommandBody) -> Option<Result<CustodyCommand, Refusal>> {
        Some(match body {
            CommandBody::RegisterCredential(b) => Self::register(b),
            CommandBody::RemoveCredential(RemoveCredentialCommand { credential: c }) => {
                credential(c).map(CustodyCommand::RemoveCredential)
            }
            CommandBody::DelegateCustody(AccountCommand { account: a }) => {
                account(a).map(CustodyCommand::DelegateCustody)
            }
            CommandBody::ReclaimCustody(b) => Self::reclaim(b),
            CommandBody::CancelRecovery(PendingCommand { pending: p }) => {
                pending(p).map(CustodyCommand::CancelRecovery)
            }
            CommandBody::NameSuccessor(NameSuccessorCommand {
                account: a,
                successor,
            }) => (|| {
                Ok(CustodyCommand::NameSuccessor(
                    account(a)?,
                    successor.as_deref().map(identity).transpose()?,
                ))
            })(),
            CommandBody::ClaimSuccession(AccountCommand { account: a }) => {
                account(a).map(CustodyCommand::ClaimSuccession)
            }
            _ => return None,
        })
    }

    fn register(b: &RegisterCredentialBody) -> Result<CustodyCommand, Refusal> {
        Ok(CustodyCommand::RegisterCredential(RegisterCredential {
            marker: match b.marker {
                CredentialMarkerBody::Ordinary => CredentialMarker::Ordinary,
                CredentialMarkerBody::RecoveryHeld => CredentialMarker::RecoveryHeld,
            },
            scope: scope(b.scope.as_ref())?,
            verifier: verifier_of(b.scheme.as_deref(), &b.verifier)?,
        }))
    }

    fn reclaim(b: &ReclaimCustodyCommand) -> Result<CustodyCommand, Refusal> {
        Ok(CustodyCommand::ReclaimCustody(ReclaimCustody {
            account: account(&b.account)?,
            proof: CredentialProof {
                credential: CredentialId::parse(&b.proof_credential)
                    .map_err(|_| Refusal::CredentialInvalid)?,
                proof: match (b.passkey_checked, b.signature.as_deref()) {
                    (true, None) => Proof::PasskeyChecked,
                    (false, Some(sig)) => {
                        Proof::Signature(signature(sig, Refusal::CredentialInvalid)?)
                    }
                    _ => return Err(Refusal::CredentialInvalid),
                },
            },
            acknowledged: b.acknowledged.as_deref().map(digest).transpose()?,
        }))
    }

    /// The canonical wire form.
    pub fn to_body(&self) -> CommandBody {
        match self {
            CustodyCommand::RegisterCredential(r) => {
                CommandBody::RegisterCredential(RegisterCredentialBody {
                    marker: match r.marker {
                        CredentialMarker::Ordinary => CredentialMarkerBody::Ordinary,
                        CredentialMarker::RecoveryHeld => CredentialMarkerBody::RecoveryHeld,
                        // Never parsed from a holder's command: only `import_credentials` marks
                        // a credential imported (`REQ-MG-7`).
                        CredentialMarker::Imported => CredentialMarkerBody::Ordinary,
                    },
                    scope: scope_body(r.scope.as_ref()),
                    verifier: hex::encode(r.verifier.as_bytes()),
                    scheme: scheme_body(&r.verifier),
                })
            }
            CustodyCommand::RemoveCredential(c) => {
                CommandBody::RemoveCredential(RemoveCredentialCommand {
                    credential: c.to_did(),
                })
            }
            CustodyCommand::DelegateCustody(a) => CommandBody::DelegateCustody(AccountCommand {
                account: a.to_did(),
            }),
            CustodyCommand::ReclaimCustody(r) => {
                CommandBody::ReclaimCustody(ReclaimCustodyCommand {
                    account: r.account.to_did(),
                    proof_credential: r.proof.credential.to_did(),
                    signature: match &r.proof.proof {
                        Proof::Signature(sig) => Some(signature_hex(sig)),
                        Proof::PasskeyChecked => None,
                    },
                    passkey_checked: r.proof.proof == Proof::PasskeyChecked,
                    acknowledged: r.acknowledged.map(|d| hex::encode(d.0)),
                })
            }
            CustodyCommand::CancelRecovery(p) => CommandBody::CancelRecovery(PendingCommand {
                pending: p.0.to_did(),
            }),
            CustodyCommand::NameSuccessor(a, s) => {
                CommandBody::NameSuccessor(NameSuccessorCommand {
                    account: a.to_did(),
                    successor: s.map(|s| s.to_did()),
                })
            }
            CustodyCommand::ClaimSuccession(a) => CommandBody::ClaimSuccession(AccountCommand {
                account: a.to_did(),
            }),
        }
    }
}

/// `initiate_recovery`'s request, from the operator command's fields.
pub fn initiate(
    a: &str,
    scheme: Option<&str>,
    v: &str,
    s: Option<&Vec<String>>,
) -> Result<InitiateRecovery, Refusal> {
    Ok(InitiateRecovery {
        account: account(a)?,
        verifier: verifier_of(scheme, v)?,
        scope: scope(s)?,
    })
}

/// The recovery mechanisms as a reply shows them, with their digest.
pub fn mechanisms_body(m: &RecoveryMechanisms) -> RecoveryMechanismsBody {
    RecoveryMechanismsBody {
        mechanisms: m
            .mechanisms
            .iter()
            .map(|m| match m {
                Mechanism::RecoveryAuthority { authority, delay } => {
                    MechanismBody::RecoveryAuthority {
                        authority: authority.to_did(),
                        delay: *delay,
                    }
                }
                Mechanism::Succession { delay, named } => MechanismBody::Succession {
                    delay: *delay,
                    named: named.map(|n| n.to_did()),
                },
                Mechanism::HolderHeld { credentials } => MechanismBody::HolderHeld {
                    credentials: credentials.iter().map(|c| c.to_did()).collect(),
                },
            })
            .collect(),
        digest: hex::encode(m.digest().0),
    }
}

/// The account's custody view as a reply shows it.
pub fn view_body(v: &CustodyView) -> CustodyViewBody {
    CustodyViewBody {
        account: v.account.to_did(),
        owner: v.owner.to_did(),
        custody: match v.custody {
            Custody::Delegated => CustodyBody::Delegated,
            Custody::Owned => CustodyBody::Owned,
        },
        successor: v.successor.map(|s| s.to_did()),
        epoch: v.epoch,
        changes: v
            .changes
            .iter()
            .map(|c| PendingChangeBody {
                pending: c.id.0.to_did(),
                kind: match c.kind {
                    ChangeKind::Recovery => "recovery",
                    ChangeKind::Succession => "succession",
                }
                .to_owned(),
                state: match c.state {
                    ChangeState::Pending => "pending",
                    ChangeState::Cancelled => "cancelled",
                    ChangeState::Completed => "completed",
                }
                .to_owned(),
                initiated_at: c.initiated_at,
                effective_at: c.effective_at,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_kernel::posting::OperationRef;

    #[test]
    fn a_custody_command_round_trips_through_its_canonical_form() {
        let op = OperationId::from_ref(OperationRef([3; 16]));
        let a = AccountId::mint(&op, 0);
        let bodies = [
            CommandBody::RegisterCredential(RegisterCredentialBody {
                marker: CredentialMarkerBody::RecoveryHeld,
                scope: Some(vec![a.to_did()]),
                verifier: "0a".repeat(32),
                scheme: None,
            }),
            CommandBody::RegisterCredential(RegisterCredentialBody {
                marker: CredentialMarkerBody::Ordinary,
                scope: None,
                verifier: "0b".repeat(32),
                scheme: Some("api-key".into()),
            }),
            CommandBody::ReclaimCustody(ReclaimCustodyCommand {
                account: a.to_did(),
                proof_credential: CredentialId::mint(&op, 1).to_did(),
                signature: Some("11".repeat(64)),
                passkey_checked: false,
                acknowledged: Some("22".repeat(32)),
            }),
            CommandBody::ReclaimCustody(ReclaimCustodyCommand {
                account: a.to_did(),
                proof_credential: CredentialId::mint(&op, 1).to_did(),
                signature: None,
                passkey_checked: true,
                acknowledged: None,
            }),
            CommandBody::NameSuccessor(NameSuccessorCommand {
                account: a.to_did(),
                successor: None,
            }),
            CommandBody::CancelRecovery(PendingCommand {
                pending: op.to_did(),
            }),
        ];
        for b in bodies {
            let c = CustodyCommand::parse(&b).unwrap().unwrap();
            assert_eq!(c.to_body(), b);
        }
    }

    #[test]
    fn a_malformed_signature_or_digest_fails_closed() {
        assert_eq!(
            signature("11", Refusal::CredentialInvalid),
            Err(Refusal::CredentialInvalid)
        );
        // Shape faults (amendment 0021, `REQ-ER-3`).
        assert_eq!(digest("zz"), Err(Refusal::MalformedCommand));
        assert_eq!(scope(Some(&Vec::new())), Err(Refusal::MalformedCommand));
    }
}
