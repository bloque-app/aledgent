// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The wire form of the command, query and notification surfaces (`CTR-CMD`, `CTR-QRY`,
//! `CTR-EVT`): JSON, described by OpenAPI 3.1 generated from these types (accepted proposal of
//! `F-009`).
//!
//! - Amounts are decimal-integer strings of **minor units** (`REQ-AS-4`, `INV-7`): `"1050"` is
//!   ten-fifty of an asset of precision 2. No decimal point, no exponent, no float anywhere.
//! - Identifiers are `did:aledgent:<kind>:<body>` strings (`REQ-ID-1`); asset ids are their
//!   registry symbols.
//! - Times are milliseconds since the Unix epoch, the unit of the ingress clock (`DEC-6`).
//! - A refusal is a 4xx whose body is `{"error": "ERR-…", "description": "…"}`, never a 2xx
//!   (`REQ-CS-4`, `REQ-ER-1`): [`ledger_core::Refusal`]'s own serialised shape.
//!
//! Every body refuses fields it does not know, so a misspelt field is never silently dropped.

use std::collections::BTreeMap;

use ledger_core::Refusal;
use ledger_core::lifecycle::{Shown, State, StateClass};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

// ------------------------------------------------------------------------------------ commands

/// One leg of a transfer: `amount` of `asset` from `from` to `to`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LegBody {
    /// The debited ledger account.
    pub from: String,
    /// The credited ledger account.
    pub to: String,
    /// The asset.
    pub asset: String,
    /// Minor units, as a decimal-integer string.
    pub amount: String,
}

/// `transfer` (SPEC §8.1): one leg or many, all or nothing (`US-B1`, `US-B3`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferBody {
    /// The legs, in the order they apply.
    pub legs: Vec<LegBody>,
    /// The operation's deadline; the deployment's default for transfers if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
    /// Pass-through data (`REQ-PO-5`): one JSON value of at most 1 KiB, the caller's own, echoed
    /// wherever its operation is read and never read by aledgent. Part of the request: a repeat
    /// under the same client key with a different one is `ERR-ClientKeyReused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// `reserve` (SPEC §8.1, `REQ-BA-4`): acknowledged at `Received`; the decision follows by query and
/// notification (`REQ-CS-3`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReserveBody {
    /// The account the value is reserved against.
    pub payer: String,
    /// The asset.
    pub asset: String,
    /// Minor units, as a decimal-integer string.
    pub amount: String,
    /// The operation's deadline; the deployment's default for reservations if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
    /// Pass-through data (`REQ-PO-5`): one JSON value of at most 1 KiB, the caller's own, echoed
    /// wherever its operation is read and never read by aledgent. Part of the request: a repeat
    /// under the same client key with a different one is `ERR-ClientKeyReused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// `capture` of the reservation named in the path (SPEC §8.1, `INV-14`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureBody {
    /// The account credited.
    pub destination: String,
    /// The amount cleared, in minor units. It must equal the reservation (`INV-14`).
    pub cleared: String,
}

/// A command with nothing in its body but what its path names (`release`, `close`, `revoke`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyBody {}

/// `issue` (SPEC §8.1, `REQ-AU-10`): value enters the book from the issuance counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueBody {
    /// The asset.
    pub asset: String,
    /// The account credited.
    pub destination: String,
    /// Minor units, as a decimal-integer string.
    pub amount: String,
    /// Pass-through data (`REQ-PO-5`): one JSON value of at most 1 KiB, the caller's own, echoed
    /// wherever its operation is read and never read by aledgent. Part of the request: a repeat
    /// under the same client key with a different one is `ERR-ClientKeyReused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// `redeem` (SPEC §8.1): value leaves the book to the issuance counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RedeemBody {
    /// The asset.
    pub asset: String,
    /// The account debited.
    pub source: String,
    /// Minor units, as a decimal-integer string.
    pub amount: String,
    /// Pass-through data (`REQ-PO-5`): one JSON value of at most 1 KiB, the caller's own, echoed
    /// wherever its operation is read and never read by aledgent. Part of the request: a repeat
    /// under the same client key with a different one is `ERR-ClientKeyReused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// `withdraw` (SPEC §8.1, `F-010`): an outbound leg, reserved against the payer first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WithdrawBody {
    /// The account the value leaves.
    pub payer: String,
    /// The asset.
    pub asset: String,
    /// Minor units, as a decimal-integer string.
    pub amount: String,
    /// The operation's deadline; the deployment's default if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
    /// Pass-through data (`REQ-PO-5`): one JSON value of at most 1 KiB, the caller's own, echoed
    /// wherever its operation is read and never read by aledgent. Part of the request: a repeat
    /// under the same client key with a different one is `ERR-ClientKeyReused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// The kind of an identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IdentityKindBody {
    /// A natural person.
    Person,
    /// An organisation.
    Organisation,
    /// An agent, acting for a named identity (`REQ-AG-1`).
    Agent,
}

/// `open_identity` (SPEC §8.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenIdentityBody {
    /// Its kind.
    pub kind: IdentityKindBody,
    /// For an agent: the identity it acts for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acts_for: Option<String>,
    /// The first credential's verifier: public material only, hex-encoded. Never a secret.
    pub verifier: String,
    /// Its scheme (`DEC-19`, `F-028`): `key` when absent, `webauthn-es256` or `api-key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// Pass-through data (`REQ-PO-5`), as on a transfer: the caller's own, at most 1 KiB, echoed
    /// in the reply and in `/v1/operations/{reference}`. The identities principal puts its own
    /// justification here, such as the registration it opens this for (amendment 0025 R1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// `register_integrating_party` on the operator surface (`REQ-ID-7`, amendment 0011): the
/// operator's policy authority alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterIntegratingPartyBody {
    /// The party: its organisation identity, `did:`-shaped.
    pub party: String,
    /// The principals tied to it on registration, each tied to no party. May be empty.
    #[serde(default)]
    pub principals: Vec<String>,
    /// Why.
    pub justification: String,
}

/// `tie_principal` to the party in the path (`REQ-ID-7`, amendment 0011): from then on the
/// principal's client keys are the party's (`REQ-OP-10`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TiePrincipalBody {
    /// The principal, `did:`-shaped.
    pub principal: String,
    /// Why.
    pub justification: String,
}

/// A principal tied to an integrating party.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PartyPrincipalBody {
    /// The principal, `did:`-shaped.
    pub principal: String,
    /// When it was tied.
    pub tied_at: u64,
}

/// A registered integrating party, as the operator lists it (`REQ-ID-7`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IntegratingPartyBody {
    /// The party: its organisation identity, `did:`-shaped.
    pub party: String,
    /// When it was registered.
    pub registered_at: u64,
    /// Its principals, in identifier order.
    pub principals: Vec<PartyPrincipalBody>,
}

