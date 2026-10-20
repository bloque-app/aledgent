// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The audit trail (`F-013` §1–§3, `REQ-AT-1`, `REQ-AT-2`, `REQ-AU-7`, `DEC-1`, `DEC-3`).
//!
//! Every value-moving operation, authority change, privileged read, operator intervention and
//! disclosure leaves one [`AuditRecord`]. A record names the principal, whom it acted for, the
//! authority it used, the subject, the operation, the time input of the transition, the
//! justification where one is required, and the outcome. Records form a hash chain: each carries
//! its sequence number and the hash of the record before it, so removing or changing one is
//! detectable ([`AuditTrail::verify`]).
//!
//! ## Where records come from
//!
//! - **Emitted inside the commit** ([`emit`]). The service store derives a command's record from
//!   the attribution record it writes (`PutRecord`, `PutTransition`, an attributed transition), in
//!   the same all-or-nothing batch, and appends it to its audit outbox. So a posting can never
//!   commit without its record: the rule is the store's, whatever the command. A command adds
//!   what the attribution record does not carry (the justification, extra subjects) with a
//!   [`emit::AuditNote`] in the same batch. On the Agent (`F-005`) the outbox rides the Agent's
//!   delta.
//! - **Shipped at least once** ([`crate::Ledger::ship_audit`]) to an [`AuditSink`]: the audit
//!   store, kept apart from the book's storage with its own credentials (`REQ-AT-2`). The sink
//!   deduplicates by `audit_seq`.
//! - **Privileged reads** commit nothing to the book: the read path appends its record and ships
//!   it to the sink **before** it returns data, and refuses if the sink refuses (`REQ-PV-4`,
//!   `REQ-PV-5`).
//!
//! Justification text lives only here: in the record, the outbox until it is shipped, and the
//! audit store. It never enters the book, an identifier or an anchored value. No record carries
//! credential material (`REQ-AU-8`): an [`AuthorityUsed`] names a credential by its identifier,
//! never by its verifier.

use alloc::string::String;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;
use ledger_kernel::posting::OperationRef;

use crate::account::Time;
use crate::asset::AssetId;
use crate::authority::{AuthorityKind, AuthorityUsed};
use crate::id::{AccountId, CredentialId, GrantId, IdentityId, OperationId};
use crate::lifecycle::{State, Trigger};
use crate::refusal::Refusal;
use crate::state::Command;

pub mod emit;
pub mod trail;

pub use emit::{AuditLog, AuditNote};
pub use trail::{AuditQuery, AuditSink, AuditTrail, SinkError, TrailError};

/// Domain of the record hash.
const RECORD_DOMAIN: &[u8] = b"aledgent/audit/v1";

/// The five classes of `REQ-AT-1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditClass {
    /// A value-moving operation.
    ValueMoving,
    /// A change of authority: identities, accounts, credentials, grants.
    AuthorityChange,
    /// A read that bypasses the authority checks of `REQ-PV-1` (`REQ-PV-4`).
    PrivilegedRead,
    /// An operator intervention (`REQ-AT-3`).
    OperatorIntervention,
    /// A disclosure (`REQ-DS-5`, `F-017`).
    Disclosure,
}

impl AuditClass {
    /// Every class.
    pub const ALL: [AuditClass; 5] = [
        AuditClass::ValueMoving,
        AuditClass::AuthorityChange,
        AuditClass::PrivilegedRead,
        AuditClass::OperatorIntervention,
        AuditClass::Disclosure,
    ];

    /// A stable one-byte code. Never reorder.
    pub const fn code(self) -> u8 {
        match self {
            AuditClass::ValueMoving => 1,
            AuditClass::AuthorityChange => 2,
            AuditClass::PrivilegedRead => 3,
            AuditClass::OperatorIntervention => 4,
            AuditClass::Disclosure => 5,
        }
    }

    fn from_code(c: u8) -> Option<AuditClass> {
        AuditClass::ALL.into_iter().find(|k| k.code() == c)
    }

