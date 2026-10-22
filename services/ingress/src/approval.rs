// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Per-command approval at the front door (amendment 0037 `[PENDING AMENDMENT 0037]`, `F-040`).
//!
//! A command that moves or reserves value, changes who may act or changes a credential, submitted
//! under a **session**, must carry an approval: an assertion by one of the initiating identity's
//! device-bound credentials over a challenge (F2) that carries this command's digest (F1). The
//! formats are frozen in `authn::approval`.
//!
//! - **The switch** ([`ApprovalMode`], `T-040-07`): off (the default: nothing changes), report
//!   only (log what would be refused, refuse nothing) or enforce.
//! - **The door** ([`crate::auth::Proof`], `T-040-11`): only a session needs an approval. An
//!   issued secret and a configured token, presented as they are, are exempt (ruling 4).
//! - **The policy** ([`Kind`], `T-040-04`): every command kind, classified by an exhaustive match.
//! - **The two steps** (`T-040-05`): step (a) is the command's own route under
//!   [`APPROVAL_PREFIX`], which takes nothing in and returns the challenge; step (b) is the command
//!   route with the approval in the [`APPROVAL_HEADER`] header. Ingress recomputes the digest from
//!   the command in that request; it never trusts one the client sends.
//!
//! Every failure of a presented approval is the same `ERR-CredentialInvalid`, with no detail, so
//! the route is no oracle. No assertion, challenge or credential reaches a log (`REQ-AU-8`).

use authn::approval::{self as fmt, CommandContext};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ledger_core::Refusal;
use ledger_core::account::Time;
use ledger_core::id::IdentityId;
use ledger_core::principal::Scheme;
use ledger_core::scheme::PasskeyKey;
use serde::{Deserialize, Serialize};

use crate::auth::{Authenticated, Passkeys, Proof};
use crate::boundary::Core;
use crate::command::Command;
use crate::credentials::{self, Pending};
use crate::operator::OperatorOp;
use crate::service::Shared;
use crate::settlement::SettlementCommand;
use crate::store::Store;
use crate::wire::AssertionBody;

/// The header that carries an approval on a command route: base64url of the JSON
/// [`ApprovalBody`].
pub const APPROVAL_HEADER: &str = "aledgent-approval";

/// Step (a)'s routes: each command route's path with `/v1` replaced by this prefix, such as
/// `POST /v1/approvals/transfers` for `POST /v1/transfers`. Same authentication, same body, same
/// headers; it takes nothing in.
pub const APPROVAL_PREFIX: &str = "/v1/approvals";

/// The refusal of a command that needs an approval and carries none (amendment 0037, R2).
/// Held in ingress until the next core carries it in its own list (`T-040-10` reconciles this
/// with the published refusal table).
pub const COMMAND_APPROVAL_REQUIRED: &str = "ERR-CommandApprovalRequired";

/// Its description, verbatim from amendment 0037, R2.
pub const COMMAND_APPROVAL_REQUIRED_DESCRIPTION: &str = "A command that needs a credential's \
    approval of that one command (`REQ-AU-23`) was submitted without one. A presented approval \
    that does not verify is `ERR-CredentialInvalid`";

/// The refusal [`COMMAND_APPROVAL_REQUIRED`], as the surface sends it: the status of
/// `ERR-NotAuthorised` (the amendment's proposal), and a body of the shape every refusal has.
pub fn approval_required() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": COMMAND_APPROVAL_REQUIRED,
            "description": COMMAND_APPROVAL_REQUIRED_DESCRIPTION,
        })),
    )
        .into_response()
}

// ----------------------------------------------------------------------------- the switch

/// The rollout switch (`T-040-07`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Nothing changes: no approval is asked for, and one presented is ignored. The default.
    #[default]
    Off,
    /// Log what would be refused, and refuse nothing.
    ReportOnly,
    /// Refuse.
    Enforce,
}

impl ApprovalMode {
    /// Its name, as configured.
    pub fn name(self) -> &'static str {
        match self {
            ApprovalMode::Off => "off",
            ApprovalMode::ReportOnly => "report_only",
            ApprovalMode::Enforce => "enforce",
        }
    }
}

/// A deployment's approval setting: the switch, and what separates its approvals from another
/// deployment's or environment's (F1 fields 3 and 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approvals {
    mode: ApprovalMode,
    environment: String,
    deployment: [u8; 16],
}