/// `GET /v1/ops/integrating-parties`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IntegratingPartiesBody {
    /// Every registered party, in identifier order.
    pub items: Vec<IntegratingPartyBody>,
}

/// `register_alias` (`REQ-RC-7`, amendment 0024): name the account in the path by (source, name),
/// as a seed's aliases do. A system account's role is one (`ops/runbooks/system-accounts.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterAliasBody {
    /// The source: 16 bytes as 32 hex digits, or a name of at most 16 ASCII bytes, zero-padded
    /// (`system-role` for a system account's role, as the migration's role aliases).
    pub source: String,
    /// The name within the source: a role (`issuer/fee`) or a prior system's identifier. At most
    /// 256 bytes; never personal data (`INV-17`).
    pub name: String,
    /// Why.
    pub justification: String,
}

/// `upgrade` (`REQ-RT-16`, `T-037-04`): the writer's code becomes the code version `code` from input
/// `from`. The execution node fetches the version's manifest and artefacts and checks them
/// (`REQ-RT-17`, `REQ-RT-20`) before the command is logged; the writer decides it under the
/// upgrade-authority mode in force (`REQ-RT-18`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpgradeBody {
    /// The code version: the `blake2b-256` of its manifest, as 64 hex digits.
    pub code: String,
    /// The input number it applies from: later than the command's own.
    pub from: u64,
    /// Why.
    pub justification: String,
}

/// `reschedule_upgrade` (`REQ-RT-20`, `T-037-15`): move the scheduled upgrade to input `from`, later
/// than the command's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RescheduleUpgradeBody {
    /// The input number the upgrade now applies from.
    pub from: u64,
    /// Why.
    pub justification: String,
}

/// `alias(source, name)` (`REQ-RC-7`): the account an alias names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AliasBody {
    /// The account, `did:`-shaped.
    pub account: String,
}

/// One grant over an account, as `grants(account)` shows it (`REQ-RC-7`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GrantViewBody {
    /// The grant, `did:`-shaped.
    pub grant: String,
    /// Who granted it.
    pub grantor: String,
    /// When it was granted.
    pub granted_at: u64,
    /// When it was revoked, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked: Option<u64>,
    /// What it says: kind, holder, subject, operations, bounds, expiry.
    pub terms: GrantBody,
}

/// `grants(account)` (`REQ-RC-7`): every grant whose subject is the account, live and revoked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GrantsBody {
    /// The account, `did:`-shaped.
    pub account: String,
    /// Its grants, in identifier order.
    pub items: Vec<GrantViewBody>,
}

/// The status of a ledger account (SPEC §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatusBody {
    /// It transacts.
    Active,
    /// It is frozen.
    Frozen,
    /// It is closing: no new movements.
    Closing,
    /// It is closed for good.
    Closed,
}

/// `account(account)` (SPEC §8.2, amendment 0028): an account as the book holds it, so a service
/// can check whose it is before it adopts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountViewBody {
    /// The account, `did:`-shaped.
    pub account: String,
    /// The one identity that owns it, `did:aledgent:id:…`.
    pub owner: String,
    /// Its kind.
    pub kind: AccountKindBody,
    /// Its custody mode.
    pub custody: CustodyBody,
    /// Its status.
    pub status: AccountStatusBody,
    /// Whether it is closed for good: `status` is `closed`.
    pub closed: bool,
    /// When it was opened, in milliseconds since the epoch.
    pub opened_at: u64,
}

/// `grants(identity)` (amendment 0026): the grants whose subject is the identity, live and
/// revoked: every one for the identity itself, the operator and the identities principal; the
/// caller's own for anyone else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IdentityGrantsBody {
    /// The identity, `did:`-shaped.
    pub identity: String,
    /// The grants, in identifier order.
    pub items: Vec<GrantViewBody>,
}

/// One credential of an import (`REQ-MG-7`, amendment 0010, `F-028`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifierBody {
    /// The scheme: `key`, `webauthn-es256` or `api-key`.
    pub scheme: String,
    /// The verifier: public material only, hex-encoded.
    pub verifier: String,
    /// The accounts it is limited to; absent for all of the identity's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
}

/// `import_credentials` of the identity in the path, on the operator surface (`REQ-MG-7`,
/// amendment 0010, `F-028`). A passkey is accepted only if its `rpId` is one the deployment
/// configures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportCredentialsBody {
    /// The credentials, in order.
    pub credentials: Vec<VerifierBody>,
    /// Why: the migration it belongs to.
    pub justification: String,
}

/// The kind of a ledger account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountKindBody {
    /// Never negative.
    Customer,
    /// May be negative down to a declared limit per asset.
    Treasury,
    /// Value whose destination is not yet known.
    Suspense,
}

/// The custody mode of a ledger account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CustodyBody {
    /// The operator may act, under bounded operator authority.
    Delegated,
    /// No operator authority exists over the account.
    Owned,
}

/// `open_account` (SPEC §8.1, `REQ-LA-1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenAccountBody {
    /// The one owning identity.
    pub owner: String,
    /// Its kind.
    pub kind: AccountKindBody,
    /// Its custody mode.
    pub custody: CustodyBody,
    /// A Treasury account's limits below zero, per asset, in minor units.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub treasury_limits: BTreeMap<String, String>,
    /// For owned custody: the digest, 32 bytes of hex, of the recovery mechanisms the holder was
    /// shown (`GET /v1/identities/{identity}/recovery-terms`; `REQ-AU-9`, `AC-A5.3`, `F-012`).
    /// Missing or stale is `ERR-NotAuthorised`. Ignored for delegated custody.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<String>,
    /// Pass-through data (`REQ-PO-5`), as on a transfer: the caller's own, at most 1 KiB, echoed
    /// in the reply and in `/v1/operations/{reference}`. The identities principal puts its own
    /// justification here, such as the registration it opens this for (amendment 0025 R1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// The kind of a grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrantKindBody {
    /// Granted by a holder, or narrowed from such a grant.
    Delegated,
    /// Granted by the operator, over accounts in delegated custody.
    Operator,
    /// Authority over an asset: issue and redeem.
    Issuance,
}

/// What a grant is over. Exactly one field is present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SubjectBody {
    /// One ledger account.
    Account(String),
    /// Every account in delegated custody at the moment of evaluation.
    DelegatedAccounts,
    /// One asset.
    Asset(String),
    /// One identity, `did:`-shaped (amendment 0025): a delegated grant to open the accounts it
    /// will own.
    Identity(String),
}