    /// The class of an accepted command's record.
    pub const fn of(command: Command) -> AuditClass {
        match command {
            Command::Issue
            | Command::Redeem
            | Command::Transfer
            | Command::Withdraw
            | Command::SubmitOutbound
            | Command::Reserve
            | Command::Capture
            | Command::Release => AuditClass::ValueMoving,
            Command::Genesis
            | Command::OpenIdentity
            | Command::SetIdentityStatus
            | Command::OpenAccount
            | Command::CloseAccount
            | Command::GrantAuthority
            | Command::RevokeAuthority
            | Command::GrantOperator
            | Command::RevokeOperator
            | Command::GrantIssuance
            | Command::RevokeIssuance
            | Command::ConfigureSettlement
            | Command::RegisterCredential
            | Command::RemoveCredential
            | Command::DelegateCustody
            | Command::ReclaimCustody
            | Command::CancelRecovery
            | Command::NameSuccessor
            | Command::ClaimSuccession => AuditClass::AuthorityChange,
            Command::ResolveOperation
            | Command::SubmitEvidence
            | Command::Resume
            | Command::ProposeReversal
            | Command::ApproveReversal
            | Command::FreezeAccount
            | Command::UnfreezeAccount
            | Command::RegisterAsset
            | Command::DeprecateAsset
            | Command::BindAsset
            | Command::DeclareAdapterSupport
            | Command::InitiateRecovery
            | Command::CompleteRecovery
            | Command::ImportBook
            | Command::LoadOpeningBalances
            | Command::AnchorCheckpoint
            | Command::CompareSettlement
            | Command::ImportCredentials
            | Command::RegisterIntegratingParty
            | Command::TiePrincipal
            | Command::ApplyChainSpec
            | Command::RegisterAlias
            | Command::RescheduleUpgrade
            | Command::CancelUpgrade
            | Command::SetAuthority
            | Command::ProposeUpgrade
            | Command::ApproveUpgrade
            | Command::ProposeMigration => AuditClass::OperatorIntervention,
            Command::IssueDisclosure => AuditClass::Disclosure,
        }
    }
}

/// A stable one-byte code for each command. Never reorder; a new command takes the next code.
///
/// **The writer's code (`F-037`), assigned once for the genesis code** (owner ruling on #44 and
/// #45): 51 `RescheduleUpgrade` and 52 `CancelUpgrade` (`T-037-15`); 53 `SetAuthority`,
/// 54 `ProposeUpgrade`, 55 `ApproveUpgrade` and 56 `ProposeMigration` (`T-037-05`). The export
/// tags (`crate::export::records`, `Command`) take the same numbers.
pub const fn command_code(c: Command) -> u8 {
    match c {
        Command::Genesis => 1,
        Command::OpenIdentity => 2,
        Command::SetIdentityStatus => 3,
        Command::OpenAccount => 4,
        Command::CloseAccount => 5,
        Command::GrantAuthority => 6,
        Command::RevokeAuthority => 7,
        Command::Issue => 8,
        Command::Redeem => 9,
        Command::Reserve => 10,
        Command::Capture => 11,
        Command::Release => 12,
        Command::GrantOperator => 13,
        Command::RevokeOperator => 14,
        Command::ResolveOperation => 15,
        Command::Resume => 16,
        Command::ProposeReversal => 17,
        Command::ApproveReversal => 18,
        Command::FreezeAccount => 19,
        Command::UnfreezeAccount => 20,
        Command::RegisterAsset => 21,
        Command::DeprecateAsset => 22,
        Command::GrantIssuance => 23,
        Command::RevokeIssuance => 24,
        Command::DeclareAdapterSupport => 25,
        Command::Transfer => 26,
        Command::Withdraw => 27,
        Command::SubmitOutbound => 28,
        Command::ConfigureSettlement => 29,
        Command::RegisterCredential => 30,
        Command::RemoveCredential => 31,
        Command::DelegateCustody => 32,
        Command::ReclaimCustody => 33,
        Command::InitiateRecovery => 34,
        Command::CancelRecovery => 35,
        Command::CompleteRecovery => 36,
        Command::NameSuccessor => 37,
        Command::ClaimSuccession => 38,
        Command::ImportBook => 39,
        Command::LoadOpeningBalances => 40,
        Command::AnchorCheckpoint => 41,
        Command::CompareSettlement => 42,
        Command::IssueDisclosure => 43,
        Command::SubmitEvidence => 44,
        Command::ImportCredentials => 45,
        Command::RegisterIntegratingParty => 46,
        Command::TiePrincipal => 47,
        Command::BindAsset => 48,
        Command::ApplyChainSpec => 49,
        Command::RegisterAlias => 50,
        Command::RescheduleUpgrade => 51,
        Command::CancelUpgrade => 52,
        Command::SetAuthority => 53,
        Command::ProposeUpgrade => 54,
        Command::ApproveUpgrade => 55,
        Command::ProposeMigration => 56,
    }
}

