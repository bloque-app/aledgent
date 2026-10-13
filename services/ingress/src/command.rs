// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Commands: validating a wire command into `ledger-core`'s types, its canonical form, its
//! client key, and how ingress takes it in (`REQ-CS-1`, `REQ-OP-10`, `REQ-OP-11`).
//!
//! Validation is the shape only: identifiers parse, amounts are exact integer minor units, sets
//! are not empty. Everything that needs the book — whether an account exists, the registry,
//! authority — is the core's to decide. A value that does not parse is refused with the condition
//! of SPEC §10 that names it: an account `ERR-UnknownAccount`, an asset `ERR-UnknownAsset`, an
//! operation `ERR-UnknownOperation`, an amount `ERR-PrecisionExceeded` (as `F-003` refuses a
//! decimal that cannot be represented exactly). Any other shape fault (a body that is not the
//! command, an identifier, encoding or enumerated value that does not parse, an empty list that
//! must not be) is `ERR-MalformedCommand` (`REQ-ER-3`, amendment 0021; `NOTES.md`, decision 4).

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::custody::MechanismsDigest;
use ledger_core::id::{AccountId, CredentialId, GrantId, IdentityId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::settlement::Withdrawal;
use ledger_core::ledger::{GrantRequest, Leg, OpenAccount, Transfer};
use ledger_core::lifecycle::keys::{Canon, ClientKey, RequestDigest, Surface, derive_client_key};
use ledger_core::principal::Verifier;
use ledger_core::set::NonEmptySet;

use crate::anchoring::AnchoringCommand;
use crate::custody::CustodyCommand;
use crate::operator::Justified;
use crate::settlement::SettlementCommand;
use crate::wire::{
    AccountKindBody, CaptureCommand, CloseAccountCommand, CommandBody, CustodyBody, GrantBody,
    GrantKindBody, IdentityKindBody, IssueBody, LegBody, OpenAccountBody, OpenIdentityBody,
    OperationKindBody, PassThrough, RedeemBody, ReleaseCommand, ReserveBody, RevokeCommand,
    SubjectBody, TransferBody, WindowBody, WithdrawBody,
};

/// The largest command the core accepts in one invocation, in bytes of canonical body
/// (`notes/FIT-VOS-CIPHERCLERK.md`: the 16 KiB invocation limit; `OQ-24`).
pub const MAX_COMMAND_BYTES: usize = 16 * 1024;

/// A verifier, which is never formatted (`REQ-AU-8`): `Debug` shows its scheme only.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifierArg(pub Verifier);

impl std::fmt::Debug for VerifierArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Verifier").field(&self.0.scheme()).finish()
    }
}

/// A command, validated into `ledger-core`'s types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `transfer`.
    Transfer(Transfer),
    /// `reserve`.
    Reserve {
        /// The payer.
        payer: AccountId,
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: Minor,
        /// The deadline.
        deadline: Option<Time>,
    },
    /// `capture`.
    Capture {
        /// The reservation.
        reservation: OperationId,
        /// The account credited.
        destination: AccountId,
        /// The amount cleared.
        cleared: Minor,
    },
    /// `release`.
    Release {
        /// The reservation.
        reservation: OperationId,
    },
    /// `issue`.
    Issue {
        /// The asset.
        asset: AssetId,
        /// The account credited.
        destination: AccountId,
        /// The amount.
        amount: Minor,
    },
    /// `redeem`.
    Redeem {
        /// The asset.
        asset: AssetId,
        /// The account debited.
        source: AccountId,
        /// The amount.
        amount: Minor,
    },
    /// `withdraw`.
    Withdraw(Withdrawal),
    /// `open_identity`.
    OpenIdentity {
        /// Its kind.
        kind: IdentityKind,
        /// Whom an agent acts for.
        acts_for: Option<IdentityId>,
        /// The first credential's verifier, with its scheme (`F-028`).
        verifier: VerifierArg,
    },
    /// `open_account`.
    OpenAccount(OpenAccount),
    /// `close_account`.
    CloseAccount {
        /// The account.
        account: AccountId,
    },
    /// `grant_authority`.
    GrantAuthority(GrantRequest),
    /// `revoke_authority`.
    RevokeAuthority {
        /// The grant.
        grant: GrantId,
    },
    /// An operator command (`CTR-OPS`, `F-013`).
    Operator(Justified),
    /// A holder's custody, credential-set or succession command (`F-012`).
    Custody(CustodyCommand),
    /// `submit_outbound`, `configure_settlement`, or an input of the settlement adapter's
    /// (`F-016`).
    Settlement(SettlementCommand),
    /// `issue_disclosure`, `anchor_checkpoint` or `compare_settlement` (`F-017`).
    Anchoring(AnchoringCommand),
}