/// The kinds of operation a grant may be used for.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum OperationKindBody {
    /// An internal transfer.
    Transfer,
    /// A reservation.
    Reserve,
    /// A capture.
    Capture,
    /// A release.
    Release,
    /// A conversion.
    Convert,
    /// An encumbrance.
    Encumber,
    /// Lifting an encumbrance.
    Disencumber,
    /// Issuance.
    Issue,
    /// Redemption.
    Redeem,
    /// Closing the account.
    CloseAccount,
    /// Issuing a disclosure about the account (`F-017`).
    Disclose,
    /// A reversal (amendment 0006 item 1): the kind of a reversal operation. A grant that names
    /// it is refused; reversal authority is held on the operator surface.
    Reversal,
    /// Opening a ledger account for an identity (amendment 0025): named by a delegated grant over
    /// an identity, alone, and by nothing else.
    OpenAccount,
}

/// A per-window bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowBody {
    /// The window's length, in milliseconds.
    pub length: u64,
    /// The most that may move in one window, in minor units.
    pub limit: String,
}

/// `grant_authority` (SPEC §8.1, `REQ-AU-2`, `REQ-AU-4`, `REQ-AU-10`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrantBody {
    /// Its kind.
    pub kind: GrantKindBody,
    /// Who will hold it.
    pub principal: String,
    /// What it is over.
    pub subject: SubjectBody,
    /// What it may be used for.
    pub ops: Vec<OperationKindBody>,
    /// The most one operation may move, in minor units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_operation: Option<String>,
    /// The most one window may move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_window: Option<WindowBody>,
    /// The assets it is limited to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assets: Option<Vec<String>>,
    /// The counterparties it is limited to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterparties: Option<Vec<String>>,
    /// The grant it narrows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// When it stops working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
}

/// `capture` as the core receives it: the reservation from the path, and the body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureCommand {
    /// The reservation's operation reference.
    pub reservation: String,
    /// The account credited.
    pub destination: String,
    /// The amount cleared, in minor units.
    pub cleared: String,
}

/// `release` as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseCommand {
    /// The reservation's operation reference.
    pub reservation: String,
}

/// `close_account` as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseAccountCommand {
    /// The account.
    pub account: String,
}

/// `revoke_authority` as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeCommand {
    /// The grant.
    pub grant: String,
}

/// A command as ingress logs it and the core receives it: every field the command's effect
/// depends on, paths included, and nothing about the transport (`REQ-CS-1`). Its canonical JSON
/// is what the received-log stores and digests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum CommandBody {
    /// `transfer`.
    Transfer(TransferBody),
    /// `reserve`.
    Reserve(ReserveBody),
    /// `capture`.
    Capture(CaptureCommand),
    /// `release`.
    Release(ReleaseCommand),
    /// `issue`.
    Issue(IssueBody),
    /// `redeem`.
    Redeem(RedeemBody),
    /// `withdraw`.
    Withdraw(WithdrawBody),
    /// `open_identity`.
    OpenIdentity(OpenIdentityBody),
    /// `open_account`.
    OpenAccount(OpenAccountBody),
    /// `close_account`.
    CloseAccount(CloseAccountCommand),
    /// `grant_authority`.
    GrantAuthority(GrantBody),
    /// `revoke_authority`.
    RevokeAuthority(RevokeCommand),
    /// `CTR-OPS`: an operator command (`F-013`), routed.
    Operator(OperatorCommand),
    /// `register_credential` (`F-012`).
    RegisterCredential(RegisterCredentialBody),
    /// `remove_credential` (`F-012`).
    RemoveCredential(RemoveCredentialCommand),
    /// `delegate_custody` (`F-012`).
    DelegateCustody(AccountCommand),
    /// `reclaim_custody` (`F-012`).
    ReclaimCustody(ReclaimCustodyCommand),
    /// `cancel_recovery`: a pending recovery or succession (`F-012`).
    CancelRecovery(PendingCommand),
    /// `name_successor` (`F-012`).
    NameSuccessor(NameSuccessorCommand),
    /// `claim_succession` (`F-012`).
    ClaimSuccession(AccountCommand),
    /// `submit_outbound` (`F-010`, `F-016`).
    SubmitOutbound(crate::settlement::SubmitOutboundCommand),
    /// `CTR-OPS`: `configure_settlement` (`F-010`, `F-016`).
    ConfigureSettlement(crate::settlement::ConfigureSettlementBody),
    /// An input of the settlement adapter's (`F-016`).
    Adapter(crate::settlement::AdapterCommandBody),
    /// `issue_disclosure`, `anchor_checkpoint` or `compare_settlement` (`F-017`).
    Anchoring(crate::anchoring::AnchoringCommandBody),
}

// ------------------------------------------------------------------------------------ custody

/// What kind of member of the holder's credential set (`REQ-AU-20`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMarkerBody {
    /// An everyday credential.
    Ordinary,
    /// A holder-held recovery credential, kept away from everyday devices (`REQ-AU-18`).
    RecoveryHeld,
}

/// `register_credential` (SPEC §8.1, `REQ-AU-20`, `F-012`): a new member of the caller's own set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterCredentialBody {
    /// Everyday or holder-held recovery.
    pub marker: CredentialMarkerBody,
    /// The accounts it is limited to; absent for all of the identity's. Within the presenting
    /// credential's own scope (`REQ-AU-4`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    /// Its verifier: public material, hex. A key that can sign a reclaim proof is 32 bytes.
    pub verifier: String,
    /// Its scheme (`DEC-19`, `F-028`): `key` when absent, `webauthn-es256` or `api-key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
}

/// `POST /v1/credentials` as a caller sends it (`F-028`): a credential of any scheme. Ingress turns
/// it into the [`RegisterCredentialBody`] the core receives, which holds only public material.
///
/// - `key` (the default): `verifier` is the 32-byte public key, hex.
/// - `webauthn-es256`: `passkey` carries the browser's registration, made over a challenge from
///   `POST /v1/auth/challenge` with purpose `register`. Ingress verifies it.
/// - `api-key`: nothing else. Ingress draws the secret, keeps its hash, and returns the key once,
///   in `api_key` of the reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterCredentialRequest {
    /// Everyday or holder-held recovery.
    pub marker: CredentialMarkerBody,
    /// The accounts it is limited to; absent for all of the identity's (`REQ-AU-20`), within the
    /// presenting credential's own scope (`REQ-AU-4`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    /// `key` when absent, `webauthn-es256` or `api-key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// A `key` verifier, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<String>,
    /// A passkey registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passkey: Option<PasskeyRegistrationBody>,
}

/// A WebAuthn registration, as `navigator.credentials.create` returned it (`F-028`). Base64url.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PasskeyRegistrationBody {
    /// `response.clientDataJSON`, base64url.
    pub client_data_json: String,
    /// `response.attestationObject`, base64url.
    pub attestation_object: String,
}

/// A WebAuthn assertion, as `navigator.credentials.get` returned it (`F-028`). Base64url.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssertionBody {
    /// `rawId`, base64url.
    pub raw_id: String,
    /// `response.authenticatorData`, base64url.
    pub authenticator_data: String,
    /// `response.clientDataJSON`, base64url.
    pub client_data_json: String,
    /// `response.signature`, base64url (DER).
    pub signature: String,
}