impl Approvals {
    /// Off.
    pub fn off() -> Approvals {
        Approvals {
            mode: ApprovalMode::Off,
            environment: String::new(),
            deployment: [0; 16],
        }
    }

    /// The setting for `mode`, in `environment`, for the book `deployment` (16 bytes of hex, the
    /// genesis configuration's). Ingress refuses to start with approvals on and no environment.
    /// Off reads neither, so it can never stop a deployment from starting.
    pub fn new(
        mode: ApprovalMode,
        environment: Option<&str>,
        deployment: &str,
    ) -> Result<Approvals, String> {
        if mode == ApprovalMode::Off {
            return Ok(Approvals::off());
        }
        let environment = environment.unwrap_or_default().to_owned();
        if mode != ApprovalMode::Off && environment.trim().is_empty() {
            return Err(
                "passkeys: approvals are on and no environment is named (amendment 0037, F1)"
                    .into(),
            );
        }
        let deployment = hex::decode(deployment.trim())
            .ok()
            .and_then(|v| <[u8; 16]>::try_from(v).ok())
            .ok_or("genesis.deployment must be 16 bytes of hex")?;
        Ok(Approvals {
            mode,
            environment,
            deployment,
        })
    }

    /// The switch's position.
    pub fn mode(&self) -> ApprovalMode {
        self.mode
    }
}

// ----------------------------------------------------------------------------- the policy

/// Every command kind a route can submit (`T-040-04`). [`kind_of`] maps every [`Command`] to one
/// by an exhaustive match, and [`Kind::needs_approval`] classifies every kind by another: a new
/// command does not compile until it is classified, on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Kind {
    Transfer,
    Reserve,
    Withdraw,
    Capture,
    Release,
    Issue,
    Redeem,
    SubmitOutbound,
    ProposeReversal,
    ApproveReversal,
    ResolveOperation,
    Resume,
    GrantAuthority,
    RevokeAuthority,
    GrantIssuance,
    RevokeIssuance,
    GrantOperator,
    RevokeOperator,
    DelegateCustody,
    NameSuccessor,
    ClaimSuccession,
    CancelRecovery,
    InitiateRecovery,
    CompleteRecovery,
    RegisterCredential,
    RemoveCredential,
    ImportCredentials,
    RegisterAlias,
    BindAsset,
    ConfigureSettlement,
    RegisterIntegratingParty,
    TiePrincipal,
    Upgrade,
    RescheduleUpgrade,
    CancelUpgrade,
    OpenIdentity,
    OpenAccount,
    CloseAccount,
    ReclaimCustody,
    FreezeAccount,
    UnfreezeAccount,
    RegisterAsset,
    DeprecateAsset,
    DeclareAdapterSupport,
    SubmitEvidence,
    IssueDisclosure,
    AnchorCheckpoint,
    CompareSettlement,
    AdapterInput,
}

impl Kind {
    /// Every kind.
    pub const ALL: &[Kind] = &[
        Kind::Transfer,
        Kind::Reserve,
        Kind::Withdraw,
        Kind::Capture,
        Kind::Release,
        Kind::Issue,
        Kind::Redeem,
        Kind::SubmitOutbound,
        Kind::ProposeReversal,
        Kind::ApproveReversal,
        Kind::ResolveOperation,
        Kind::Resume,
        Kind::GrantAuthority,
        Kind::RevokeAuthority,
        Kind::GrantIssuance,
        Kind::RevokeIssuance,
        Kind::GrantOperator,
        Kind::RevokeOperator,
        Kind::DelegateCustody,
        Kind::NameSuccessor,
        Kind::ClaimSuccession,
        Kind::CancelRecovery,
        Kind::InitiateRecovery,
        Kind::CompleteRecovery,
        Kind::RegisterCredential,
        Kind::RemoveCredential,
        Kind::ImportCredentials,
        Kind::RegisterAlias,
        Kind::BindAsset,
        Kind::ConfigureSettlement,
        Kind::RegisterIntegratingParty,
        Kind::TiePrincipal,
        Kind::Upgrade,
        Kind::RescheduleUpgrade,
        Kind::CancelUpgrade,
        Kind::OpenIdentity,
        Kind::OpenAccount,
        Kind::CloseAccount,
        Kind::ReclaimCustody,
        Kind::FreezeAccount,
        Kind::UnfreezeAccount,
        Kind::RegisterAsset,
        Kind::DeprecateAsset,
        Kind::DeclareAdapterSupport,
        Kind::SubmitEvidence,
        Kind::IssueDisclosure,
        Kind::AnchorCheckpoint,
        Kind::CompareSettlement,
        Kind::AdapterInput,
    ];