/// Every command, for decoding.
const COMMANDS: [Command; 56] = [
    Command::Genesis,
    Command::OpenIdentity,
    Command::SetIdentityStatus,
    Command::OpenAccount,
    Command::CloseAccount,
    Command::GrantAuthority,
    Command::RevokeAuthority,
    Command::Issue,
    Command::Redeem,
    Command::Reserve,
    Command::Capture,
    Command::Release,
    Command::GrantOperator,
    Command::RevokeOperator,
    Command::ResolveOperation,
    Command::Resume,
    Command::ProposeReversal,
    Command::ApproveReversal,
    Command::FreezeAccount,
    Command::UnfreezeAccount,
    Command::RegisterAsset,
    Command::DeprecateAsset,
    Command::GrantIssuance,
    Command::RevokeIssuance,
    Command::DeclareAdapterSupport,
    Command::Transfer,
    Command::Withdraw,
    Command::SubmitOutbound,
    Command::ConfigureSettlement,
    Command::RegisterCredential,
    Command::RemoveCredential,
    Command::DelegateCustody,
    Command::ReclaimCustody,
    Command::InitiateRecovery,
    Command::CancelRecovery,
    Command::CompleteRecovery,
    Command::NameSuccessor,
    Command::ClaimSuccession,
    Command::ImportBook,
    Command::LoadOpeningBalances,
    Command::AnchorCheckpoint,
    Command::CompareSettlement,
    Command::IssueDisclosure,
    Command::SubmitEvidence,
    Command::ImportCredentials,
    Command::RegisterIntegratingParty,
    Command::TiePrincipal,
    Command::BindAsset,
    Command::ApplyChainSpec,
    Command::RegisterAlias,
    Command::RescheduleUpgrade,
    Command::CancelUpgrade,
    Command::SetAuthority,
    Command::ProposeUpgrade,
    Command::ApproveUpgrade,
    Command::ProposeMigration,
];

/// What a privileged read read (`REQ-PV-4`). Each is one place that uses the mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ReadKind {
    /// An account's balances, read without authority over it.
    Balances,
    /// The whole book, as an export (`F-014`, `REQ-RC-3`).
    Export,
    /// Every balance, compared with an external source; only mismatches returned (`F-018`,
    /// `NFR-9` axis 1(c)).
    Reconciliation,
}

impl ReadKind {
    const fn code(self) -> u8 {
        match self {
            ReadKind::Balances => 1,
            ReadKind::Export => 2,
            ReadKind::Reconciliation => 3,
        }
    }

    fn from_code(c: u8) -> Option<ReadKind> {
        match c {
            1 => Some(ReadKind::Balances),
            2 => Some(ReadKind::Export),
            3 => Some(ReadKind::Reconciliation),
            _ => None,
        }
    }
}

/// What the record is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditAction {
    /// A command, accepted or refused.
    Command(Command),
    /// A lifecycle transition that carries its own attribution (`F-007`, `F-010`).
    Transition {
        /// The state left.
        from: State,
        /// The state entered.
        to: State,
        /// What moved it.
        trigger: Trigger,
    },
    /// A privileged read.
    PrivilegedRead(ReadKind),
}

/// What an audit record is about. Never a credential (`REQ-AU-8`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AuditSubject {
    /// A ledger account.
    Account(AccountId),
    /// An identity.
    Identity(IdentityId),
    /// An operation.
    Operation(OperationId),
    /// An asset.
    Asset(AssetId),
    /// A posting, by its sequence number (`REQ-PO-2`).
    Posting(u64),
    /// An authority grant.
    Grant(GrantId),
    /// The reviewed change a registry change carries (`REQ-AS-2`), by its reference.
    Review([u8; 32]),
    /// The evidence an operator cited, by its digest (`REQ-AT-3`, `INV-11`).
    Evidence([u8; 32]),
}

/// How the audited action ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditOutcome {
    /// It was accepted.
    Accepted,
    /// It was refused, with this condition.
    Refused(Refusal),
}

/// An operator's reason for an action (`REQ-OS-1`, `REQ-AT-3`). Never blank. It lives only in the
/// audit trail.
#[derive(Clone, PartialEq, Eq)]
pub struct Justification(Vec<u8>);

impl Justification {
    /// A justification, or `ERR-JustificationRequired` if it is empty or only whitespace.
    pub fn new(text: &[u8]) -> Result<Justification, Refusal> {
        if text.iter().all(u8::is_ascii_whitespace) {
            return Err(Refusal::JustificationRequired);
        }
        Ok(Justification(text.to_vec()))
    }