/// `remove_credential` as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveCredentialCommand {
    /// The credential.
    pub credential: String,
}

/// A command about one account named in its path (`delegate_custody`, `claim_succession`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountCommand {
    /// The account.
    pub account: String,
}

/// `reclaim_custody` of the account in the path (`REQ-AU-13`, `F-012`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReclaimCustodyBody {
    /// The credential that proves it works: an active member of the holder's set covering the
    /// account.
    pub proof_credential: String,
    /// For a `key` credential: its signature, 64 bytes of hex, over the challenge that binds the
    /// account and its custody epoch, as the custody view shows it
    /// (`ledger_core::custody::reclaim_challenge`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// For a passkey (`F-028`): its assertion, whose challenge is that same 32-byte
    /// `reclaim_challenge`. Ingress verifies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion: Option<AssertionBody>,
    /// The digest, 32 bytes of hex, of the account's recovery mechanisms as shown
    /// (`GET /v1/accounts/{account}/recovery-mechanisms`; `AC-A5.3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<String>,
}

/// `reclaim_custody` as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReclaimCustodyCommand {
    /// The account.
    pub account: String,
    /// The proving credential.
    pub proof_credential: String,
    /// Its signature, hex: a `key` credential's proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// A passkey's proof, checked by ingress before the command was logged (`F-028`, `DEC-19`).
    /// Only ingress sets it; no caller's body can.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub passkey_checked: bool,
    /// The acknowledgement, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<String>,
}

/// A command about one pending recovery or succession named in its path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PendingCommand {
    /// The pending change: the operation reference that opened it.
    pub pending: String,
}

/// `name_successor` of the account in the path (`REQ-AU-17`, `F-012`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NameSuccessorBody {
    /// The successor; absent to clear the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<String>,
}

/// `name_successor` as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NameSuccessorCommand {
    /// The account.
    pub account: String,
    /// The successor, or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<String>,
}

/// `initiate_recovery` of the account in the path, on the operator surface (`REQ-AU-15`,
/// `REQ-AU-16`, `F-012`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitiateRecoveryBody {
    /// The verifier, hex, of the credential the recovery will issue to the holder.
    pub verifier: String,
    /// Its scheme (`F-028`): `key` when absent. A passkey's verifier comes from a registration
    /// the holder's new device made (`POST /v1/credentials` validates the same form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// Its scope: the recovered account among the owner's; absent for all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    /// Why: the holder's out-of-band request.
    pub justification: String,
}

/// `complete_recovery` of the pending recovery in the path, on the operator surface
/// (`REQ-AU-15`, `F-012`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteRecoveryBody {
    /// The recovery authority's approval, 64 bytes of hex: its signature over the pending change,
    /// the account and the new verifier (`ledger_core::custody::approval_message`).
    pub approval: String,
    /// Why.
    pub justification: String,
}

/// One recovery mechanism that applies (`REQ-AU-9`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mechanism", rename_all = "snake_case", deny_unknown_fields)]
pub enum MechanismBody {
    /// The recovery authority, after its delay.
    RecoveryAuthority {
        /// How the audit trail names it.
        authority: String,
        /// The delay, milliseconds.
        delay: u64,
    },
    /// A named successor's claim, after its delay.
    Succession {
        /// The delay, milliseconds.
        delay: u64,
        /// The successor named, if one is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        named: Option<String>,
    },
    /// The holder's holder-held recovery credentials. No delay.
    HolderHeld {
        /// The credentials.
        credentials: Vec<String>,
    },
}

/// The recovery mechanisms that apply, and the digest a command that makes an account owned
/// carries as its acknowledgement (`REQ-AU-9`, `AC-A5.3`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryMechanismsBody {
    /// The mechanisms.
    pub mechanisms: Vec<MechanismBody>,
    /// Their digest, 32 bytes of hex.
    pub digest: String,
}

/// One recovery or succession recorded for an account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PendingChangeBody {
    /// Its identity: the operation reference that opened it.
    pub pending: String,
    /// `recovery` or `succession`.
    pub kind: String,
    /// `pending`, `cancelled` or `completed`.
    pub state: String,
    /// When it was opened.
    pub initiated_at: u64,
    /// When it may complete.
    pub effective_at: u64,
}

/// An account's custody as its authority holders see it (`INV-20`, `REQ-AU-16`, `F-012`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CustodyViewBody {
    /// The account.
    pub account: String,
    /// Its owner.
    pub owner: String,
    /// Its custody mode.
    pub custody: CustodyBody,
    /// The successor named, if one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<String>,
    /// Its custody epoch: how many custody changes it has had. A reclaim proof signs it.
    pub epoch: u64,
    /// Every recovery and succession recorded for it, with its `effective_at`.
    pub changes: Vec<PendingChangeBody>,
}

// ------------------------------------------------------------------------------------ operator

/// A body that carries only the operator's justification (`REQ-OS-1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JustifiedBody {
    /// Why. Required: an operator command without one is `ERR-JustificationRequired`.
    pub justification: String,
}

/// What a `Needs-Review` operation resolves to: `Settled` or `Released` only (amendment 0006
/// items 1 and 4). Value to return after that is a reversal operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionBody {
    /// `Settled`.
    Settle,
    /// `Released`.
    Release,
}

/// A settlement-layer record cited by its coordinate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoordinateBody {
    /// The instance.
    pub instance: u32,
    /// The position in its inbox.
    pub seq: u64,
    /// The entry's digest, hex.
    pub digest: String,
}

/// The evidence a resolution cites. Exactly one field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionEvidenceBody {
    /// A record the settlement store ingested.
    Record(CoordinateBody),
    /// Non-inclusion of the operation's instruction under the last finalised head.
    NonInclusion,
    /// No mechanical proof (`REQ-RC-5`, step 3): only after the settlement adapter checked
    /// submitted evidence and found none, and only with a second, distinct principal agreeing.
    NoProof,
}

/// `submit_evidence` for the operation in the path (`REQ-RC-5`, step 2): where on the settlement
/// layer the proof of its outcome is, for the settlement adapter to fetch and verify.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitEvidenceBody {
    /// The settlement layer's block.
    pub block: u64,
    /// The transaction's hash in it, 32 bytes of hex.
    pub transaction: String,
    /// Why.
    pub justification: String,
}

/// `resolve_operation` of the operation in the path (`REQ-OS-2`, `F-013`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveBody {
    /// What it resolves to.
    pub target: ResolutionBody,
    /// The evidence cited.
    pub evidence: ResolutionEvidenceBody,
    /// Why.
    pub justification: String,
}