    /// Whether a command of this kind needs an approval when submitted under a session: the
    /// default list of amendment 0037, "Which commands", in full.
    pub fn needs_approval(self) -> bool {
        match self {
            // Move or reserve value, or name where it goes.
            Kind::Transfer
            | Kind::Reserve
            | Kind::Withdraw
            | Kind::Capture
            | Kind::Release
            | Kind::Issue
            | Kind::Redeem
            | Kind::SubmitOutbound
            // Each can move value: a reversal posts, a resolution settles or releases, a resume
            // restarts an operation.
            | Kind::ProposeReversal
            | Kind::ApproveReversal
            | Kind::ResolveOperation
            | Kind::Resume
            // Change who may act.
            | Kind::GrantAuthority
            | Kind::RevokeAuthority
            | Kind::GrantIssuance
            | Kind::RevokeIssuance
            | Kind::GrantOperator
            | Kind::RevokeOperator
            // Change who may act on an account.
            | Kind::DelegateCustody
            | Kind::NameSuccessor
            | Kind::ClaimSuccession
            | Kind::CancelRecovery
            | Kind::InitiateRecovery
            | Kind::CompleteRecovery
            // Change a credential (the digest covers the verifier being added).
            | Kind::RegisterCredential
            | Kind::RemoveCredential
            | Kind::ImportCredentials
            // Decides where value sent to an alias goes.
            | Kind::RegisterAlias
            // Fixes where an asset's value leaves and enters.
            | Kind::BindAsset
            // Names the adapter whose facts the book accepts.
            | Kind::ConfigureSettlement
            // Decide whose client keys a principal shares and who sees its pass-through data.
            | Kind::RegisterIntegratingParty
            | Kind::TiePrincipal
            // Replaces the code the book runs; approved by the credential the mode names.
            | Kind::Upgrade => true,
            // A way out: any one member may take it alone, with no second approval (amendment
            // 0034, ruling `w6` (B)). It can stop a change of code, never make one.
            Kind::RescheduleUpgrade | Kind::CancelUpgrade => false,
            // Ruled 2026-10-21.
            Kind::OpenIdentity | Kind::OpenAccount | Kind::CloseAccount => false,
            // Ruled 2026-10-21: it carries its own proof of possession over the account and epoch.
            Kind::ReclaimCustody => false,
            // None of the three tests: no value moves, no authority or credential changes.
            Kind::FreezeAccount
            | Kind::UnfreezeAccount
            | Kind::RegisterAsset
            | Kind::DeprecateAsset
            | Kind::DeclareAdapterSupport
            | Kind::SubmitEvidence
            | Kind::IssueDisclosure
            | Kind::AnchorCheckpoint
            | Kind::CompareSettlement => false,
            // Facts the settlement adapter reports; only the principal `configure_settlement`
            // names may submit them (`INV-11`).
            Kind::AdapterInput => false,
        }
    }