/// How ingress answers a command it has logged: the two ways of `REQ-CS-8` (amendment 0008, ruled
/// 2026-09-30). A command that moves value is acknowledged at `Received` ([`Intake::Acknowledge`],
/// [`Intake::Reserve`]: `NFR-1`); every other command is answered with its decision
/// ([`Intake::Decide`]: `NFR-16`), and an account or security command always is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intake {
    /// Reply `Received` at once (`REQ-CS-2`); the core decides later and records a refusal on the
    /// operation (`Received → Rejected`) with its notification.
    Acknowledge,
    /// Reply `Received` at once, as [`Intake::Acknowledge`], and decide with priority in the
    /// forwarder queue: a reservation (`REQ-CS-3`; amendment 0007, `OQ-61` as ruled on
    /// 2026-09-29). A refusal is recorded as `Received → Rejected`.
    Reserve,
    /// Reply only with the core's decision, in arrival order (`REQ-CS-8`, `NFR-16`), never
    /// `Received`: a command the core decides and completes at once and records no refusal of, so
    /// an acknowledgement would leave a refusal reported nowhere (`NOTES.md`, decision 3).
    Decide,
}

fn minor(s: &str) -> Result<Minor, Refusal> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Refusal::PrecisionExceeded);
    }
    s.parse::<u64>()
        .map(Minor::new)
        .map_err(|_| Refusal::PrecisionExceeded)
}

fn nonzero(s: &str) -> Result<NonZeroMinor, Refusal> {
    NonZeroMinor::new(minor(s)?)
}

fn account(s: &str) -> Result<AccountId, Refusal> {
    AccountId::parse(s).map_err(|_| Refusal::UnknownAccount)
}

fn asset(s: &str) -> Result<AssetId, Refusal> {
    AssetId::parse(s)
}

fn operation(s: &str) -> Result<OperationId, Refusal> {
    OperationId::parse(s).map_err(|_| Refusal::UnknownOperation)
}