/// `register_asset` (`REQ-AS-2`, `F-013`): an asset, registered book-only (amendment 0011,
/// `REQ-AS-7`), with its one precision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterAssetBody {
    /// Its symbol.
    pub id: String,
    /// Its one precision (`INV-24`).
    pub precision: u8,
    /// `fiat`, `stable`, `native` or `other`.
    pub kind: String,
    /// The reviewed change it comes from, 32 bytes of hex.
    pub review: String,
    /// Why.
    pub justification: String,
}

/// `deprecate_asset` of the asset in the path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeprecateAssetBody {
    /// The reviewed change it comes from, 32 bytes of hex.
    pub review: String,
    /// Why.
    pub justification: String,
}

/// `bind_asset` of the asset in the path (amendment 0011, `REQ-AS-8`, `DEC-20`): bind a
/// book-only asset to the settlement layer, under issuance authority over it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BindAssetBody {
    /// Its location on the settlement layer, in the layer's textual form (`Here(100)`).
    pub location: String,
    /// Why.
    pub justification: String,
}

/// `grant_issuance` (`REQ-AU-10`, `F-013`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrantIssuanceBody {
    /// The grant.
    pub grant: GrantBody,
    /// Why.
    pub justification: String,
}

/// The operator capabilities a grant may carry (`REQ-OS-1`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum OperatorOpKindBody {
    /// Resolve a `Needs-Review` operation.
    ResolveOperation,
    /// Resume an open operation past its deadline.
    Resume,
    /// Propose a reversal.
    ProposeReversal,
    /// Approve a reversal.
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
    /// Declare the settlement adapter's support.
    DeclareAdapterSupport,
    /// Read through the privileged-read mechanism.
    PrivilegedRead,
    /// Import a whole export into an empty deployment (`F-014`).
    ImportBook,
    /// Load opening balances (`F-014`).
    LoadOpeningBalances,
    /// Create a checkpoint on demand (`F-017`).
    AnchorCheckpoint,
    /// Compare the book with the settlement layer on request (`F-017`).
    CompareSettlement,
    /// Issue a disclosure under a legal obligation (`F-017`).
    LegalDisclosure,
    /// Read the operational signals: counts and ages (`F-018`).
    ReadSignals,
    /// Import credentials into an identity's set (`F-028`, `REQ-MG-7`).
    ImportCredentials,
    /// Open identities, and ledger accounts for any identity: the identities principal's role
    /// (`REQ-ID-6`, amendment 0025).
    OpenIdentity,
}

/// `grant_operator` (`REQ-OS-1`, `F-013`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrantOperatorBody {
    /// Who will hold it.
    pub principal: String,
    /// The capabilities, each its own (`REQ-OS-1`: no "all of the above").
    pub kinds: Vec<OperatorOpKindBody>,
    /// When it stops working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    /// Why.
    pub justification: String,
}

/// One (operation kind, asset) the settlement adapter supports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterLegBody {
    /// The kind.
    pub kind: OperationKindBody,
    /// The asset.
    pub asset: String,
}

/// `declare_adapter_support` (`REQ-SA-1`, `F-013`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterSupportBody {
    /// Every supported pair; the declaration replaces the last.
    pub legs: Vec<AdapterLegBody>,
    /// Why.
    pub justification: String,
}

/// An operator command as the core receives it: its target from the path, and its body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OperatorCommand {
    /// Freeze an account.
    FreezeAccount {
        /// The account.
        account: String,
        /// Why.
        justification: String,
    },
    /// Unfreeze an account.
    UnfreezeAccount {
        /// The account.
        account: String,
        /// Why.
        justification: String,
    },
    /// Resume an open operation.
    Resume {
        /// The operation.
        operation: String,
        /// Why.
        justification: String,
    },
    /// Resolve a `Needs-Review` operation.
    Resolve {
        /// The operation.
        operation: String,
        /// What it resolves to.
        target: ResolutionBody,
        /// The evidence cited.
        evidence: ResolutionEvidenceBody,
        /// Why.
        justification: String,
    },
    /// Submit evidence for a `Needs-Review` operation.
    SubmitEvidence {
        /// The operation.
        operation: String,
        /// The settlement layer's block.
        block: u64,
        /// The transaction's hash, hex.
        transaction: String,
        /// Why.
        justification: String,
    },
    /// Propose a reversal of an operation.
    ProposeReversal {
        /// The operation.
        operation: String,
        /// Why.
        justification: String,
    },
    /// Approve a proposed reversal.
    ApproveReversal {
        /// The proposal.
        proposal: String,
        /// Why.
        justification: String,
    },
    /// Register an asset.
    RegisterAsset(RegisterAssetBody),
    /// Deprecate an asset.
    DeprecateAsset {
        /// The asset.
        asset: String,
        /// The reviewed change, hex.
        review: String,
        /// Why.
        justification: String,
    },
    /// Bind a book-only asset to the settlement layer (amendment 0011, `REQ-AS-8`).
    BindAsset {
        /// The asset.
        asset: String,
        /// Its location on the layer.
        location: String,
        /// Why.
        justification: String,
    },
    /// Grant issuance authority.
    GrantIssuance(GrantIssuanceBody),
    /// Revoke issuance authority.
    RevokeIssuance {
        /// The grant.
        grant: String,
        /// Why.
        justification: String,
    },
    /// Grant operator authority.
    GrantOperator(GrantOperatorBody),
    /// Revoke operator authority.
    RevokeOperator {
        /// The grant.
        grant: String,
        /// Why.
        justification: String,
    },
    /// Declare the adapter's support.
    DeclareAdapterSupport(AdapterSupportBody),
    /// Initiate the recovery of an owned-custody account (`F-012`).
    InitiateRecovery {
        /// The account.
        account: String,
        /// The new credential's verifier, hex.
        verifier: String,
        /// Its scheme (`F-028`): `key` when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
        /// Its scope; absent for all of the owner's accounts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<Vec<String>>,
        /// Why.
        justification: String,
    },
    /// Import credentials into an identity's set (`REQ-MG-7`, `F-028`).
    ImportCredentials {
        /// The identity.
        identity: String,
        /// The credentials, in order.
        credentials: Vec<VerifierBody>,
        /// Why.
        justification: String,
    },
    /// Register an integrating party with the principals tied to it (`REQ-ID-7`).
    RegisterIntegratingParty {
        /// The party: its organisation identity.
        party: String,
        /// The principals tied to it.
        principals: Vec<String>,
        /// Why.
        justification: String,
    },
    /// Tie a principal to a registered party (`REQ-ID-7`).
    TiePrincipal {
        /// The party.
        party: String,
        /// The principal.
        principal: String,
        /// Why.
        justification: String,
    },
    /// Name an account by an alias (`REQ-RC-7`, amendment 0024).
    RegisterAlias {
        /// The account.
        account: String,
        /// The source ([`RegisterAliasBody::source`]).
        source: String,
        /// The name within it.
        name: String,
        /// Why.
        justification: String,
    },
    /// Complete a pending recovery with the recovery authority's approval (`F-012`).
    CompleteRecovery {
        /// The pending recovery.
        pending: String,
        /// The approval, hex.
        approval: String,
        /// Why.
        justification: String,
    },
    /// The writer's code becomes a code version from an input (`REQ-RT-16`, `T-037-04`).
    Upgrade {
        /// The code version, hex.
        code: String,
        /// The input it applies from.
        from: u64,
        /// Why.
        justification: String,
    },
    /// Move the scheduled upgrade to a later input (`REQ-RT-20`, `T-037-15`).
    RescheduleUpgrade {
        /// The input it now applies from.
        from: u64,
        /// Why.
        justification: String,
    },
    /// Cancel the scheduled upgrade (`REQ-RT-20`, `T-037-15`).
    CancelUpgrade {
        /// Why.
        justification: String,
    },
}