    /// The command's name, as the amendment's table writes it. For logs and counts.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Transfer => "transfer",
            Kind::Reserve => "reserve",
            Kind::Withdraw => "withdraw",
            Kind::Capture => "capture",
            Kind::Release => "release",
            Kind::Issue => "issue",
            Kind::Redeem => "redeem",
            Kind::SubmitOutbound => "submit_outbound",
            Kind::ProposeReversal => "propose_reversal",
            Kind::ApproveReversal => "approve_reversal",
            Kind::ResolveOperation => "resolve_operation",
            Kind::Resume => "resume",
            Kind::GrantAuthority => "grant_authority",
            Kind::RevokeAuthority => "revoke_authority",
            Kind::GrantIssuance => "grant_issuance",
            Kind::RevokeIssuance => "revoke_issuance",
            Kind::GrantOperator => "grant_operator",
            Kind::RevokeOperator => "revoke_operator",
            Kind::DelegateCustody => "delegate_custody",
            Kind::NameSuccessor => "name_successor",
            Kind::ClaimSuccession => "claim_succession",
            Kind::CancelRecovery => "cancel_recovery",
            Kind::InitiateRecovery => "initiate_recovery",
            Kind::CompleteRecovery => "complete_recovery",
            Kind::RegisterCredential => "register_credential",
            Kind::RemoveCredential => "remove_credential",
            Kind::ImportCredentials => "import_credentials",
            Kind::RegisterAlias => "register_alias",
            Kind::BindAsset => "bind_asset",
            Kind::ConfigureSettlement => "configure_settlement",
            Kind::RegisterIntegratingParty => "register_integrating_party",
            Kind::TiePrincipal => "tie_principal",
            Kind::Upgrade => "upgrade",
            Kind::RescheduleUpgrade => "reschedule_upgrade",
            Kind::CancelUpgrade => "cancel_upgrade",
            Kind::OpenIdentity => "open_identity",
            Kind::OpenAccount => "open_account",
            Kind::CloseAccount => "close_account",
            Kind::ReclaimCustody => "reclaim_custody",
            Kind::FreezeAccount => "freeze_account",
            Kind::UnfreezeAccount => "unfreeze_account",
            Kind::RegisterAsset => "register_asset",
            Kind::DeprecateAsset => "deprecate_asset",
            Kind::DeclareAdapterSupport => "declare_adapter_support",
            Kind::SubmitEvidence => "submit_evidence",
            Kind::IssueDisclosure => "issue_disclosure",
            Kind::AnchorCheckpoint => "anchor_checkpoint",
            Kind::CompareSettlement => "compare_settlement",
            Kind::AdapterInput => "adapter_input",
        }
    }
}

/// The kind of `c`, by an exhaustive match.
pub fn kind_of(c: &Command) -> Kind {
    use crate::anchoring::AnchoringCommand as A;
    use crate::custody::CustodyCommand as C;
    match c {
        Command::Transfer(_) => Kind::Transfer,
        Command::Reserve { .. } => Kind::Reserve,
        Command::Capture { .. } => Kind::Capture,
        Command::Release { .. } => Kind::Release,
        Command::Issue { .. } => Kind::Issue,
        Command::Redeem { .. } => Kind::Redeem,
        Command::Withdraw(_) => Kind::Withdraw,
        Command::OpenIdentity { .. } => Kind::OpenIdentity,
        Command::OpenAccount(_) => Kind::OpenAccount,
        Command::CloseAccount { .. } => Kind::CloseAccount,
        Command::GrantAuthority(_) => Kind::GrantAuthority,
        Command::RevokeAuthority { .. } => Kind::RevokeAuthority,
        Command::Operator(j) => match &j.op {
            OperatorOp::Freeze { frozen: true, .. } => Kind::FreezeAccount,
            OperatorOp::Freeze { frozen: false, .. } => Kind::UnfreezeAccount,
            OperatorOp::Resume(_) => Kind::Resume,
            OperatorOp::Resolve(_) => Kind::ResolveOperation,
            OperatorOp::SubmitEvidence(_) => Kind::SubmitEvidence,
            OperatorOp::ProposeReversal(_) => Kind::ProposeReversal,
            OperatorOp::ApproveReversal(_) => Kind::ApproveReversal,
            OperatorOp::RegisterAsset(_) => Kind::RegisterAsset,
            OperatorOp::DeprecateAsset(..) => Kind::DeprecateAsset,
            OperatorOp::BindAsset(..) => Kind::BindAsset,
            OperatorOp::GrantIssuance(_) => Kind::GrantIssuance,
            OperatorOp::RevokeIssuance(_) => Kind::RevokeIssuance,
            OperatorOp::GrantOperator { .. } => Kind::GrantOperator,
            OperatorOp::RevokeOperator(_) => Kind::RevokeOperator,
            OperatorOp::DeclareAdapterSupport(_) => Kind::DeclareAdapterSupport,
            OperatorOp::InitiateRecovery(_) => Kind::InitiateRecovery,
            OperatorOp::CompleteRecovery(..) => Kind::CompleteRecovery,
            OperatorOp::ImportCredentials(_) => Kind::ImportCredentials,
            OperatorOp::RegisterIntegratingParty(_) => Kind::RegisterIntegratingParty,
            OperatorOp::TiePrincipal { .. } => Kind::TiePrincipal,
            OperatorOp::RegisterAlias { .. } => Kind::RegisterAlias,
            OperatorOp::Upgrade { .. } => Kind::Upgrade,
            OperatorOp::RescheduleUpgrade { .. } => Kind::RescheduleUpgrade,
            OperatorOp::CancelUpgrade => Kind::CancelUpgrade,
        },
        Command::Custody(c) => match c {
            C::RegisterCredential(_) => Kind::RegisterCredential,
            C::RemoveCredential(_) => Kind::RemoveCredential,
            C::DelegateCustody(_) => Kind::DelegateCustody,
            C::ReclaimCustody(_) => Kind::ReclaimCustody,
            C::CancelRecovery(_) => Kind::CancelRecovery,
            C::NameSuccessor(..) => Kind::NameSuccessor,
            C::ClaimSuccession(_) => Kind::ClaimSuccession,
        },
        Command::Settlement(s) => match s {
            SettlementCommand::SubmitOutbound { .. } => Kind::SubmitOutbound,
            SettlementCommand::Configure(_) => Kind::ConfigureSettlement,
            SettlementCommand::Adapter(_) => Kind::AdapterInput,
        },
        Command::Anchoring(a) => match a {
            A::IssueDisclosure(_) => Kind::IssueDisclosure,
            A::AnchorCheckpoint(_) => Kind::AnchorCheckpoint,
            A::CompareSettlement(_) => Kind::CompareSettlement,
        },
    }
}