    /// Its text.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl core::fmt::Debug for Justification {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Justification({} bytes)", self.0.len())
    }
}

/// One audit record (`REQ-AT-1`; `F-013` §1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// Its position in the chain, from 0, without gaps.
    pub audit_seq: u64,
    /// The hash of the record before it; zero for the first.
    pub prev_hash: [u8; 32],
    /// Which of the five classes.
    pub class: AuditClass,
    /// What it is of.
    pub action: AuditAction,
    /// Every authority relied on, with the principal that used it (`REQ-AU-7`). The first is the
    /// initiator's. For a refusal, the authority the command would have used.
    pub authorities: Vec<AuthorityUsed>,
    /// What it is about.
    pub subject: Vec<AuditSubject>,
    /// The operation it belongs to.
    pub operation_ref: OperationId,
    /// The time input of the transition (`REQ-RT-7`).
    pub at: Time,
    /// The justification, where one is required.
    pub justification: Option<Justification>,
    /// Accepted, or the named refusal.
    pub outcome: AuditOutcome,
}

impl AuditRecord {
    /// The initiating principal.
    pub fn principal(&self) -> Option<IdentityId> {
        self.authorities.first().map(|a| a.principal)
    }

    /// Whom the initiator acted for.
    pub fn on_behalf_of(&self) -> Option<IdentityId> {
        self.authorities.first().and_then(|a| a.on_behalf_of)
    }

    /// The kind of authority the initiator used.
    pub fn authority_used(&self) -> Option<AuthorityKind> {
        self.authorities.first().map(|a| a.kind)
    }