// ------------------------------------------------------------------------------- pass-through

/// The most pass-through data one command may carry: bytes of its JSON text, as sent
/// (`REQ-PO-5`).
pub const MAX_PASS_THROUGH_BYTES: usize = 1024;

/// Pass-through data (`REQ-PO-5`, amendment 0020 R1, `prq-08`): one JSON value, UTF-8, of at most
/// [`MAX_PASS_THROUGH_BYTES`] bytes, carried on the wire as `reference`.
///
/// aledgent keeps the caller's text exactly as it was sent (whitespace and key order included,
/// numbers never re-read) and never parses it beyond checking that it is one JSON value: nothing
/// branches on it (`REQ-PO-4`). Ingress takes it off the command before the command is
/// canonicalised for the core, keeps it in the command's received-log row, and echoes it from
/// there; the core never receives it, so it is in no decision, no state commitment, no disclosure
/// and no anchor (`INV-17`).
///
/// Over the bound, or not one JSON value, the body does not parse: `ERR-MalformedCommand`
/// (`REQ-ER-3`). Its `Debug` shows only its length, so it never reaches a log.
#[derive(Clone)]
pub struct PassThrough(Box<RawValue>);

impl PassThrough {
    /// Pass-through data from its JSON text: `None` if the text is not one JSON value, or is
    /// over the bound.
    pub fn parse(text: &str) -> Option<PassThrough> {
        let raw: Box<RawValue> = serde_json::from_str(text).ok()?;
        (raw.get().len() <= MAX_PASS_THROUGH_BYTES).then_some(PassThrough(raw))
    }

    /// Its JSON text, exactly as the caller sent it.
    pub fn get(&self) -> &str {
        self.0.get()
    }
}

impl PartialEq for PassThrough {
    fn eq(&self, other: &PassThrough) -> bool {
        self.get() == other.get()
    }
}

impl Eq for PassThrough {}

impl std::fmt::Debug for PassThrough {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The caller's own data, possibly anything: never formatted.
        write!(f, "PassThrough({} bytes)", self.get().len())
    }
}

impl Serialize for PassThrough {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for PassThrough {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<PassThrough, D::Error> {
        // Only from a JSON text read directly (a request body, a stored column): a raw value does
        // not survive serde's buffering, so a body that carries one is never inside a tagged enum
        // when it is parsed (`CommandBody` never carries it: ingress takes it off first).
        let raw = Box::<RawValue>::deserialize(d)?;
        if raw.get().len() > MAX_PASS_THROUGH_BYTES {
            // Names nothing the caller sent (`REQ-ER-3`, `REQ-AU-8`).
            return Err(serde::de::Error::custom("pass-through data over its bound"));
        }
        Ok(PassThrough(raw))
    }
}

impl JsonSchema for PassThrough {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PassThrough".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "Pass-through data (REQ-PO-5): any one JSON value, at most 1024 bytes of UTF-8 as sent. Kept and echoed exactly as sent; never read by aledgent. Over the bound, or not one JSON value, the command is refused with ERR-MalformedCommand."
        })
    }
}

// ------------------------------------------------------------------------------------ replies

/// What a decided command produced. Only the fields of the command's result are present.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    /// The posting sequence numbers the command appended, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postings: Vec<u64>,
    /// A new identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Its first credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The credentials an import added, in order (`F-028`, `REQ-MG-7`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<String>,
    /// A new ledger account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// A new grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    /// An integrating party registered (`REQ-ID-7`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
    /// A pending recovery or succession opened or completed (`F-012`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    /// When that pending change may complete (`F-012`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at: Option<u64>,
    /// What a settlement command produced (`F-016`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement: Option<Box<crate::settlement::SettlementResultBody>>,
    /// What an anchoring command produced (`F-017`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchoring: Option<Box<crate::anchoring::AnchoringResultBody>>,
}

/// The class of a state (`REQ-OP-7`): whether it is final, and whether anything has returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum StateClassBody {
    /// Terminal: the outcome is final.
    Terminal,
    /// Not terminal and not refunding: nothing has returned yet.
    OpenNonRefunding,
    /// `Needs-Review`: waiting for a human. Not terminal and not refunding.
    AwaitingHuman,
}

impl From<StateClass> for StateClassBody {
    fn from(c: StateClass) -> Self {
        match c {
            StateClass::Terminal => StateClassBody::Terminal,
            StateClass::OpenNonRefunding => StateClassBody::OpenNonRefunding,
            StateClass::AwaitingHuman => StateClassBody::AwaitingHuman,
        }
    }
}

/// A lifecycle state as a reply shows it: its SPEC §5.6 name, its class, and whether it is
/// terminal. A caller can never read a non-terminal state as success (`REQ-OP-7`, `REQ-CS-6`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShownBody {
    /// The state, spelt as SPEC §5.6 spells it.
    pub state: String,
    /// Its class.
    pub class: StateClassBody,
    /// Whether it is terminal. `false` for every state that is not.
    pub terminal: bool,
}

impl From<Shown> for ShownBody {
    fn from(s: Shown) -> Self {
        ShownBody {
            state: s.state.name().to_owned(),
            class: s.class.into(),
            terminal: s.state.is_terminal(),
        }
    }
}

impl ShownBody {
    /// `Received`: durably taken in, not yet decided (`REQ-CS-2`).
    pub fn received() -> ShownBody {
        Shown::of(State::Received).into()
    }
}