/// Whether `command` from `who` needs an approval under `approvals`: never while the switch is
/// off, never for a bearer secret presented as it is, and otherwise as [`Kind::needs_approval`].
pub fn needed(approvals: &Approvals, who: &Authenticated, command: &Command) -> bool {
    approvals.mode() != ApprovalMode::Off
        && who.proof == Proof::Session
        && kind_of(command).needs_approval()
}

// ----------------------------------------------------------------------------- the digest

/// The command digest of F1 for `command` from `who`, recomputed by ingress from its own copy:
/// the text and the digest. `pending` is the registration a `register_credential` of a
/// device-bound credential brings (F1 field 9). A command whose text cannot be written is
/// `ERR-MalformedCommand`; a registration that does not decode is `ERR-CredentialInvalid`.
pub fn digest_of(
    approvals: &Approvals,
    who: &Authenticated,
    on_behalf_of: Option<&IdentityId>,
    client_key: &str,
    pending: Option<&Pending>,
    command: &Command,
) -> Result<(Vec<u8>, [u8; 32]), Refusal> {
    let value = serde_json::to_value(command.to_body()).map_err(|_| Refusal::MalformedCommand)?;
    let text = fmt::canonical_text(&value).ok_or(Refusal::MalformedCommand)?;
    let payload = match pending {
        Some(Pending::RegisterPasskey(p)) => Some(fmt::payload_digest(
            &credentials::b64(&p.client_data_json)?,
            &credentials::b64(&p.attestation_object)?,
        )),
        _ => None,
    };
    let obo = on_behalf_of.map(|o| o.to_did());
    let digest = fmt::command_digest(
        &CommandContext {
            deployment: &approvals.deployment,
            environment: &approvals.environment,
            client_key,
            initiator: &who.principal,
            on_behalf_of: obo.as_deref(),
            payload_digest: payload.as_ref(),
        },
        &text,
    );
    Ok((text, digest))
}

// ----------------------------------------------------------------------------- step (a)

/// Step (a)'s reply: whether the command needs an approval, and if it does, what to sign.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalChallengeReply {
    /// Whether the command needs an approval from this caller.
    pub required: bool,
    /// The switch's position: `off`, `report_only` or `enforce`.
    pub mode: String,
    /// The client key the command is approved under: send the same (or none, if derived).
    pub client_key: String,
    /// Whether ingress derived it (`REQ-OP-11`).
    pub client_key_derived: bool,
    /// The command's canonical text, for the client to show what is approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The command digest (F1), hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_digest: Option<String>,
    /// The challenge (F2), base64url: what the device signs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenge: Option<String>,
    /// When the challenge expires, milliseconds since the Unix epoch, ingress's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Time>,
}