fn identity(s: &str) -> Result<IdentityId, Refusal> {
    IdentityId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

/// A mechanisms digest: 32 bytes of hex (`F-012`). Anything else fails closed.
fn digest(s: &str) -> Result<MechanismsDigest, Refusal> {
    hex::decode(s)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .map(MechanismsDigest)
        .ok_or(Refusal::MalformedCommand)
}

fn grant(s: &str) -> Result<GrantId, Refusal> {
    GrantId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

/// A credential identifier (used by the authenticator's configuration).
pub fn credential(s: &str) -> Result<CredentialId, Refusal> {
    CredentialId::parse(s).map_err(|_| Refusal::CredentialInvalid)
}

/// An identity identifier.
pub fn identity_id(s: &str) -> Result<IdentityId, Refusal> {
    identity(s)
}

pub(crate) fn op_kind(k: OperationKindBody) -> OperationKind {
    match k {
        OperationKindBody::Transfer => OperationKind::Transfer,
        OperationKindBody::Reserve => OperationKind::Reserve,
        OperationKindBody::Capture => OperationKind::Capture,
        OperationKindBody::Release => OperationKind::Release,
        OperationKindBody::Convert => OperationKind::Convert,
        OperationKindBody::Encumber => OperationKind::Encumber,
        OperationKindBody::Disencumber => OperationKind::Disencumber,
        OperationKindBody::Issue => OperationKind::Issue,
        OperationKindBody::Redeem => OperationKind::Redeem,
        OperationKindBody::CloseAccount => OperationKind::CloseAccount,
        OperationKindBody::Disclose => OperationKind::Disclose,
        OperationKindBody::Reversal => OperationKind::Reversal,
        OperationKindBody::OpenAccount => OperationKind::OpenAccount,
    }
}

pub(crate) fn op_kind_body(k: OperationKind) -> OperationKindBody {
    match k {
        OperationKind::Transfer => OperationKindBody::Transfer,
        OperationKind::Reserve => OperationKindBody::Reserve,
        OperationKind::Capture => OperationKindBody::Capture,
        OperationKind::Release => OperationKindBody::Release,
        OperationKind::Convert => OperationKindBody::Convert,
        OperationKind::Encumber => OperationKindBody::Encumber,
        OperationKind::Disencumber => OperationKindBody::Disencumber,
        OperationKind::Issue => OperationKindBody::Issue,
        OperationKind::Redeem => OperationKindBody::Redeem,
        OperationKind::CloseAccount => OperationKindBody::CloseAccount,
        OperationKind::Disclose => OperationKindBody::Disclose,
        OperationKind::Reversal => OperationKindBody::Reversal,
        OperationKind::OpenAccount => OperationKindBody::OpenAccount,
    }
}

fn non_empty<T: Ord>(items: impl IntoIterator<Item = T>) -> Result<NonEmptySet<T>, Refusal> {
    NonEmptySet::new(items.into_iter().collect::<BTreeSet<T>>()).ok_or(Refusal::MalformedCommand)
}

impl Command {
    /// Validate a wire command (see the module documentation).
    pub fn parse(body: &CommandBody) -> Result<Command, Refusal> {
        Ok(match body {
            CommandBody::Transfer(TransferBody { legs, deadline, .. }) => {
                let mut out = Vec::with_capacity(legs.len());
                for l in legs {
                    out.push(Leg {
                        from: account(&l.from)?,
                        to: account(&l.to)?,
                        asset: asset(&l.asset)?,
                        amount: minor(&l.amount)?,
                    });
                }
                Command::Transfer(Transfer {
                    legs: out,
                    deadline: *deadline,
                })
            }
            CommandBody::Reserve(ReserveBody {
                payer,
                asset: a,
                amount,
                deadline,
                ..
            }) => Command::Reserve {
                payer: account(payer)?,
                asset: asset(a)?,
                amount: minor(amount)?,
                deadline: *deadline,
            },
            CommandBody::Capture(CaptureCommand {
                reservation,
                destination,
                cleared,
            }) => Command::Capture {
                reservation: operation(reservation)?,
                destination: account(destination)?,
                cleared: minor(cleared)?,
            },
            CommandBody::Release(ReleaseCommand { reservation }) => Command::Release {
                reservation: operation(reservation)?,
            },
            CommandBody::Issue(IssueBody {
                asset: a,
                destination,
                amount,
                ..
            }) => Command::Issue {
                asset: asset(a)?,
                destination: account(destination)?,
                amount: minor(amount)?,
            },
            CommandBody::Redeem(RedeemBody {
                asset: a,
                source,
                amount,
                ..
            }) => Command::Redeem {
                asset: asset(a)?,
                source: account(source)?,
                amount: minor(amount)?,
            },
            CommandBody::Withdraw(WithdrawBody {
                payer,
                asset: a,
                amount,
                deadline,
                ..
            }) => Command::Withdraw(Withdrawal {
                payer: account(payer)?,
                asset: asset(a)?,
                amount: minor(amount)?,
                deadline: *deadline,
            }),
            CommandBody::OpenIdentity(OpenIdentityBody {
                kind,
                acts_for,
                verifier,
                scheme,
                ..
            }) => Command::OpenIdentity {
                kind: match kind {
                    IdentityKindBody::Person => IdentityKind::Person,
                    IdentityKindBody::Organisation => IdentityKind::Organisation,
                    IdentityKindBody::Agent => IdentityKind::Agent,
                },
                acts_for: acts_for.as_deref().map(identity).transpose()?,
                verifier: VerifierArg(crate::custody::verifier_of(scheme.as_deref(), verifier)?),
            },
            CommandBody::OpenAccount(OpenAccountBody {
                owner,
                kind,
                custody,
                treasury_limits,
                acknowledged,
                ..
            }) => {
                let mut limits = BTreeMap::new();
                for (a, m) in treasury_limits {
                    limits.insert(asset(a)?, minor(m)?);
                }
                Command::OpenAccount(OpenAccount {
                    owner: identity(owner)?,
                    kind: match kind {
                        AccountKindBody::Customer => AccountKind::Customer,
                        AccountKindBody::Treasury => AccountKind::Treasury,
                        AccountKindBody::Suspense => AccountKind::Suspense,
                    },
                    custody: match custody {
                        CustodyBody::Delegated => Custody::Delegated,
                        CustodyBody::Owned => Custody::Owned,
                    },
                    treasury_limits: limits,
                    acknowledged: acknowledged.as_deref().map(digest).transpose()?,
                })
            }
            CommandBody::CloseAccount(CloseAccountCommand { account: a }) => {
                Command::CloseAccount {
                    account: account(a)?,
                }
            }
            CommandBody::GrantAuthority(g) => Command::GrantAuthority(grant_request(g)?),
            CommandBody::RevokeAuthority(RevokeCommand { grant: g }) => {
                Command::RevokeAuthority { grant: grant(g)? }
            }
            CommandBody::Operator(o) => Command::Operator(Justified::parse(o)?),
            b @ (CommandBody::RegisterCredential(_)
            | CommandBody::RemoveCredential(_)
            | CommandBody::DelegateCustody(_)
            | CommandBody::ReclaimCustody(_)
            | CommandBody::CancelRecovery(_)
            | CommandBody::NameSuccessor(_)
            | CommandBody::ClaimSuccession(_)) => {
                Command::Custody(CustodyCommand::parse(b).ok_or(Refusal::NotAuthorised)??)
            }
            b @ (CommandBody::SubmitOutbound(_)
            | CommandBody::ConfigureSettlement(_)
            | CommandBody::Adapter(_)) => {
                Command::Settlement(SettlementCommand::parse(b).ok_or(Refusal::NotAuthorised)??)
            }
            b @ CommandBody::Anchoring(_) => {
                Command::Anchoring(AnchoringCommand::parse(b).ok_or(Refusal::NotAuthorised)??)
            }
        })
    }

    /// The surface it arrives on: `CTR-OPS` for an operator command, `CTR-CMD` otherwise. The
    /// client-key domain is (principal, surface, key) (`REQ-OP-10`, `DEP-10`).
    pub fn surface(&self) -> Surface {
        match self {
            Command::Operator(_) => Surface::Operator,
            Command::Settlement(s) if s.is_operator() => Surface::Operator,
            Command::Anchoring(a) if a.is_operator() => Surface::Operator,
            _ => Surface::Command,
        }
    }

    /// The command in its canonical wire form: every value re-rendered from its parsed form, so
    /// two spellings of one request (`"010"` and `"10"`) are one request.
    pub fn to_body(&self) -> CommandBody {
        let m = |x: Minor| x.get().to_string();
        match self {
            Command::Transfer(t) => CommandBody::Transfer(TransferBody {
                legs: t
                    .legs
                    .iter()
                    .map(|l| LegBody {
                        from: l.from.to_did(),
                        to: l.to.to_did(),
                        asset: l.asset.as_str().to_owned(),
                        amount: m(l.amount),
                    })
                    .collect(),
                deadline: t.deadline,
                reference: None,
            }),
            Command::Reserve {
                payer,
                asset,
                amount,
                deadline,
            } => CommandBody::Reserve(ReserveBody {
                payer: payer.to_did(),
                asset: asset.as_str().to_owned(),
                amount: m(*amount),
                deadline: *deadline,
                reference: None,
            }),
            Command::Capture {
                reservation,
                destination,
                cleared,
            } => CommandBody::Capture(CaptureCommand {
                reservation: reservation.to_did(),
                destination: destination.to_did(),
                cleared: m(*cleared),
            }),
            Command::Release { reservation } => CommandBody::Release(ReleaseCommand {
                reservation: reservation.to_did(),
            }),
            Command::Issue {
                asset,
                destination,
                amount,
            } => CommandBody::Issue(IssueBody {
                asset: asset.as_str().to_owned(),
                destination: destination.to_did(),
                amount: m(*amount),
                reference: None,
            }),
            Command::Redeem {
                asset,
                source,
                amount,
            } => CommandBody::Redeem(RedeemBody {
                asset: asset.as_str().to_owned(),
                source: source.to_did(),
                amount: m(*amount),
                reference: None,
            }),
            Command::Withdraw(w) => CommandBody::Withdraw(WithdrawBody {
                payer: w.payer.to_did(),
                asset: w.asset.as_str().to_owned(),
                amount: m(w.amount),
                deadline: w.deadline,
                reference: None,
            }),
            Command::OpenIdentity {
                kind,
                acts_for,
                verifier,
            } => CommandBody::OpenIdentity(OpenIdentityBody {
                kind: match kind {
                    IdentityKind::Person => IdentityKindBody::Person,
                    IdentityKind::Organisation => IdentityKindBody::Organisation,
                    IdentityKind::Agent => IdentityKindBody::Agent,
                },
                acts_for: acts_for.map(|i| i.to_did()),
                verifier: hex::encode(verifier.0.as_bytes()),
                scheme: crate::custody::scheme_body(&verifier.0),
                reference: None,
            }),
            Command::OpenAccount(o) => CommandBody::OpenAccount(OpenAccountBody {
                owner: o.owner.to_did(),
                kind: match o.kind {
                    AccountKind::Customer => AccountKindBody::Customer,
                    AccountKind::Treasury => AccountKindBody::Treasury,
                    // Never parsed from the wire: an issuance account is opened by genesis only.
                    AccountKind::Issuance | AccountKind::Suspense => AccountKindBody::Suspense,
                },
                custody: match o.custody {
                    Custody::Delegated => CustodyBody::Delegated,
                    Custody::Owned => CustodyBody::Owned,
                },
                treasury_limits: o
                    .treasury_limits
                    .iter()
                    .map(|(a, x)| (a.as_str().to_owned(), m(*x)))
                    .collect(),
                acknowledged: o.acknowledged.map(|d| hex::encode(d.0)),
                reference: None,
            }),
            Command::CloseAccount { account } => CommandBody::CloseAccount(CloseAccountCommand {
                account: account.to_did(),
            }),
            Command::GrantAuthority(g) => CommandBody::GrantAuthority(grant_body(g)),
            Command::RevokeAuthority { grant } => CommandBody::RevokeAuthority(RevokeCommand {
                grant: grant.to_did(),
            }),
            Command::Operator(o) => CommandBody::Operator(o.to_body()),
            Command::Custody(c) => c.to_body(),
            Command::Settlement(s) => s.to_body(),
            Command::Anchoring(a) => a.to_body(),
        }
    }

    /// The command's canonical bytes: the canonical JSON of [`Command::to_body`].
    pub fn canonical(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_body()).unwrap_or_default()
    }

    /// How ingress answers it (see [`Intake`], `REQ-CS-8`). Every variant is named: a new command
    /// must be put in one of the two ways on purpose.
    pub fn intake(&self) -> Intake {
        match self {
            // Commands that move value: acknowledged at `Received` (`NFR-1`).
            Command::Transfer(_) => Intake::Acknowledge,
            Command::Reserve { .. } | Command::Withdraw(_) => Intake::Reserve,
            // Account and security commands: always answered with their decision, never `Received`
            // (`REQ-CS-8`, `NFR-16`; amendment 0008). `anchor_checkpoint` and `compare_settlement`
            // are operator commands; `issue_disclosure`, on the disclosure surface, shares their
            // variant and is answered with its decision like the rest.
            Command::OpenIdentity { .. }
            | Command::OpenAccount(_)
            | Command::CloseAccount { .. }
            | Command::GrantAuthority(_)
            | Command::RevokeAuthority { .. }
            | Command::Custody(_)
            | Command::Operator(_)
            | Command::Anchoring(_) => Intake::Decide,
            // The rest are answered with their decision too (`REQ-CS-8`). Whether `capture`,
            // `release`, `issue` and `redeem`, which move value, should be acknowledged instead is
            // `OQ-63`, open.
            Command::Capture { .. }
            | Command::Release { .. }
            | Command::Issue { .. }
            | Command::Redeem { .. }
            | Command::Settlement(_) => Intake::Decide,
        }
    }

    /// The operation the command acts under: the reservation of a capture or release, otherwise
    /// the reference ingress minted for it.
    pub fn operation(&self, minted: OperationId) -> OperationId {
        match self {
            Command::Capture { reservation, .. } | Command::Release { reservation } => *reservation,
            Command::Settlement(s) => s.operation(minted),
            _ => minted,
        }
    }

    /// The refusal for a command larger than the core's invocation limit: `ERR-BatchTooLarge`
    /// for a unit of legs (`REQ-CS-10`), `ERR-MalformedCommand` for anything else (amendment 0021).
    pub fn too_large(body: &CommandBody) -> Refusal {
        match body {
            CommandBody::Transfer(_) => Refusal::BatchTooLarge,
            _ => Refusal::MalformedCommand,
        }
    }
}

fn grant_request(g: &GrantBody) -> Result<GrantRequest, Refusal> {
    let window = |w: &WindowBody| -> Result<Window, Refusal> {
        Ok(Window {
            length: NonZeroU64::new(w.length).ok_or(Refusal::UnboundedGrant)?,
            limit: nonzero(&w.limit)?,
        })
    };
    let assets = match &g.assets {
        None => None,
        Some(list) => Some(non_empty(
            list.iter()
                .map(|a| asset(a))
                .collect::<Result<Vec<_>, _>>()?,
        )?),
    };
    let counterparties = match &g.counterparties {
        None => None,
        Some(list) => Some(non_empty(
            list.iter()
                .map(|a| account(a))
                .collect::<Result<Vec<_>, _>>()?,
        )?),
    };
    Ok(GrantRequest {
        kind: match g.kind {
            GrantKindBody::Delegated => GrantKind::Delegated,
            GrantKindBody::Operator => GrantKind::Operator,
            GrantKindBody::Issuance => GrantKind::Issuance,
        },
        principal: identity(&g.principal)?,
        subject: match &g.subject {
            SubjectBody::Account(a) => Subject::Account(account(a)?),
            SubjectBody::DelegatedAccounts => Subject::DelegatedAccounts,
            SubjectBody::Asset(a) => Subject::Asset(asset(a)?),
            SubjectBody::Identity(i) => Subject::Identity(identity(i)?),
        },
        ops: non_empty(g.ops.iter().copied().map(op_kind))?,
        per_operation: g.per_operation.as_deref().map(nonzero).transpose()?,
        per_window: g.per_window.as_ref().map(window).transpose()?,
        assets,
        counterparties,
        parent: g.parent.as_deref().map(grant).transpose()?,
        expires: g.expires,
    })
}

fn grant_body(g: &GrantRequest) -> GrantBody {
    GrantBody {
        kind: match g.kind {
            GrantKind::Delegated => GrantKindBody::Delegated,
            GrantKind::Operator => GrantKindBody::Operator,
            GrantKind::Issuance => GrantKindBody::Issuance,
        },
        principal: g.principal.to_did(),
        subject: match g.subject {
            Subject::Account(a) => SubjectBody::Account(a.to_did()),
            Subject::DelegatedAccounts => SubjectBody::DelegatedAccounts,
            Subject::Asset(a) => SubjectBody::Asset(a.as_str().to_owned()),
            Subject::Identity(i) => SubjectBody::Identity(i.to_did()),
        },
        ops: g.ops.iter().copied().map(op_kind_body).collect(),
        per_operation: g.per_operation.map(|x| x.get().to_string()),
        per_window: g.per_window.map(|w| WindowBody {
            length: w.length.get(),
            limit: w.limit.get().to_string(),
        }),
        assets: g
            .assets
            .as_ref()
            .map(|s| s.iter().map(|a| a.as_str().to_owned()).collect()),
        counterparties: g
            .counterparties
            .as_ref()
            .map(|s| s.iter().map(|a| a.to_did()).collect()),
        parent: g.parent.map(|p| p.to_did()),
        expires: g.expires,
    }
}

/// An account as `account(account)` shows it (amendment 0028).
pub fn account_view_body(a: &ledger_core::account::LedgerAccount) -> crate::wire::AccountViewBody {
    use crate::wire::AccountStatusBody;
    use ledger_core::account::AccountStatus;
    crate::wire::AccountViewBody {
        account: a.id().to_did(),
        owner: a.owner().to_did(),
        kind: match a.kind() {
            AccountKind::Customer => AccountKindBody::Customer,
            AccountKind::Treasury => AccountKindBody::Treasury,
            // The read never answers the issuance counterparty (`ERR-UnknownAccount`).
            AccountKind::Issuance | AccountKind::Suspense => AccountKindBody::Suspense,
        },
        custody: match a.custody() {
            Custody::Delegated => CustodyBody::Delegated,
            Custody::Owned => CustodyBody::Owned,
        },
        status: match a.status() {
            AccountStatus::Active => AccountStatusBody::Active,
            AccountStatus::Frozen => AccountStatusBody::Frozen,
            AccountStatus::Closing => AccountStatusBody::Closing,
            AccountStatus::Closed => AccountStatusBody::Closed,
        },
        closed: a.status() == AccountStatus::Closed,
        opened_at: a.opened_at(),
    }
}

/// A stored grant as `grants(account)` shows it (`REQ-RC-7`).
pub fn grant_view_body(g: &ledger_core::authority::Grant) -> crate::wire::GrantViewBody {
    let b = g.bounds();
    crate::wire::GrantViewBody {
        grant: g.id().to_did(),
        grantor: g.grantor().to_did(),
        granted_at: g.granted_at(),
        revoked: g.revoked(),
        terms: grant_body(&GrantRequest {
            kind: g.kind(),
            principal: g.principal(),
            subject: g.subject(),
            ops: g.ops().clone(),
            per_operation: b.per_operation(),
            per_window: b.per_window(),
            assets: b.assets().cloned(),
            counterparties: b.counterparties().cloned(),
            parent: g.parent(),
            expires: g.expires(),
        }),
    }
}

/// The digest of a request's semantic identity: who asks, for whom, on which surface, and the
/// canonical command (`REQ-OP-10`). No time, no transport field and no credential enter it
/// (`REQ-OP-11`).
pub fn request_digest(
    principal: &IdentityId,
    on_behalf_of: Option<&IdentityId>,
    surface: Surface,
    canonical: &[u8],
) -> RequestDigest {
    request_digest_with(principal, on_behalf_of, surface, canonical, None)
}

/// [`request_digest`] of a command that may carry pass-through data (`REQ-PO-5`): the data is
/// part of the request, so the same client key with different data is another request
/// (`ERR-ClientKeyReused`, `AC-M8.2`). Without any, the digest is [`request_digest`]'s, unchanged:
/// a row logged before pass-through data existed keeps its digest.
pub fn request_digest_with(
    principal: &IdentityId,
    on_behalf_of: Option<&IdentityId>,
    surface: Surface,
    canonical: &[u8],
    pass_through: Option<&PassThrough>,
) -> RequestDigest {
    let mut c = Canon::new("aledgent/ingress/request/v1");
    c.bytes(principal.as_bytes());
    c.opt(on_behalf_of, |c, id| {
        c.bytes(id.as_bytes());
    });
    c.u8(surface.code());
    c.bytes(canonical);
    if let Some(p) = pass_through {
        c.bytes(b"pass-through");
        c.bytes(p.get().as_bytes());
    }
    c.digest()
}

/// The client key of a request submitted without one (`REQ-OP-11`): derived from the request's
/// digest only, so every retry derives the same key, returned as hex.
pub fn derived_client_key(surface: Surface, digest: &RequestDigest) -> String {
    hex::encode(derive_client_key(surface, digest).as_bytes())
}

/// A client key as the store keeps it: the caller's string, as bytes.
pub fn client_key_bytes(key: &str) -> ClientKey {
    ClientKey::new(key.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::LegBody;

    fn did_account(n: u8) -> String {
        AccountId::mint(
            &OperationId::from_ref(ledger_kernel::posting::OperationRef([n; 16])),
            0,
        )
        .to_did()
    }

    fn transfer(amount: &str) -> CommandBody {
        CommandBody::Transfer(TransferBody {
            legs: vec![LegBody {
                from: did_account(1),
                to: did_account(2),
                asset: "USD".into(),
                amount: amount.into(),
            }],
            deadline: Some(5),
            reference: None,
        })
    }

    /// `REQ-OP-11`: a derived key depends on the request only. No time goes in: the same request
    /// derives the same key on every retry, and another request another key.
    #[test]
    fn req_op_11_a_derived_key_is_stable_across_retries_and_holds_no_time() {
        let p = IdentityId::mint(
            &OperationId::from_ref(ledger_kernel::posting::OperationRef([7; 16])),
            0,
        );
        let a = Command::parse(&transfer("10")).unwrap();
        let again = Command::parse(&transfer("010")).unwrap();
        let other = Command::parse(&transfer("11")).unwrap();
        let key = |c: &Command| {
            derived_client_key(
                Surface::Command,
                &request_digest(&p, None, Surface::Command, &c.canonical()),
            )
        };
        assert_eq!(key(&a), key(&a));
        assert_eq!(
            key(&a),
            key(&again),
            "two spellings of one amount are one request"
        );
        assert_ne!(key(&a), key(&other));
        // Acting for someone else is another request.
        let q = IdentityId::mint(
            &OperationId::from_ref(ledger_kernel::posting::OperationRef([8; 16])),
            0,
        );
        assert_ne!(
            request_digest(&p, None, Surface::Command, &a.canonical()),
            request_digest(&p, Some(&q), Surface::Command, &a.canonical())
        );
    }

    /// `REQ-PO-5`, `AC-M8.2`: pass-through data is part of the request. Different data is another
    /// request; the same data the same one; and a command without any keeps the digest it had
    /// before pass-through data existed.
    #[test]
    fn req_po_5_pass_through_data_is_part_of_the_request() {
        let p = IdentityId::mint(
            &OperationId::from_ref(ledger_kernel::posting::OperationRef([7; 16])),
            0,
        );
        let c = Command::parse(&transfer("10")).unwrap().canonical();
        let order = |t: &str| PassThrough::parse(t).unwrap();
        let d = |x: Option<&PassThrough>| request_digest_with(&p, None, Surface::Command, &c, x);
        assert_eq!(
            d(None),
            request_digest(&p, None, Surface::Command, &c),
            "no pass-through data: the digest is unchanged"
        );
        let a = order(r#"{"order":"o-1"}"#);
        assert_eq!(d(Some(&a)), d(Some(&order(r#"{"order":"o-1"}"#))));
        assert_ne!(d(Some(&a)), d(Some(&order(r#"{"order":"o-2"}"#))));
        assert_ne!(d(Some(&a)), d(None));
        // Kept as sent: another spelling of the same value is other data.
        assert_ne!(d(Some(&a)), d(Some(&order(r#"{ "order": "o-1" }"#))));
    }

    #[test]
    fn amounts_are_exact_integer_minor_units() {
        assert_eq!(minor("0"), Ok(Minor::new(0)));
        assert_eq!(minor("18446744073709551615"), Ok(Minor::new(u64::MAX)));
        for bad in ["", "1.5", "-1", "+1", "1e3", " 1", "18446744073709551616"] {
            assert_eq!(minor(bad), Err(Refusal::PrecisionExceeded), "{bad:?}");
        }
    }

    #[test]
    fn parse_names_the_field_that_does_not_parse() {
        let mut t = transfer("1");
        if let CommandBody::Transfer(b) = &mut t {
            b.legs[0].from = "nope".into();
        }
        assert_eq!(Command::parse(&t), Err(Refusal::UnknownAccount));
        let r = CommandBody::Release(ReleaseCommand {
            reservation: "nope".into(),
        });
        assert_eq!(Command::parse(&r), Err(Refusal::UnknownOperation));
    }

    #[test]
    fn a_command_round_trips_through_its_canonical_form() {
        let c = Command::parse(&transfer("0010")).unwrap();
        assert_eq!(Command::parse(&c.to_body()).unwrap(), c);
        assert_eq!(c.intake(), Intake::Acknowledge);
    }

    /// `REQ-CS-8`, `NFR-16` (amendment 0008, ruled 2026-09-30): an account or security command is
    /// answered with its decision, never acknowledged at `Received`; a transfer, a reservation and
    /// a withdrawal are acknowledged (`NFR-1`).
    #[test]
    fn req_cs_8_account_and_security_commands_are_answered_with_their_decision() {
        let op = OperationId::from_ref(ledger_kernel::posting::OperationRef([9; 16]));
        let account = AccountId::mint(&op, 0);
        let commands = [
            Command::CloseAccount { account },
            Command::RevokeAuthority {
                grant: GrantId::mint(&op, 1),
            },
            Command::Custody(CustodyCommand::RemoveCredential(CredentialId::mint(&op, 2))),
            Command::Custody(CustodyCommand::DelegateCustody(account)),
            Command::Custody(CustodyCommand::NameSuccessor(account, None)),
            Command::Custody(CustodyCommand::ClaimSuccession(account)),
            Command::Operator(Justified {
                op: crate::operator::OperatorOp::Freeze {
                    account,
                    frozen: true,
                },
                justification: b"incident 1".to_vec(),
            }),
            Command::Anchoring(AnchoringCommand::AnchorCheckpoint(b"cadence".to_vec())),
        ];
        for c in &commands {
            assert_eq!(c.intake(), Intake::Decide, "{c:?}");
        }
        let transfer = Command::parse(&transfer("1")).unwrap();
        assert_eq!(transfer.intake(), Intake::Acknowledge);
        let reserve = Command::Reserve {
            payer: account,
            asset: AssetId::parse("USD").unwrap(),
            amount: Minor::new(1),
            deadline: None,
        };
        assert_eq!(reserve.intake(), Intake::Reserve);
    }
}