/// The reply to a command (`REQ-CS-2`): an acknowledgement, not an outcome. Its state may be
/// `Received`; a caller learns completion by notification or by `operation(reference)`
/// (`REQ-CS-6`). There is no blocking "wait until terminal" reply.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandReply {
    /// The operation reference: service-derived, opaque, never supplied by a caller
    /// (`REQ-OP-10`).
    pub operation_reference: String,
    /// The operation's state at the moment of the reply, where the command has a lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// That state's class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<StateClassBody>,
    /// Whether this reply is terminal. `false` whenever the state is not terminal (`REQ-OP-7`).
    pub terminal: bool,
    /// The client key the command was taken under: the caller's, or the one derived from the
    /// request when the caller gave none (`REQ-OP-11`). A retry with it gets this reply again.
    pub client_key: String,
    /// Whether ingress derived the client key.
    pub client_key_derived: bool,
    /// What the command produced, once decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<CommandResult>,
    /// The API key a `register_credential` with scheme `api-key` issued (`F-028`): on the one
    /// reply that decided it, and never again. aledgent keeps only its hash (`REQ-AU-8`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The pass-through data its operation was submitted with (`REQ-PO-5`), exactly as sent.
    /// Shown only to principals of the submitting principal's integrating party; absent otherwise,
    /// and when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// `operation(reference)` (SPEC §8.2, `F-011`): the state the core holds, with its class
/// (`REQ-OP-7`), its deadline, evidence, outcome, refusal and postings. A command decided and
/// complete at once has no lifecycle state and is terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationReply {
    /// The operation reference.
    pub operation_reference: String,
    /// The command that opened it, where recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Its lifecycle kind, where it has a lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Its state, where it has a lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The state's class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<StateClassBody>,
    /// Whether it is terminal. `false` for every state that is not.
    pub terminal: bool,
    /// Its deadline, where not terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
    /// When it reached its terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<u64>,
    /// The terminal state, where terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The named refusal, where refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<RefusalSchema>")]
    pub error: Option<Refusal>,
    /// The digests of the evidence its transitions rest on, hex.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// The posting sequence numbers it appended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postings: Vec<u64>,
    /// The pass-through data its operation was submitted with (`REQ-PO-5`), exactly as sent.
    /// Shown only to principals of the submitting principal's integrating party; absent otherwise,
    /// and when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

impl OperationReply {
    /// A command ingress holds and the core has not decided: `Received`, not terminal.
    pub fn received(reference: String) -> OperationReply {
        let shown = ShownBody::received();
        OperationReply {
            operation_reference: reference,
            state: Some(shown.state),
            class: Some(shown.class),
            terminal: false,
            ..OperationReply::default()
        }
    }

    /// A command acknowledged at `Received` that the core refused before it opened an operation.
    pub fn rejected(reference: String, error: Refusal) -> OperationReply {
        let shown = ShownBody::from(Shown::of(State::Rejected));
        OperationReply {
            operation_reference: reference,
            state: Some(shown.state.clone()),
            class: Some(shown.class),
            terminal: true,
            outcome: Some(shown.state),
            error: Some(error),
            ..OperationReply::default()
        }
    }
}

/// The body of a refusal, `{"error": "ERR-…", "description": "…"}`. Its full description in the
/// OpenAPI document is generated from SPEC §10's closed list ([`crate::openapi`]).
#[derive(JsonSchema)]
#[schemars(rename = "Refusal")]
pub struct RefusalSchema {
    /// The named condition, `ERR-…`.
    pub error: String,
    /// The condition's published description (SPEC §10).
    pub description: String,
}

// ------------------------------------------------------------------------------------ notifications

/// A notification (`CTR-EVT`, `REQ-EV-1`–`REQ-EV-3`). It stands on its own: it never needs an
/// earlier notification to be understood, and its `id` is the same on every attempt, so a
/// redelivery is recognisable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotificationBody {
    /// `H(operation, transition)`, hex: stable across attempts (`REQ-EV-3`).
    pub id: String,
    /// The operation.
    pub operation_reference: String,
    /// The state it entered.
    pub state: String,
    /// That state's class.
    pub class: StateClassBody,
    /// Whether it is terminal.
    pub terminal: bool,
    /// The terminal state, where terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The named refusal, where refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<RefusalSchema>")]
    pub error: Option<Refusal>,
    /// The latest anchor whose publication had reached finality when the transition was
    /// recorded, or `null` when none had (`REQ-EV-1`, `AC-M2.4`). Always present. It is fixed
    /// when the transition is recorded: every delivery of this notification names the same one
    /// (`REQ-EV-3`).
    pub anchor: Option<AnchorRefBody>,
    /// The pass-through data the operation was submitted with (`REQ-PO-5`), exactly as sent, when
    /// the recipient is of the submitting principal's integrating party. Inside the signed body,
    /// and the same on every delivery: it is fixed when the operation is taken in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PassThrough>,
}

/// An anchor a notification names (`REQ-EV-1`, `AC-M2.4`): a final checkpoint of the book.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchorRefBody {
    /// Its number: the settlement layer's anchor number.
    pub number: u64,
    /// The sequence of the last entry it commits to (`REQ-PR-7`).
    pub entry_sequence: u64,
    /// Its `Anchored` record on the settlement layer.
    pub coordinate: CoordinateBody,
    /// The height of the block that wrote that record.
    pub height: u64,
}

/// An undelivered notification, with its age (`REQ-EV-6`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UndeliveredBody {
    /// The notification.
    pub notification: NotificationBody,
    /// The principal it is owed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    /// How long it has been owed, in milliseconds.
    pub age: u64,
}