/// Issue the challenge for `digest`, from `who`, valid for the deployment's challenge lifetime.
pub fn issue(
    pk: &Passkeys,
    who: &Authenticated,
    digest: &[u8; 32],
    now: Time,
) -> Result<(String, Time), Refusal> {
    let identity = IdentityId::parse(&who.principal).map_err(|_| Refusal::CredentialInvalid)?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| Refusal::Internal)?;
    let expires_at = now.saturating_add(pk.challenge_ttl());
    let a = pk.approvals();
    let c = fmt::issue_approval_challenge(
        pk.key(),
        expires_at,
        nonce,
        &identity,
        digest,
        &a.deployment,
        &a.environment,
    );
    Ok((URL_SAFE_NO_PAD.encode(c), expires_at))
}

// ----------------------------------------------------------------------------- step (b)

/// Why [`verify`] did not accept an approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Refused with this refusal.
    Refused(Refusal),
    /// Everything verified but the one-time number was already spent: by the first attempt of
    /// this same request, still in flight, or by another. The caller checks for the first before
    /// it refuses with `ERR-CredentialInvalid`.
    NumberSpent,
}

impl From<Refusal> for Rejected {
    fn from(r: Refusal) -> Rejected {
        Rejected::Refused(r)
    }
}

/// An approval as the [`APPROVAL_HEADER`] header carries it, base64url of this JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalBody {
    /// The approving credential, `did:`-shaped: any active device-bound credential of the
    /// initiating identity, not only the one the session signed in with.
    pub credential: String,
    /// Its assertion over the challenge of step (a).
    pub assertion: AssertionBody,
}

impl ApprovalBody {
    /// The header's value for this approval.
    pub fn header(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }
}

/// Verify the approval `header` carries for the command whose digest ingress recomputed,
/// `digest`, from `who`, in the order amendment 0037 gives: the challenge's length, purpose, seal
/// (deployment and environment) and expiry; its digest and identity (before its number is spent);
/// for `upgrade`, that the approver is the credential the caller presented; that the approver is
/// an active device-bound credential of the initiator's set, read live from the book (a book that
/// cannot be read is `ERR-DecisionUnavailable`, and no stored copy is used); the assertion, with
/// the device's own check required whatever sign-in asks; the number spent, once, on every
/// instance; the sign count raised. Returns the approving credential.
#[allow(clippy::too_many_arguments)]
pub async fn verify<S: Store, C: Core>(
    shared: &Shared<S, C>,
    pk: &Passkeys,
    who: &Authenticated,
    kind: Kind,
    digest: &[u8; 32],
    header: &str,
) -> Result<String, Rejected> {
    let now = shared.clock.now();
    let body: ApprovalBody = URL_SAFE_NO_PAD
        .decode(header.trim().trim_end_matches('='))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(Refusal::CredentialInvalid)?;
    let initiator = IdentityId::parse(&who.principal).map_err(|_| Refusal::CredentialInvalid)?;
    let cdj = credentials::b64(&body.assertion.client_data_json)?;
    let ch = authn::webauthn::presented_challenge(&cdj).ok_or(Refusal::CredentialInvalid)?;
    let a = pk.approvals();
    let issued = fmt::check_approval_challenge(pk.key(), &ch, now, &a.deployment, &a.environment)
        .ok_or(Refusal::CredentialInvalid)?;
    if !issued.is_for(&initiator, digest) {
        return Err(Refusal::CredentialInvalid.into());
    }
    if kind == Kind::Upgrade && body.credential != who.credential {
        return Err(Refusal::CredentialInvalid.into());
    }
    // The current credential list (R1): live, never the durable copy.
    let found = shared
        .core
        .credential(&body.credential)
        .await
        .map_err(|_| Refusal::DecisionUnavailable)?
        .ok_or(Refusal::CredentialInvalid)?;
    if found.scheme != Scheme::WebAuthnEs256.name() || found.identity != who.principal {
        return Err(Refusal::CredentialInvalid.into());
    }
    let key = hex::decode(&found.verifier)
        .ok()
        .and_then(|b| PasskeyKey::decode(&b))
        .ok_or(Refusal::CredentialInvalid)?;
    let count = credentials::verify_assertion_count(
        shared,
        pk,
        &body.credential,
        &key,
        &body.assertion,
        &ch,
        true,
    )
    .await?;
    let first = shared
        .store
        .use_challenge(issued.nonce, issued.expires_at, now)
        .await
        .map_err(|_| Refusal::DecisionUnavailable)?;
    if !first {
        return Err(Rejected::NumberSpent);
    }
    credentials::raise_count(shared, &body.credential, count).await?;
    Ok(body.credential)
}