    /// The canonical encoding: every field in order, each length-prefixed, identifiers in their
    /// surface form. The hash, the audit store's files and the property tests read this.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        e.u64(self.audit_seq);
        e.bytes(&self.prev_hash);
        e.u8(self.class.code());
        match self.action {
            AuditAction::Command(c) => {
                e.u8(1);
                e.u8(command_code(c));
            }
            AuditAction::Transition { from, to, trigger } => {
                e.u8(2);
                e.u8(from.code());
                e.u8(to.code());
                e.u8(trigger.code());
            }
            AuditAction::PrivilegedRead(k) => {
                e.u8(3);
                e.u8(k.code());
            }
        }
        e.u64(self.authorities.len() as u64);
        for a in &self.authorities {
            e.u8(authority_code(a.kind));
            e.opt_str(a.grant.map(|g| g.to_did()));
            e.str(&a.principal.to_did());
            e.str(&a.credential.to_did());
            e.opt_str(a.on_behalf_of.map(|i| i.to_did()));
        }
        e.u64(self.subject.len() as u64);
        for s in &self.subject {
            match s {
                AuditSubject::Account(a) => {
                    e.u8(1);
                    e.str(&a.to_did());
                }
                AuditSubject::Identity(i) => {
                    e.u8(2);
                    e.str(&i.to_did());
                }
                AuditSubject::Operation(o) => {
                    e.u8(3);
                    e.bytes(&o.to_ref().0);
                }
                AuditSubject::Asset(a) => {
                    e.u8(4);
                    e.str(a.as_str());
                }
                AuditSubject::Posting(p) => {
                    e.u8(5);
                    e.u64(*p);
                }
                AuditSubject::Grant(g) => {
                    e.u8(6);
                    e.str(&g.to_did());
                }
                AuditSubject::Review(r) => {
                    e.u8(7);
                    e.bytes(r);
                }
                AuditSubject::Evidence(d) => {
                    e.u8(8);
                    e.bytes(d);
                }
            }
        }
        e.bytes(&self.operation_ref.to_ref().0);
        e.u64(self.at);
        match &self.justification {
            None => e.u8(0),
            Some(j) => {
                e.u8(1);
                e.bytes(j.as_bytes());
            }
        }
        match self.outcome {
            AuditOutcome::Accepted => e.u8(0),
            AuditOutcome::Refused(r) => {
                e.u8(1);
                e.str(r.code());
            }
        }
        e.0
    }

    /// The record [`AuditRecord::encode`] wrote, or `None` if the bytes are not one.
    pub fn decode(bytes: &[u8]) -> Option<AuditRecord> {
        let mut d = Dec(bytes);
        let audit_seq = d.u64()?;
        let prev_hash: [u8; 32] = d.bytes()?.try_into().ok()?;
        let class = AuditClass::from_code(d.u8()?)?;
        let action = match d.u8()? {
            1 => {
                let c = d.u8()?;
                AuditAction::Command(COMMANDS.into_iter().find(|k| command_code(*k) == c)?)
            }
            2 => {
                let from = state_of(d.u8()?)?;
                let to = state_of(d.u8()?)?;
                let t = d.u8()?;
                let trigger = Trigger::ALL.into_iter().find(|k| k.code() == t)?;
                AuditAction::Transition { from, to, trigger }
            }
            3 => AuditAction::PrivilegedRead(ReadKind::from_code(d.u8()?)?),
            _ => return None,
        };
        let n = d.u64()?;
        let mut authorities = Vec::new();
        for _ in 0..n {
            let kind = authority_of(d.u8()?)?;
            let grant = match d.opt_str()? {
                None => None,
                Some(s) => Some(GrantId::parse(&s).ok()?),
            };
            let principal = IdentityId::parse(&d.str()?).ok()?;
            let credential = CredentialId::parse(&d.str()?).ok()?;
            let on_behalf_of = match d.opt_str()? {
                None => None,
                Some(s) => Some(IdentityId::parse(&s).ok()?),
            };
            authorities.push(AuthorityUsed {
                kind,
                grant,
                principal,
                credential,
                on_behalf_of,
            });
        }
        let n = d.u64()?;
        let mut subject = Vec::new();
        for _ in 0..n {
            subject.push(match d.u8()? {
                1 => AuditSubject::Account(AccountId::parse(&d.str()?).ok()?),
                2 => AuditSubject::Identity(IdentityId::parse(&d.str()?).ok()?),
                3 => AuditSubject::Operation(op_of(d.bytes()?)?),
                4 => AuditSubject::Asset(AssetId::parse(&d.str()?).ok()?),
                5 => AuditSubject::Posting(d.u64()?),
                6 => AuditSubject::Grant(GrantId::parse(&d.str()?).ok()?),
                7 => AuditSubject::Review(d.bytes()?.try_into().ok()?),
                8 => AuditSubject::Evidence(d.bytes()?.try_into().ok()?),
                _ => return None,
            });
        }
        let operation_ref = op_of(d.bytes()?)?;
        let at = d.u64()?;
        let justification = match d.u8()? {
            0 => None,
            1 => Some(Justification::new(d.bytes()?).ok()?),
            _ => return None,
        };
        let outcome = match d.u8()? {
            0 => AuditOutcome::Accepted,
            1 => {
                let code = d.str()?;
                AuditOutcome::Refused(Refusal::ALL.into_iter().find(|r| r.code() == code)?)
            }
            _ => return None,
        };
        if !d.0.is_empty() {
            return None;
        }
        Some(AuditRecord {
            audit_seq,
            prev_hash,
            class,
            action,
            authorities,
            subject,
            operation_ref,
            at,
            justification,
            outcome,
        })
    }

    /// The record's hash, which the next record carries: `blake2b-256("aledgent/audit/v1" ‖
    /// encoding)`.
    pub fn hash(&self) -> [u8; 32] {
        blake2b_256(RECORD_DOMAIN, &[&self.encode()])
    }
}

const fn authority_code(k: AuthorityKind) -> u8 {
    match k {
        AuthorityKind::Owner => 1,
        AuthorityKind::Delegated => 2,
        AuthorityKind::Operator => 3,
        AuthorityKind::Issuance => 4,
        AuthorityKind::Recovery => 5,
    }
}

fn authority_of(c: u8) -> Option<AuthorityKind> {
    [
        AuthorityKind::Owner,
        AuthorityKind::Delegated,
        AuthorityKind::Operator,
        AuthorityKind::Issuance,
        AuthorityKind::Recovery,
    ]
    .into_iter()
    .find(|k| authority_code(*k) == c)
}

fn state_of(c: u8) -> Option<State> {
    State::ALL.into_iter().find(|s| s.code() == c)
}

fn op_of(b: &[u8]) -> Option<OperationId> {
    Some(OperationId::from_ref(OperationRef(b.try_into().ok()?)))
}

/// The canonical encoder: fixed-width integers, length-prefixed byte strings.
#[derive(Default)]
struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    fn opt_str(&mut self, s: Option<String>) {
        match s {
            None => self.u8(0),
            Some(s) => {
                self.u8(1);
                self.str(&s);
            }
        }
    }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u64()?).ok()?;
        self.take(n)
    }

    fn str(&mut self) -> Option<String> {
        core::str::from_utf8(self.bytes()?).ok().map(String::from)
    }

    fn opt_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