/// The name SPEC §5.6 gives a state, for parsing a stored reply back.
pub fn state_named(name: &str) -> Option<State> {
    State::ALL.into_iter().find(|s| s.name() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `REQ-OP-7`, `REQ-CS-6`: every state that is not terminal is labelled `terminal: false`,
    /// and every state reads back by its name.
    #[test]
    fn req_op_7_every_non_terminal_state_is_labelled_non_terminal() {
        for name in [
            "Received",
            "Accepted",
            "Reserved",
            "Submitted",
            "Observed",
            "Final",
            "Settled",
            "Released",
            "Rejected",
            "Expired",
            "Orphaned",
            "Needs-Review",
        ] {
            let s = state_named(name).expect("every §5.6 state has its name");
            let body = ShownBody::from(Shown::of(s));
            assert_eq!(body.state, name);
            assert_eq!(body.terminal, s.is_terminal(), "{name}");
            assert_eq!(
                body.class == StateClassBody::Terminal,
                s.is_terminal(),
                "{name}"
            );
        }
        // Withdrawn by amendment 0006 item 1: a reversal is an operation of its own.
        assert_eq!(state_named("Reversed"), None);
    }

    /// `REQ-PO-5`, `AC-M8.3`: pass-through data is one JSON value of at most 1 KiB, kept exactly
    /// as sent; over the bound, or not a value, the body does not parse; `null` is none.
    #[test]
    fn req_po_5_pass_through_is_one_value_of_at_most_1_kib_kept_as_sent() {
        let body = |reference: &str| {
            format!(
                r#"{{"legs":[{{"from":"a","to":"b","asset":"USD","amount":"1"}}],"reference":{reference}}}"#
            )
        };
        // Kept exactly: key order, spacing inside the value, and a number no float holds.
        let sent = r#"{"z": 1, "a":[true,null], "n": 123456789012345678901234567890.5}"#;
        let t: TransferBody = serde_json::from_str(&body(sent)).unwrap();
        let p = t.reference.clone().unwrap();
        assert_eq!(p.get(), sent);
        let echoed = serde_json::to_string(&CommandReply {
            operation_reference: "did:aledgent:op:x".into(),
            state: None,
            class: None,
            terminal: true,
            client_key: "k".into(),
            client_key_derived: false,
            result: None,
            api_key: None,
            reference: Some(p.clone()),
        })
        .unwrap();
        assert!(
            echoed.ends_with(&format!(r#""reference":{sent}}}"#)),
            "{echoed}"
        );
        // Its `Debug` shows its length, never its content.
        assert_eq!(
            format!("{p:?}"),
            format!("PassThrough({} bytes)", sent.len())
        );
        assert!(!format!("{t:?}").contains("123456789"));
        // The bound is on the bytes as sent: 1 024 is taken, 1 025 is not.
        let at = format!(r#""{}""#, "x".repeat(MAX_PASS_THROUGH_BYTES - 2));
        let over = format!(r#""{}""#, "x".repeat(MAX_PASS_THROUGH_BYTES - 1));
        assert_eq!(at.len(), 1024);
        assert!(serde_json::from_str::<TransferBody>(&body(&at)).is_ok());
        assert!(serde_json::from_str::<TransferBody>(&body(&over)).is_err());
        assert!(PassThrough::parse(&at).is_some());
        assert!(PassThrough::parse(&over).is_none());
        // Not one JSON value.
        assert!(serde_json::from_str::<TransferBody>(&body("{")).is_err());
        assert!(serde_json::from_str::<TransferBody>(&body("1 2")).is_err());
        assert!(PassThrough::parse("1 2").is_none());
        // `null` is none, and a body without it has none.
        let t: TransferBody = serde_json::from_str(&body("null")).unwrap();
        assert_eq!(t.reference, None);
        // Any one value: a string, a number, an array.
        for v in [r#""o-1""#, "42", "[1,2]"] {
            let t: TransferBody = serde_json::from_str(&body(v)).unwrap();
            assert_eq!(t.reference.map(|p| p.get().to_owned()).as_deref(), Some(v));
        }
        // Only the commands that submit an operation take it.
        let close = r#"{"command":"release","reservation":"r","reference":1}"#;
        assert!(serde_json::from_str::<CommandBody>(close).is_err());
        assert!(
            serde_json::from_str::<CaptureBody>(
                r#"{"destination":"d","cleared":"1","reference":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_command_body_round_trips_and_refuses_unknown_fields() {
        let c = CommandBody::Capture(CaptureCommand {
            reservation: "did:aledgent:op:x".into(),
            destination: "did:aledgent:account:y".into(),
            cleared: "10".into(),
        });
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["command"], "capture");
        assert_eq!(serde_json::from_value::<CommandBody>(json).unwrap(), c);
        let extra = serde_json::json!({"command": "release", "reservation": "r", "x": 1});
        assert!(serde_json::from_value::<CommandBody>(extra).is_err());
        let bad = serde_json::json!({"legs": [], "surprise": 1});
        assert!(serde_json::from_value::<TransferBody>(bad).is_err());
    }
}

impl std::fmt::Debug for CommandReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // An issued API key is a secret: never formatted (`REQ-AU-8`).
        f.debug_struct("CommandReply")
            .field("operation_reference", &self.operation_reference)
            .field("state", &self.state)
            .field("class", &self.class)
            .field("terminal", &self.terminal)
            .field("client_key", &self.client_key)
            .field("client_key_derived", &self.client_key_derived)
            .field("result", &self.result)
            .field("api_key", &self.api_key.as_ref().map(|_| "<issued>"))
            .field("reference", &self.reference)
            .finish()
    }
}

// ------------------------------------------------------------------------------ answering

/// How a principal's transfers and reservations are answered (`REQ-CS-9`, amendment 0013).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnswerBody {
    /// Acknowledged at `Received`; the decision follows by notification and query. The default.
    Acknowledged,
    /// Answered with the decision, or `ERR-DecisionUnavailable` past the wait.
    Decided,
}

/// `POST /v1/principals/self/answering` (`REQ-CS-9`): set the calling principal's answering
/// setting. It applies to transfers and reservations taken in from now on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnsweringBody {
    /// How its transfers and reservations are answered.
    pub answer: AnswerBody,
    /// How long a decided reply may wait for the decision, in milliseconds. At most the
    /// deployment's decision wait; without it, `NFR-16`'s 1 000 ms, or the decision wait if
    /// shorter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
}

/// A principal's answering setting, as it stands (`REQ-CS-9`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnsweringReply {
    /// The principal it applies to.
    pub principal: String,
    /// How its transfers and reservations are answered.
    pub answer: AnswerBody,
    /// How long a decided reply waits for the decision, in milliseconds.
    pub wait_ms: u64,
    /// Who set it. Absent for the default, which nobody set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by: Option<String>,
    /// When it was set. Absent for the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_at: Option<u64>,
}

// ------------------------------------------------------------------------- authentication

/// What a challenge is for (`F-028`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChallengePurposeBody {
    /// A sign-in: `navigator.credentials.get`.
    Authenticate,
    /// A registration: `navigator.credentials.create`.
    Register,
}

/// `POST /v1/auth/challenge` (`F-028`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChallengeRequest {
    /// The ceremony it is for.
    pub purpose: ChallengePurposeBody,
    /// For `register` only, optional: the identity whose set the credential will join,
    /// `did:aledgent:id:…`. A registration over the challenge is then refused with
    /// `ERR-CredentialInvalid` for any other identity's set (amendment 0027). Absent, the challenge
    /// is bound to no identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// A challenge (`F-028`): pass `challenge`, decoded, as the ceremony's challenge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChallengeReply {
    /// The challenge, base64url.
    pub challenge: String,
    /// When it expires.
    pub expires_at: u64,
}

/// `POST /v1/auth/passkey` (`F-028`): sign in with a passkey.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PasskeySignInBody {
    /// The credential, `did:aledgent:cred:…`.
    pub credential: String,
    /// The assertion over a challenge with purpose `authenticate`.
    pub assertion: AssertionBody,
}

/// A session (`F-028`): present `session` as a bearer token until `expires_at`. It is a secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionReply {
    /// The session token.
    pub session: String,
    /// When it expires.
    pub expires_at: u64,
    /// The identity it authenticates.
    pub principal: String,
    /// The passkey it signed in with.
    pub credential: String,
}

impl std::fmt::Debug for SessionReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The session is a secret (`REQ-AU-8`).
        f.debug_struct("SessionReply")
            .field("expires_at", &self.expires_at)
            .field("principal", &self.principal)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}