/// What the report-only position logs for a command it would have refused: the command kind and
/// how the caller proved who it is, never a credential, an assertion or a digest.
/// `presented` says whether an approval came with the command: only then is a scheme named.
pub fn report(kind: Kind, proof: Proof, presented: bool) {
    if presented {
        tracing::info!(
            target: "aledgent_ingress::approval",
            command = kind.name(),
            proof = proof.name(),
            scheme = "webauthn-es256",
            would_refuse = "invalid",
            "approval report-only: this command would be refused under enforce"
        );
    } else {
        tracing::info!(
            target: "aledgent_ingress::approval",
            command = kind.name(),
            proof = proof.name(),
            would_refuse = "missing",
            "approval report-only: this command would be refused under enforce"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `T-040-04`, `AC-A10.10`: the classification equals amendment 0037's table, in full.
    #[test]
    fn t_040_04_the_policy_is_the_amendments_table() {
        let yes = [
            "transfer",
            "reserve",
            "withdraw",
            "capture",
            "release",
            "issue",
            "redeem",
            "submit_outbound",
            "propose_reversal",
            "approve_reversal",
            "resolve_operation",
            "resume",
            "grant_authority",
            "revoke_authority",
            "grant_issuance",
            "revoke_issuance",
            "grant_operator",
            "revoke_operator",
            "delegate_custody",
            "name_successor",
            "claim_succession",
            "cancel_recovery",
            "initiate_recovery",
            "complete_recovery",
            "register_credential",
            "remove_credential",
            "import_credentials",
            "register_alias",
            "bind_asset",
            "configure_settlement",
            "register_integrating_party",
            "tie_principal",
            "upgrade",
        ];
        let no = [
            "reschedule_upgrade",
            "cancel_upgrade",
            "open_identity",
            "open_account",
            "close_account",
            "set_answering",
            "reclaim_custody",
            "freeze_account",
            "unfreeze_account",
            "register_asset",
            "deprecate_asset",
            "declare_adapter_support",
            "submit_evidence",
            "issue_disclosure",
            "anchor_checkpoint",
            "compare_settlement",
            "adapter_input",
        ];
        for k in Kind::ALL {
            let n = k.name();
            if k.needs_approval() {
                assert!(yes.contains(&n), "{n} needs approval but the table says no");
            } else {
                assert!(no.contains(&n), "{n} is exempt but the table says yes");
            }
        }
        for n in yes {
            assert!(
                Kind::ALL
                    .iter()
                    .any(|k| k.name() == n && k.needs_approval()),
                "{n} is in the table and not classified as needing approval"
            );
        }
        // `set_answering` is not a command route (it has its own handler); the rest are kinds.
        for n in no.iter().filter(|n| **n != "set_answering") {
            assert!(Kind::ALL.iter().any(|k| k.name() == *n), "{n} is missing");
        }
        let unique: std::collections::BTreeSet<_> = Kind::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(unique.len(), Kind::ALL.len());
    }

    #[test]
    fn t_040_07_approvals_on_need_an_environment_and_default_off() {
        let dep = "d9d9d9d9d9d9d9d9d9d9d9d9d9d9d9d9";
        assert_eq!(ApprovalMode::default(), ApprovalMode::Off);
        assert!(Approvals::new(ApprovalMode::Off, None, dep).is_ok());
        // Off reads nothing: a deployment value it cannot parse never stops a start.
        assert_eq!(
            Approvals::new(ApprovalMode::Off, None, "<FILL>"),
            Ok(Approvals::off())
        );
        for m in [ApprovalMode::ReportOnly, ApprovalMode::Enforce] {
            assert!(Approvals::new(m, None, dep).is_err());
            assert!(Approvals::new(m, Some(" "), dep).is_err());
            assert!(Approvals::new(m, Some("test"), dep).is_ok());
        }
        assert!(Approvals::new(ApprovalMode::Enforce, Some("test"), "nope").is_err());
    }
}
