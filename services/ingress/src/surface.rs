// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The HTTP surface (`CTR-CMD`, `CTR-QRY`; `REQ-CS-1`–`REQ-CS-7`, `NFR-7`, `REQ-PV-5`).
//!
//! **The command path.** Authenticate the transport; read at most the core's invocation limit;
//! validate the shape into `ledger-core`'s types; take the client key or derive it; look it up —
//! a repeat of the same request gets the stored reply, a changed request `ERR-ClientKeyReused`;
//! mint the operation reference; append the row and commit it; then answer:
//!
//! - a transfer, and a reservation (`reserve`, `withdraw`): `202` with `{operation_reference,
//!   state: "Received", terminal: false, client_key}` at once (`REQ-CS-2`, `REQ-CS-3`,
//!   `REQ-CS-6`; amendment 0007, `OQ-61` as ruled on 2026-09-29). The decision (`Reserved`, or
//!   `Rejected` naming the refusal) follows through `operation(reference)` and the requester's
//!   notification. A reservation keeps its priority in the forwarder's queue;
//! - any other command, and always an account or security command: only once the core's
//!   decision is durable, in arrival order, and never `Received` (`REQ-CS-8`, `NFR-16`; amendment
//!   0008; `NOTES.md`, decision 3). While the core is down the request stays open for at most
//!   [`Config::decision_wait`](crate::service::Config); past it the answer is
//!   `ERR-DecisionUnavailable` (503). The command stays logged and is decided when the core
//!   returns: a retry with the same client key gets that decision (`REQ-CS-7`, amendment 0006
//!   item 8).
//!
//! A refusal is a 4xx whose body is `{"error", "description"}` (`REQ-CS-4`), with the status the
//! OpenAPI document publishes for it ([`status_of`]).
//!
//! **Deny by default** (`NFR-7`). Every route, the OpenAPI document and the metrics included,
//! authenticates first; an unknown route or method is refused the same way.
//!
//! **The access log** keeps the request id, the principal, the route's name and the status: never
//! a header, a body or a URL, so no credential can reach it (`REQ-AU-8`, `REQ-PV-5`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ledger_core::Refusal;
use ledger_core::id::OperationId;
use ledger_kernel::posting::OperationRef;
use serde::de::DeserializeOwned;

use crate::anchoring::AnchoringRoute;
use crate::answering;
use crate::auth::Authenticated;
use crate::boundary::{Core, CoreError, CoreReply, InputKey, Outcome, Query, QueryReply};
use crate::command::{
    Command, Intake, MAX_COMMAND_BYTES, derived_client_key, identity_id, request_digest_with,
};
use crate::credentials::{self, Pending};
use crate::metrics::Metrics;
use crate::pass_through::{Echo, key_of};
use crate::query::{AssetParams, MovementBody, StatementParams};
use crate::service::{Shared, mint};
use crate::settlement::SettlementRoute;
use crate::store::{
    Appended, InputKind, NewRow, Row, RowCaller, Status, Store, party_domain, principal_domain,
};
use crate::wire::{
    AccountCommand, BindAssetBody, CaptureBody, CaptureCommand, CloseAccountCommand, CommandBody,
    CommandReply, CompleteRecoveryBody, DeprecateAssetBody, EmptyBody, InitiateRecoveryBody,
    JustifiedBody, NameSuccessorBody, NameSuccessorCommand, OperationReply, OperatorCommand,
    PassThrough, PendingCommand, ReleaseCommand, RemoveCredentialCommand, ResolveBody,
    RevokeCommand, ShownBody, SubmitEvidenceBody,
};

/// The header a caller names its client key in. Optional: without it, one is derived.
pub const CLIENT_KEY: &str = "client-key";
/// The header naming whom the caller acts for, where different (`REQ-PN-1`).
pub const ON_BEHALF_OF: &str = "on-behalf-of";
/// The header carrying the request id, on every reply.
pub const REQUEST_ID: &str = "request-id";
/// The longest client key accepted, in bytes.
pub const MAX_CLIENT_KEY: usize = 256;

/// A command route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandRoute {
    /// `POST /v1/transfers`.
    Transfer,
    /// `POST /v1/reservations`.
    Reserve,
    /// `POST /v1/reservations/{reference}/capture`.
    Capture,
    /// `POST /v1/reservations/{reference}/release`.
    Release,
    /// `POST /v1/issuances`.
    Issue,
    /// `POST /v1/redemptions`.
    Redeem,
    /// `POST /v1/withdrawals`.
    Withdraw,
    /// `POST /v1/identities`.
    OpenIdentity,
    /// `POST /v1/accounts`.
    OpenAccount,
    /// `POST /v1/accounts/{account}/close`.
    CloseAccount,
    /// `POST /v1/grants`.
    GrantAuthority,
    /// `POST /v1/grants/{grant}/revoke`.
    RevokeAuthority,
    /// `POST /v1/ops/accounts/{account}/freeze`.
    FreezeAccount,
    /// `POST /v1/ops/accounts/{account}/unfreeze`.
    UnfreezeAccount,
    /// `POST /v1/ops/operations/{reference}/resume`.
    Resume,
    /// `POST /v1/ops/operations/{reference}/resolve`.
    Resolve,
    /// `POST /v1/ops/operations/{reference}/evidence`.
    SubmitEvidence,
    /// `POST /v1/ops/operations/{reference}/reversal`.
    ProposeReversal,
    /// `POST /v1/ops/reversals/{reference}/approve`.
    ApproveReversal,
    /// `POST /v1/ops/assets`.
    RegisterAsset,
    /// `POST /v1/ops/assets/{asset}/deprecate`.
    DeprecateAsset,
    /// `POST /v1/ops/assets/{asset}/bind` (amendment 0011, `REQ-AS-8`).
    BindAsset,
    /// `POST /v1/ops/issuance-grants`.
    GrantIssuance,
    /// `POST /v1/ops/issuance-grants/{grant}/revoke`.
    RevokeIssuance,
    /// `POST /v1/ops/operator-grants`.
    GrantOperator,
    /// `POST /v1/ops/operator-grants/{grant}/revoke`.
    RevokeOperator,
    /// `POST /v1/ops/adapter-support`.
    DeclareAdapterSupport,
    /// `POST /v1/credentials` (`F-012`).
    RegisterCredential,
    /// `POST /v1/credentials/{credential}/remove` (`F-012`).
    RemoveCredential,
    /// `POST /v1/accounts/{account}/custody/delegate` (`F-012`).
    DelegateCustody,
    /// `POST /v1/accounts/{account}/custody/reclaim` (`F-012`).
    ReclaimCustody,
    /// `POST /v1/recoveries/{pending}/cancel` (`F-012`).
    CancelRecovery,
    /// `POST /v1/accounts/{account}/successor` (`F-012`).
    NameSuccessor,
    /// `POST /v1/accounts/{account}/succession/claim` (`F-012`).
    ClaimSuccession,
    /// `POST /v1/ops/accounts/{account}/recovery` (`F-012`).
    InitiateRecovery,
    /// `POST /v1/ops/identities/{identity}/credentials` (`F-028`, `REQ-MG-7`).
    ImportCredentials,
    /// `POST /v1/ops/recoveries/{pending}/complete` (`F-012`).
    CompleteRecovery,
    /// `POST /v1/ops/integrating-parties` (`REQ-ID-7`, amendment 0011).
    RegisterIntegratingParty,
    /// `POST /v1/ops/integrating-parties/{party}/principals` (`REQ-ID-7`, amendment 0011).
    TiePrincipal,
    /// `POST /v1/ops/accounts/{account}/aliases` (`REQ-RC-7`, amendment 0024).
    RegisterAlias,
    /// A settlement route (`F-016`): `submit_outbound`, `configure_settlement`, or the
    /// adapter's.
    Settlement(crate::settlement::SettlementRoute),
    /// An anchoring route (`F-017`): `issue_disclosure`, `anchor_checkpoint`,
    /// `compare_settlement`.
    Anchoring(crate::anchoring::AnchoringRoute),
}

/// What a route does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handler {
    /// A command.
    Command(CommandRoute),
    /// `operation(reference)`.
    Operation,
    /// Any other query.
    Query(QueryRoute),
    /// This replica's metrics.
    Metrics,
    /// The OpenAPI document.
    OpenApi,
    /// A route that authenticates by what its body carries (`F-028`).
    Auth(AuthRoute),
    /// The caller's answering setting (`REQ-CS-9`, amendment 0013).
    Answering(AnsweringRoute),
}

/// The two routes of the caller's answering setting (`REQ-CS-9`, amendment 0013, `T-009-18`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnsweringRoute {
    /// `GET /v1/principals/self/answering`.
    Read,
    /// `POST /v1/principals/self/answering`.
    Set,
}

/// The two routes that authenticate by their body, not by a bearer (`F-028`, `DEC-19`). Both are
/// default-deny like every other (`NFR-7`): anything but a well-formed request this deployment
/// accepts is `ERR-CredentialInvalid`, and neither reveals a ledger fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRoute {
    /// `POST /v1/auth/challenge`: a challenge for a passkey ceremony.
    Challenge,
    /// `POST /v1/auth/passkey`: sign in with a passkey, and get a session.
    Passkey,
}

/// One route of the surface. The router and the OpenAPI document are both built from
/// [`ROUTES`], so neither can have a route the other lacks.
#[derive(Clone, Copy, Debug)]
pub struct Route {
    /// `GET` or `POST`.
    pub method: &'static str,
    /// The path, with `{param}` segments.
    pub path: &'static str,
    /// What it does.
    pub handler: Handler,
    /// A short name, for the access log and the OpenAPI operation id.
    pub name: &'static str,
    /// What it is.
    pub summary: &'static str,
}

const fn command(
    path: &'static str,
    route: CommandRoute,
    name: &'static str,
    summary: &'static str,
) -> Route {
    Route {
        method: "POST",
        path,
        handler: Handler::Command(route),
        name,
        summary,
    }
}

const fn query(
    path: &'static str,
    route: QueryRoute,
    name: &'static str,
    summary: &'static str,
) -> Route {
    Route {
        method: "GET",
        path,
        handler: Handler::Query(route),
        name,
        summary,
    }
}

/// Every route of the surface.
pub const ROUTES: &[Route] = &[
    Route {
        method: "POST",
        path: "/v1/auth/challenge",
        handler: Handler::Auth(AuthRoute::Challenge),
        name: "auth_challenge",
        summary: "A challenge for a passkey ceremony: sign-in or registration (F-028). No bearer: \
                  it reveals no ledger fact.",
    },
    Route {
        method: "POST",
        path: "/v1/auth/passkey",
        handler: Handler::Auth(AuthRoute::Passkey),
        name: "auth_passkey",
        summary: "Sign in with a passkey: verify its assertion and return a session token to present \
                  as a bearer (F-028). Authenticated by the assertion it carries.",
    },
    command(
        "/v1/transfers",
        CommandRoute::Transfer,
        "transfer",
        "Transfer between ledger accounts, one leg or many. Acknowledged at Received.",
    ),
    command(
        "/v1/reservations",
        CommandRoute::Reserve,
        "reserve",
        "Reserve against an account. Acknowledged at Received; the decision (Reserved or Rejected) follows by query and notification.",
    ),
    command(
        "/v1/reservations/{reference}/capture",
        CommandRoute::Capture,
        "capture",
        "Capture a reservation, matched by identity.",
    ),
    command(
        "/v1/reservations/{reference}/release",
        CommandRoute::Release,
        "release",
        "Release a reservation.",
    ),
    command(
        "/v1/issuances",
        CommandRoute::Issue,
        "issue",
        "Issue value into an account.",
    ),
    command(
        "/v1/redemptions",
        CommandRoute::Redeem,
        "redeem",
        "Redeem value out of an account.",
    ),
    command(
        "/v1/withdrawals",
        CommandRoute::Withdraw,
        "withdraw",
        "Withdraw through the settlement layer. Acknowledged at Received; the decision follows by query and notification.",
    ),
    command(
        "/v1/identities",
        CommandRoute::OpenIdentity,
        "open_identity",
        "Open an identity with its first credential: the identities principal's, the operator or the holder of the `open_identity` operator grant (REQ-ID-6, amendment 0025).",
    ),
    command(
        "/v1/accounts",
        CommandRoute::OpenAccount,
        "open_account",
        "Open a ledger account: by its owner, by the identities principal for any identity, or under a delegated grant over the owner naming `open_account` (REQ-ID-6, amendment 0025).",
    ),
    command(
        "/v1/accounts/{account}/close",
        CommandRoute::CloseAccount,
        "close_account",
        "Close a ledger account.",
    ),
    command(
        "/v1/grants",
        CommandRoute::GrantAuthority,
        "grant_authority",
        "Grant bounded authority.",
    ),
    command(
        "/v1/grants/{grant}/revoke",
        CommandRoute::RevokeAuthority,
        "revoke_authority",
        "Revoke a grant and every grant derived from it.",
    ),
    command(
        "/v1/ops/accounts/{account}/freeze",
        CommandRoute::FreezeAccount,
        "freeze_account",
        "Operator: freeze an account (F-013).",
    ),
    command(
        "/v1/ops/accounts/{account}/unfreeze",
        CommandRoute::UnfreezeAccount,
        "unfreeze_account",
        "Operator: unfreeze an account (F-013).",
    ),
    command(
        "/v1/ops/operations/{reference}/resume",
        CommandRoute::Resume,
        "resume",
        "Operator: resume an open operation past its deadline (F-007, F-013).",
    ),
    command(
        "/v1/ops/operations/{reference}/resolve",
        CommandRoute::Resolve,
        "resolve_operation",
        "Operator: resolve a Needs-Review operation on cited evidence (F-013).",
    ),
    command(
        "/v1/ops/operations/{reference}/evidence",
        CommandRoute::SubmitEvidence,
        "submit_evidence",
        "Operator: name the block and transaction that prove a Needs-Review operation's outcome, for the settlement adapter to fetch and verify (REQ-RC-5).",
    ),
    command(
        "/v1/ops/operations/{reference}/reversal",
        CommandRoute::ProposeReversal,
        "propose_reversal",
        "Operator: propose a reversal (F-013).",
    ),
    command(
        "/v1/ops/reversals/{reference}/approve",
        CommandRoute::ApproveReversal,
        "approve_reversal",
        "Operator: approve a reversal another principal proposed (F-013).",
    ),
    command(
        "/v1/ops/assets",
        CommandRoute::RegisterAsset,
        "register_asset",
        "Operator: register an asset, book-only, with its one precision (F-013, REQ-AS-7).",
    ),
    command(
        "/v1/ops/assets/{asset}/deprecate",
        CommandRoute::DeprecateAsset,
        "deprecate_asset",
        "Operator: deprecate an asset (F-013).",
    ),
    command(
        "/v1/ops/assets/{asset}/bind",
        CommandRoute::BindAsset,
        "bind_asset",
        "Issuance authority: bind a book-only asset to its settlement-layer location, at zero divergence; the layer checks that its holding equals the book's outstanding balance (amendment 0011, REQ-AS-8, DEC-20).",
    ),
    command(
        "/v1/ops/issuance-grants",
        CommandRoute::GrantIssuance,
        "grant_issuance",
        "Operator: grant issuance authority (F-013).",
    ),
    command(
        "/v1/ops/issuance-grants/{grant}/revoke",
        CommandRoute::RevokeIssuance,
        "revoke_issuance",
        "Operator: revoke issuance authority (F-013).",
    ),
    command(
        "/v1/ops/operator-grants",
        CommandRoute::GrantOperator,
        "grant_operator",
        "Operator: grant operator capabilities (F-013).",
    ),
    command(
        "/v1/ops/operator-grants/{grant}/revoke",
        CommandRoute::RevokeOperator,
        "revoke_operator",
        "Operator: revoke operator capabilities (F-013).",
    ),
    command(
        "/v1/ops/adapter-support",
        CommandRoute::DeclareAdapterSupport,
        "declare_adapter_support",
        "Operator: declare what the settlement adapter supports (F-013).",
    ),
    command(
        "/v1/credentials",
        CommandRoute::RegisterCredential,
        "register_credential",
        "Register a credential into the caller's own set, within its scope (F-012); or, by the identities principal acting for an identity it manages (`on-behalf-of`), into that identity's set (amendment 0025 R2).",
    ),
    command(
        "/v1/credentials/{credential}/remove",
        CommandRoute::RemoveCredential,
        "remove_credential",
        "Remove a credential from the caller's own set, at once (F-012).",
    ),
    command(
        "/v1/accounts/{account}/custody/delegate",
        CommandRoute::DelegateCustody,
        "delegate_custody",
        "Owned to delegated custody, by the holder only (F-012).",
    ),
    command(
        "/v1/accounts/{account}/custody/reclaim",
        CommandRoute::ReclaimCustody,
        "reclaim_custody",
        "Delegated to owned custody, with a proven credential and the mechanisms acknowledged (F-012).",
    ),
    command(
        "/v1/recoveries/{pending}/cancel",
        CommandRoute::CancelRecovery,
        "cancel_recovery",
        "Cancel a pending recovery or succession, with a credential of the holder (F-012).",
    ),
    command(
        "/v1/accounts/{account}/successor",
        CommandRoute::NameSuccessor,
        "name_successor",
        "Name the account's successor, or clear the name (F-012).",
    ),
    command(
        "/v1/accounts/{account}/succession/claim",
        CommandRoute::ClaimSuccession,
        "claim_succession",
        "The named successor's claim: a pending succession, then, after its delay, the account (F-012).",
    ),
    command(
        "/v1/ops/accounts/{account}/recovery",
        CommandRoute::InitiateRecovery,
        "initiate_recovery",
        "Operator: initiate the recovery of an owned-custody account on the holder's out-of-band request (F-012).",
    ),
    command(
        "/v1/ops/identities/{identity}/credentials",
        CommandRoute::ImportCredentials,
        "import_credentials",
        "Operator: import credentials' public material into an identity's set; refused while an account of the identity is owned (REQ-MG-7, REQ-MG-8, F-028).",
    ),
    command(
        "/v1/ops/integrating-parties",
        CommandRoute::RegisterIntegratingParty,
        "register_integrating_party",
        "Operator: register an integrating party, identified by its organisation identity, with the principals tied to it. The operator's policy authority alone; no grant gives it (REQ-ID-7).",
    ),
    command(
        "/v1/ops/integrating-parties/{party}/principals",
        CommandRoute::TiePrincipal,
        "tie_principal",
        "Operator: tie a principal to a registered party; from then on its client keys are the party's (REQ-ID-7, REQ-OP-10). A principal is tied to at most one party, for good.",
    ),
    query(
        "/v1/ops/integrating-parties",
        QueryRoute::IntegratingParties,
        "integrating_parties",
        "Operator: every registered integrating party and the principals tied to it (REQ-ID-7).",
    ),
    command(
        "/v1/ops/accounts/{account}/aliases",
        CommandRoute::RegisterAlias,
        "register_alias",
        "Operator: name the account by an alias, (source, name), as a seed's aliases do; a system account's role is one. An alias names one account for good. The operator's policy authority alone (REQ-RC-7).",
    ),
    query(
        "/v1/aliases",
        QueryRoute::Alias,
        "alias",
        "The account an alias names, for a principal that reaches it; ERR-UnknownAccount otherwise, and ERR-IdentifierArchived for a prior system's identifier the initial state left out (REQ-RC-7).",
    ),
    query(
        "/v1/accounts/{account}",
        QueryRoute::Account,
        "account",
        "An account's owner, kind, custody and status: for whoever reaches it, the operator, the identities principal, and a holder of a grant over its owner (amendment 0028).",
    ),
    query(
        "/v1/accounts/{account}/grants",
        QueryRoute::Grants,
        "grants",
        "Every grant over the account, live and revoked: the operator's, or the account owner's (REQ-RC-7).",
    ),
    query(
        "/v1/identities/{identity}/grants",
        QueryRoute::IdentityGrants,
        "identity_grants",
        "The grants over the identity, live and revoked, with their ids, ops, bounds and parent: every one for the identity itself, the operator and the identities principal; a holder's own for any other caller; ERR-NotAuthorised to a caller holding none (amendment 0026).",
    ),
    command(
        "/v1/ops/recoveries/{pending}/complete",
        CommandRoute::CompleteRecovery,
        "complete_recovery",
        "Operator: complete a pending recovery with the recovery authority's approval, after its delay (F-012).",
    ),
    command(
        "/v1/withdrawals/{reference}/submit",
        CommandRoute::Settlement(SettlementRoute::SubmitOutbound),
        "submit_outbound",
        "Hand a reserved withdrawal to the settlement adapter, to a destination (F-010, F-016).",
    ),
    command(
        "/v1/ops/settlement",
        CommandRoute::Settlement(SettlementRoute::Configure),
        "configure_settlement",
        "Operator: the settlement configuration, naming the adapter's identity; set once (F-010).",
    ),
    command(
        "/v1/settlement/declarations",
        CommandRoute::Settlement(SettlementRoute::Declare),
        "declare_adapter",
        "Settlement adapter: its status and what it supports (F-016). The adapter's principal only.",
    ),
    command(
        "/v1/settlement/acknowledgements",
        CommandRoute::Settlement(SettlementRoute::Acknowledge),
        "ack_instruction",
        "Settlement adapter: an instruction is durable in its store (F-016). The adapter's principal only.",
    ),
    command(
        "/v1/settlement/signings",
        CommandRoute::Settlement(SettlementRoute::Sign),
        "record_signing",
        "Settlement adapter: the one transaction it signed for an instruction, before it submits it, or that it never will sign one (F-016, owner ruling 2026-09-24).",
    ),
    command(
        "/v1/settlement/observations",
        CommandRoute::Settlement(SettlementRoute::Observe),
        "record_observation",
        "Settlement adapter: a record of the settlement layer, at its coordinate (F-016).",
    ),
    command(
        "/v1/settlement/checks",
        CommandRoute::Settlement(SettlementRoute::Check),
        "record_check",
        "Settlement adapter: a canonical check of a coordinate already observed (F-016).",
    ),
    command(
        "/v1/settlement/heads",
        CommandRoute::Settlement(SettlementRoute::Head),
        "record_head",
        "Settlement adapter: the finalised head of an observation cycle (F-016).",
    ),
    command(
        "/v1/settlement/gaps",
        CommandRoute::Settlement(SettlementRoute::Gap),
        "record_gap",
        "Settlement adapter: a range it was forced to skip, visible to operators (F-016).",
    ),
    command(
        "/v1/settlement/rescans",
        CommandRoute::Settlement(SettlementRoute::Rescan),
        "record_rescan",
        "Settlement adapter: a re-scan of a range; every difference is an incident (F-016).",
    ),
    query(
        "/v1/settlement/instructions",
        QueryRoute::Instructions,
        "instructions",
        "Settlement adapter: the instructions it has not acknowledged (F-016). The adapter's principal only.",
    ),
    query(
        "/v1/settlement/calls",
        QueryRoute::Calls,
        "calls",
        "Settlement executor: the call outbox, every external movement and anchor as an envelope — reference, target, call, expiry — not yet taken over (F-027). The adapter's principal only.",
    ),
    query(
        "/v1/settlement/evidence",
        QueryRoute::Evidence,
        "pending_evidence",
        "Settlement adapter: the evidence authorities submitted for operations in Needs-Review, not yet checked, each a block and a transaction to fetch and verify on its own (REQ-RC-5). The adapter's principal only.",
    ),
    command(
        "/v1/settlement/evidence-checks",
        CommandRoute::Settlement(SettlementRoute::EvidenceChecked),
        "evidence_checked",
        "Settlement adapter: it fetched and verified submitted evidence and reported the records it found; the core resolves the operation on what they prove, or finds the evidence unprovable (REQ-RC-5).",
    ),
    command(
        "/v1/settlement/attestations",
        CommandRoute::Settlement(SettlementRoute::Attest),
        "attest_asset",
        "Settlement adapter: what the settlement layer's metadata states about an asset location it binds, at a finalised height (DEC-20). Registration reads it; a difference from the registry holds the asset. The adapter's principal only.",
    ),
    command(
        "/v1/settlement/anchor-acknowledgements",
        CommandRoute::Settlement(SettlementRoute::AcknowledgeAnchor),
        "ack_anchor",
        "Settlement adapter: an anchor instruction is durable in its store (F-017). The adapter's principal only.",
    ),
    command(
        "/v1/settlement/anchor-failures",
        CommandRoute::Settlement(SettlementRoute::AnchorNotExecuted),
        "anchor_not_executed",
        "Settlement adapter: its anchor extrinsic provably did not execute; the anchor is emitted again (F-017).",
    ),
    command(
        "/v1/settlement/positions",
        CommandRoute::Settlement(SettlementRoute::Positions),
        "record_positions",
        "Settlement adapter: the settlement layer's positions at a finalized block, compared at once (F-017).",
    ),
    query(
        "/v1/settlement/anchor",
        QueryRoute::Anchor,
        "anchor_instruction",
        "Settlement adapter: the checkpoint in flight and whether a comparison is due (F-017). The adapter's principal only.",
    ),
    command(
        "/v1/disclosures",
        CommandRoute::Anchoring(AnchoringRoute::IssueDisclosure),
        "issue_disclosure",
        "Issue a disclosure: a signed assertion about the subject, anchored at a final checkpoint (F-017).",
    ),
    query(
        "/v1/disclosures/verify",
        QueryRoute::VerifyDisclosure,
        "verify_disclosure",
        "Verify a disclosure against this deployment's final checkpoints and clerk key; no side effect (F-017).",
    ),
    command(
        "/v1/ops/anchors",
        CommandRoute::Anchoring(AnchoringRoute::AnchorCheckpoint),
        "anchor_checkpoint",
        "Operator: create a checkpoint now, or answer the latest when nothing is new (F-017).",
    ),
    query(
        "/v1/ops/anchors",
        QueryRoute::Anchoring,
        "anchoring",
        "Operator: every checkpoint, the recorded settlement comparisons, and anchoring's incidents (F-017).",
    ),
    command(
        "/v1/ops/settlement/comparisons",
        CommandRoute::Anchoring(AnchoringRoute::CompareSettlement),
        "compare_settlement",
        "Operator: compare the book with the settlement layer's positions at the latest cut (F-017).",
    ),
    query(
        "/v1/identities/{identity}/recovery-terms",
        QueryRoute::RecoveryTerms,
        "recovery_terms",
        "The recovery mechanisms a new owned-custody account would have, and the digest open_account carries (F-012).",
    ),
    query(
        "/v1/accounts/{account}/recovery-mechanisms",
        QueryRoute::RecoveryMechanisms,
        "recovery_mechanisms",
        "The recovery mechanisms that apply to an account, and the digest reclaim_custody carries (F-012).",
    ),
    query(
        "/v1/accounts/{account}/custody",
        QueryRoute::Custody,
        "custody_view",
        "An account's custody, its recoveries and successions, and its named successor (F-012).",
    ),
    Route {
        method: "GET",
        path: "/v1/operations/{reference}",
        handler: Handler::Operation,
        name: "operation",
        summary: "An operation's state, with its class.",
    },
    query(
        "/v1/balances",
        QueryRoute::Balances,
        "balances",
        "Balances by account or identity, each kind labelled, from one view.",
    ),
    query(
        "/v1/movements",
        QueryRoute::Movements,
        "movements",
        "A page of movements, filtered, with a cursor. A limit is required.",
    ),
    query(
        "/v1/statements",
        QueryRoute::Statement,
        "statement",
        "A statement for a period. The period is required.",
    ),
    query(
        "/v1/assets",
        QueryRoute::Assets,
        "assets",
        "The asset registry.",
    ),
    query(
        "/v1/trial-balance",
        QueryRoute::TrialBalance,
        "trial_balance",
        "The trial balance, on commitments and on openings. The operator's.",
    ),
    query(
        "/v1/accounts/{account}/can-spend",
        QueryRoute::CanSpend,
        "can_spend",
        "Whether a spend would pass the checks before the kernel, or why not.",
    ),
    query(
        "/v1/notifications/undelivered",
        QueryRoute::Undelivered,
        "undelivered",
        "Undelivered notifications with their age. The operator's.",
    ),
    query(
        "/v1/ops/signals",
        QueryRoute::Signals,
        "signals",
        "The operational signals: counts and ages, never a ledger fact. The operator's, with the read_signals kind.",
    ),
    Route {
        method: "GET",
        path: "/metrics",
        handler: Handler::Metrics,
        name: "metrics",
        summary: "This replica's metrics, in the Prometheus text format.",
    },
    Route {
        method: "GET",
        path: "/v1/principals/self/answering",
        handler: Handler::Answering(AnsweringRoute::Read),
        name: "answering",
        summary: "The caller's answering setting: whether its transfers and reservations are \
                  answered with their decision, and the wait (REQ-CS-9).",
    },
    Route {
        method: "POST",
        path: "/v1/principals/self/answering",
        handler: Handler::Answering(AnsweringRoute::Set),
        name: "set_answering",
        summary: "Set the caller's answering setting: its transfers and reservations taken in from \
                  now on are acknowledged at Received (the default) or answered with their \
                  decision, waiting at most wait_ms (REQ-CS-9). Answered with its result.",
    },
    Route {
        method: "GET",
        path: "/openapi.json",
        handler: Handler::OpenApi,
        name: "openapi",
        summary: "This document.",
    },
];

/// The HTTP status of each refusal. The OpenAPI document publishes the same table.
pub fn status_of(r: Refusal) -> StatusCode {
    use Refusal::*;
    match r {
        CredentialInvalid => StatusCode::UNAUTHORIZED,
        NotAuthorised | AuthorityRevoked | IdentityRestricted | NotDisclosable => {
            StatusCode::FORBIDDEN
        }
        UnknownAccount | UnknownAsset | UnknownOperation => StatusCode::NOT_FOUND,
        ClientKeyReused
        | OperationTerminal
        | InvalidTransition
        | ObservationDuplicate
        | InitialStateNotSealed => StatusCode::CONFLICT,
        IdentifierArchived => StatusCode::GONE,
        BatchTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        MalformedCommand => StatusCode::BAD_REQUEST,
        DecisionUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        Internal => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

/// Who the access log names for a request.
#[derive(Clone)]
struct Logged {
    principal: String,
    route: &'static str,
}

/// The `Retry-After` an `ERR-DecisionUnavailable` carries, in seconds (`DEC-27` (g), the owner's
/// ruling on `T-038-23`): one second. It is `NFR-16`'s bound on a decision, and the order of a
/// planned hand-over of the feed to another ingress instance (well under a second, `F-038` §9), so
/// a client retrying with the same client key after it usually finds a core deciding again. A
/// crashed instance takes a few seconds (`dead_after` and the lease), which a retry after one
/// second answers again with this same condition, never with a different outcome.
pub const DECISION_RETRY_AFTER_SECS: u64 = 1;

/// A refusal as the surface sends it. `ERR-DecisionUnavailable` is a 503 with its named body and a
/// `Retry-After` ([`DECISION_RETRY_AFTER_SECS`]): ingress answering, with nothing decided yet,
/// never a bare gateway error.
pub fn refusal(r: Refusal) -> Response {
    let mut resp = (status_of(r), axum::Json(r)).into_response();
    if r == Refusal::DecisionUnavailable {
        resp.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            HeaderValue::from(DECISION_RETRY_AFTER_SECS),
        );
    }
    resp
}

fn logged(mut resp: Response, who: Option<&Authenticated>, route: &'static str) -> Response {
    resp.extensions_mut().insert(Logged {
        principal: who.map_or_else(|| "-".to_owned(), |a| a.principal.clone()),
        route,
    });
    resp
}

async fn access_log(req: Request, next: Next) -> Response {
    let mut id = [0u8; 8];
    let _ = getrandom::fill(&mut id);
    let id = hex::encode(id);
    let method = req.method().clone();
    let mut resp = next.run(req).await;
    let (principal, route) = resp
        .extensions()
        .get::<Logged>()
        .map_or(("-".to_owned(), "-"), |l| (l.principal.clone(), l.route));
    tracing::info!(
        target: "aledgent_ingress::access",
        request_id = %id,
        principal = %principal,
        method = %method,
        route = route,
        status = resp.status().as_u16(),
        "access"
    );
    if let Ok(v) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert(REQUEST_ID, v);
    }
    resp
}

fn json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Refusal> {
    let bytes: &[u8] = if bytes.is_empty() { b"{}" } else { bytes };
    serde_json::from_slice(bytes).map_err(|_| Refusal::MalformedCommand)
}

/// A command route's body into the command the core receives, and, for a credential command, the
/// work left before it can be logged (`F-028`).
fn build(
    route: CommandRoute,
    path: Option<String>,
    bytes: &[u8],
) -> Result<(CommandBody, Option<Pending>), Refusal> {
    let param = || path.clone().ok_or(Refusal::MalformedCommand);
    match route {
        CommandRoute::RegisterCredential => return credentials::build_register(json(bytes)?),
        CommandRoute::ReclaimCustody => return credentials::build_reclaim(param()?, json(bytes)?),
        _ => {}
    }
    Ok((build_plain(route, path, bytes)?, None))
}

fn build_plain(
    route: CommandRoute,
    path: Option<String>,
    bytes: &[u8],
) -> Result<CommandBody, Refusal> {
    let param = || path.clone().ok_or(Refusal::MalformedCommand);
    Ok(match route {
        CommandRoute::Transfer => CommandBody::Transfer(json(bytes)?),
        CommandRoute::Reserve => CommandBody::Reserve(json(bytes)?),
        CommandRoute::Capture => {
            let b: CaptureBody = json(bytes)?;
            CommandBody::Capture(CaptureCommand {
                reservation: param()?,
                destination: b.destination,
                cleared: b.cleared,
            })
        }
        CommandRoute::Release => {
            let _: EmptyBody = json(bytes)?;
            CommandBody::Release(ReleaseCommand {
                reservation: param()?,
            })
        }
        CommandRoute::Issue => CommandBody::Issue(json(bytes)?),
        CommandRoute::Redeem => CommandBody::Redeem(json(bytes)?),
        CommandRoute::Withdraw => CommandBody::Withdraw(json(bytes)?),
        CommandRoute::OpenIdentity => CommandBody::OpenIdentity(json(bytes)?),
        CommandRoute::OpenAccount => CommandBody::OpenAccount(json(bytes)?),
        CommandRoute::CloseAccount => {
            let _: EmptyBody = json(bytes)?;
            CommandBody::CloseAccount(CloseAccountCommand { account: param()? })
        }
        CommandRoute::GrantAuthority => CommandBody::GrantAuthority(json(bytes)?),
        CommandRoute::RevokeAuthority => {
            let _: EmptyBody = json(bytes)?;
            CommandBody::RevokeAuthority(RevokeCommand { grant: param()? })
        }
        // Built with their pending work, above.
        CommandRoute::RegisterCredential | CommandRoute::ReclaimCustody => {
            return Err(Refusal::NotAuthorised);
        }
        CommandRoute::RemoveCredential => {
            let _: EmptyBody = json(bytes)?;
            CommandBody::RemoveCredential(RemoveCredentialCommand {
                credential: param()?,
            })
        }
        CommandRoute::DelegateCustody => {
            let _: EmptyBody = json(bytes)?;
            CommandBody::DelegateCustody(AccountCommand { account: param()? })
        }
        CommandRoute::CancelRecovery => {
            let _: EmptyBody = json(bytes)?;
            CommandBody::CancelRecovery(PendingCommand { pending: param()? })
        }
        CommandRoute::NameSuccessor => {
            let b: NameSuccessorBody = json(bytes)?;
            CommandBody::NameSuccessor(NameSuccessorCommand {
                account: param()?,
                successor: b.successor,
            })
        }
        CommandRoute::ClaimSuccession => {
            let _: EmptyBody = json(bytes)?;
            CommandBody::ClaimSuccession(AccountCommand { account: param()? })
        }
        CommandRoute::Settlement(s) => s.build(path, bytes)?,
        CommandRoute::Anchoring(a) => a.build(bytes)?,
        route => CommandBody::Operator(operator_command(route, path.clone(), bytes)?),
    })
}

/// An operator command from its route, its path parameter and its body.
fn operator_command(
    route: CommandRoute,
    param: Option<String>,
    bytes: &[u8],
) -> Result<OperatorCommand, Refusal> {
    let justification =
        || -> Result<String, Refusal> { json::<JustifiedBody>(bytes).map(|b| b.justification) };
    let path = param.ok_or(Refusal::MalformedCommand);
    Ok(match route {
        CommandRoute::FreezeAccount => OperatorCommand::FreezeAccount {
            account: path?,
            justification: justification()?,
        },
        CommandRoute::UnfreezeAccount => OperatorCommand::UnfreezeAccount {
            account: path?,
            justification: justification()?,
        },
        CommandRoute::Resume => OperatorCommand::Resume {
            operation: path?,
            justification: justification()?,
        },
        CommandRoute::Resolve => {
            let b: ResolveBody = json(bytes)?;
            OperatorCommand::Resolve {
                operation: path?,
                target: b.target,
                evidence: b.evidence,
                justification: b.justification,
            }
        }
        CommandRoute::SubmitEvidence => {
            let b: SubmitEvidenceBody = json(bytes)?;
            OperatorCommand::SubmitEvidence {
                operation: path?,
                block: b.block,
                transaction: b.transaction,
                justification: b.justification,
            }
        }
        CommandRoute::ProposeReversal => OperatorCommand::ProposeReversal {
            operation: path?,
            justification: justification()?,
        },
        CommandRoute::ApproveReversal => OperatorCommand::ApproveReversal {
            proposal: path?,
            justification: justification()?,
        },
        CommandRoute::RegisterAsset => OperatorCommand::RegisterAsset(json(bytes)?),
        CommandRoute::DeprecateAsset => {
            let b: DeprecateAssetBody = json(bytes)?;
            OperatorCommand::DeprecateAsset {
                asset: path?,
                review: b.review,
                justification: b.justification,
            }
        }
        CommandRoute::BindAsset => {
            let b: BindAssetBody = json(bytes)?;
            OperatorCommand::BindAsset {
                asset: path?,
                location: b.location,
                justification: b.justification,
            }
        }
        CommandRoute::GrantIssuance => OperatorCommand::GrantIssuance(json(bytes)?),
        CommandRoute::RevokeIssuance => OperatorCommand::RevokeIssuance {
            grant: path?,
            justification: justification()?,
        },
        CommandRoute::GrantOperator => OperatorCommand::GrantOperator(json(bytes)?),
        CommandRoute::RevokeOperator => OperatorCommand::RevokeOperator {
            grant: path?,
            justification: justification()?,
        },
        CommandRoute::DeclareAdapterSupport => OperatorCommand::DeclareAdapterSupport(json(bytes)?),
        CommandRoute::InitiateRecovery => {
            let b: InitiateRecoveryBody = json(bytes)?;
            OperatorCommand::InitiateRecovery {
                account: path?,
                verifier: b.verifier,
                scheme: b.scheme,
                scope: b.scope,
                justification: b.justification,
            }
        }
        CommandRoute::ImportCredentials => {
            let b: crate::wire::ImportCredentialsBody = json(bytes)?;
            OperatorCommand::ImportCredentials {
                identity: path?,
                credentials: b.credentials,
                justification: b.justification,
            }
        }
        CommandRoute::CompleteRecovery => {
            let b: CompleteRecoveryBody = json(bytes)?;
            OperatorCommand::CompleteRecovery {
                pending: path?,
                approval: b.approval,
                justification: b.justification,
            }
        }
        CommandRoute::RegisterIntegratingParty => {
            let b: crate::wire::RegisterIntegratingPartyBody = json(bytes)?;
            OperatorCommand::RegisterIntegratingParty {
                party: b.party,
                principals: b.principals,
                justification: b.justification,
            }
        }
        CommandRoute::TiePrincipal => {
            let b: crate::wire::TiePrincipalBody = json(bytes)?;
            OperatorCommand::TiePrincipal {
                party: path?,
                principal: b.principal,
                justification: b.justification,
            }
        }
        CommandRoute::RegisterAlias => {
            let b: crate::wire::RegisterAliasBody = json(bytes)?;
            OperatorCommand::RegisterAlias {
                account: path?,
                source: b.source,
                name: b.name,
                justification: b.justification,
            }
        }
        _ => return Err(Refusal::NotAuthorised),
    })
}

/// Take the pass-through data off a command that submits an operation (`REQ-PO-5`). Every other
/// command's body has no such field, so it refuses one as unknown (`ERR-MalformedCommand`).
fn take_pass_through(body: &mut CommandBody) -> Option<PassThrough> {
    match body {
        CommandBody::Transfer(b) => b.reference.take(),
        CommandBody::Reserve(b) => b.reference.take(),
        CommandBody::Withdraw(b) => b.reference.take(),
        CommandBody::Issue(b) => b.reference.take(),
        CommandBody::Redeem(b) => b.reference.take(),
        // The identities principal's justification, as pass-through (amendment 0025 R1).
        CommandBody::OpenIdentity(b) => b.reference.take(),
        CommandBody::OpenAccount(b) => b.reference.take(),
        _ => None,
    }
}

fn too_large(route: CommandRoute) -> Refusal {
    if route == CommandRoute::Transfer {
        Refusal::BatchTooLarge
    } else {
        Refusal::MalformedCommand
    }
}

fn header_str(headers: &HeaderMap, name: &str) -> Result<Option<String>, Refusal> {
    match headers.get(name) {
        None => Ok(None),
        Some(v) => v
            .to_str()
            .map(|s| Some(s.to_owned()))
            .map_err(|_| Refusal::MalformedCommand),
    }
}

fn to_op(key: InputKey) -> OperationId {
    OperationId::from_ref(OperationRef(key.0))
}

/// The core's reply a decided row keeps.
fn core_reply_of(row: &Row) -> Option<CoreReply> {
    row.core_reply
        .as_deref()
        .and_then(|b| serde_json::from_slice(b).ok())
}

/// A decided row's reply (`reply`, the core's, as the row keeps it), carrying `reference`: the
/// pass-through data of the operation it names ([`reply_reference`]).
fn decided_reply(
    row: &Row,
    reply: Option<CoreReply>,
    api_key: Option<String>,
    reference: Option<PassThrough>,
) -> Response {
    let key = row.new.client_key.clone().unwrap_or_default();
    match reply.map(|r| r.outcome) {
        Some(Outcome::Decided {
            operation_reference,
            state,
            result,
        }) => {
            let terminal = state.as_ref().is_none_or(|s| s.terminal);
            let body = CommandReply {
                operation_reference,
                state: state.as_ref().map(|s| s.state.clone()),
                class: state.as_ref().map(|s| s.class),
                terminal,
                client_key: key,
                client_key_derived: row.new.key_derived,
                result: Some(result),
                api_key,
                reference,
            };
            (StatusCode::OK, axum::Json(body)).into_response()
        }
        Some(Outcome::Refused { error, .. }) => refusal(error),
        // A command row always carries a command's reply; anything else fails closed.
        _ => refusal(Refusal::NotAuthorised),
    }
}

/// The acknowledgement of a command taken in at `Received`.
fn acknowledged(row: &Row) -> Response {
    let shown = ShownBody::received();
    let body = CommandReply {
        operation_reference: to_op(row.new.op_ref).to_did(),
        state: Some(shown.state),
        class: Some(shown.class),
        terminal: false,
        client_key: row.new.client_key.clone().unwrap_or_default(),
        client_key_derived: row.new.key_derived,
        result: None,
        api_key: None,
        // Its own: whoever reaches this row reaches it under its client key's domain, the
        // sender's integrating party or the sender itself (`REQ-OP-10`, `REQ-PO-5`).
        reference: row.new.pass_through.clone(),
    };
    (StatusCode::ACCEPTED, axum::Json(body)).into_response()
}

/// The pass-through data a decided row's reply carries (`REQ-PO-5`): the data of the operation
/// the reply names. A command that submitted its own operation carries its own. A `capture` or a
/// `release` names the reservation it acts on (`INV-14`), so it carries the reservation's, if
/// `reader` may see it. `Err` if the store did not answer.
async fn reply_reference<S: Store, C: Core>(
    shared: &Shared<S, C>,
    reader: &str,
    row: &Row,
    reply: Option<&CoreReply>,
) -> Result<Option<PassThrough>, ()> {
    let named = reply.and_then(|r| match &r.outcome {
        Outcome::Decided {
            operation_reference,
            ..
        } => key_of(operation_reference).map(|k| (k, operation_reference)),
        _ => None,
    });
    match named {
        Some((key, operation)) if key != row.new.op_ref => Echo::new(shared, reader)
            .of_operation(operation)
            .await
            .map_err(|e| tracing::error!(error = %e, "the store did not answer an echo")),
        _ => Ok(row.new.pass_through.clone()),
    }
}

/// [`decided_reply`] with its pass-through data; `ERR-Internal` if the store did not answer
/// (`REQ-ER-2`): a retry under the same client key gets the reply.
async fn decided<S: Store, C: Core>(
    shared: &Shared<S, C>,
    reader: &str,
    row: &Row,
    api_key: Option<String>,
) -> Response {
    let reply = core_reply_of(row);
    match reply_reference(shared, reader, row, reply.as_ref()).await {
        Ok(reference) => decided_reply(row, reply, api_key, reference),
        Err(()) => refusal(Refusal::Internal),
    }
}

async fn answer<S: Store, C: Core>(
    shared: &Shared<S, C>,
    reader: &str,
    row: Row,
    api_key: Option<String>,
    wait: Duration,
) -> Response {
    if row.status == Status::Decided {
        return decided(shared, reader, &row, api_key).await;
    }
    if !row.new.synchronous {
        return acknowledged(&row);
    }
    // REQ-CS-3: the reply waits for the core's durable decision, for at most `wait`: the
    // deployment's `decision_wait`, or a principal's answering wait for its transfers and
    // reservations (`REQ-CS-9`, amendment 0013), which never exceeds it. Past it the command is
    // kept, and a retry under the same client key gets the decision once it is made
    // (`REQ-CS-7`, `ERR-DecisionUnavailable`, amendment 0006 item 8). While no replica holds a
    // live feed lease (no core connected, or a handover), at most `NFR-16` (`F-038` §8).
    let wait = shared.decision_wait(wait);
    shared.wake.notify_one();
    let waited = tokio::time::timeout(wait, async {
        loop {
            match shared.wait_decided(row.new.op_ref).await {
                Ok(r) => return r,
                Err(e) => {
                    tracing::warn!(error = %e, "the store did not answer a waiting caller");
                    tokio::time::sleep(shared.config.poll).await;
                }
            }
        }
    })
    .await;
    match waited {
        Ok(r) => decided(shared, reader, &r, api_key).await,
        Err(_) => {
            Metrics::inc(&shared.metrics.decisions_unavailable);
            refusal(Refusal::DecisionUnavailable)
        }
    }
}

async fn handle_command<S: Store, C: Core>(
    shared: Arc<Shared<S, C>>,
    route: CommandRoute,
    name: &'static str,
    param: Option<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // `NFR-1`'s series starts here, at the request's arrival, before authentication (`T-031-13`,
    // finding F4 of `F-032`): an API key's lookup is part of what the caller waits for.
    let started = Instant::now();
    // The same instant on the ingress clock, logged with the row (`T-009-25`).
    let arrived_at = shared.clock.now();
    let who = match credentials::authenticate(&shared, &headers).await {
        Ok(a) => a,
        Err(r) => return logged(refusal(r), None, name),
    };
    let auth = micros_since(started);
    // T-036-25 (`REQ-RC-6`, `a0022-q2b`): a chainspec book whose `verify-genesis` report is not
    // installed takes no command, and nothing of it is logged. Reads are not gated.
    if !crate::service::takes_commands(&shared).await {
        return logged(refusal(Refusal::InitialStateNotSealed), Some(&who), name);
    }
    let arrival = Arrival {
        at: started,
        clock: arrived_at,
        auth_us: auth,
    };
    let reply = command_reply(&shared, route, param, &headers, body, &who, arrival).await;
    if route == CommandRoute::Reserve || route == CommandRoute::Withdraw {
        let us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        shared.metrics.reservation(us);
    }
    logged(reply, Some(&who), name)
}

fn micros_since(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// When a command's request arrived, and what authenticating it took.
#[derive(Clone, Copy)]
struct Arrival {
    /// The monotonic instant: the in-process series start here (`NFR-1`, `NFR-16`).
    at: Instant,
    /// The ingress clock at that instant: the row's `arrived_at` (`T-009-25`).
    clock: ledger_core::account::Time,
    /// Authentication, microseconds.
    auth_us: u64,
}

async fn command_reply<S: Store, C: Core>(
    shared: &Shared<S, C>,
    route: CommandRoute,
    param: Option<String>,
    headers: &HeaderMap,
    body: Body,
    who: &Authenticated,
    arrival: Arrival,
) -> Response {
    let Arrival {
        at: arrived,
        clock: arrived_at,
        auth_us,
    } = arrival;
    let Ok(bytes) = axum::body::to_bytes(body, MAX_COMMAND_BYTES).await else {
        return refusal(too_large(route));
    };
    // REQ-RC-7: a prior system's identifier the initial state left out is answered for what it
    // is, wherever the request names it; nothing of it is logged.
    if let Err(r) = shared.config.archived.check(param.as_deref(), None, &bytes) {
        return refusal(r);
    }
    let (mut body, pending) = match build(route, param, &bytes) {
        Ok(x) => x,
        Err(r) => return refusal(r),
    };
    // REQ-PO-5: the caller's pass-through data comes off the command here. It is part of the
    // request (its digest) and of the row, never of the canonical body the core receives.
    let pass_through = take_pass_through(&mut body);
    // A credential command with work left (`F-028`) is deduplicated on what the caller sent: the
    // command before ingress fills it, and the proof or registration it carries. So a repeat is
    // found before a single-use proof is verified again, and an API key's fresh secret does not
    // make a repeat look like a new request.
    // The command is not complete yet (a reclaim's proof is still to be checked), so the basis is
    // its wire form, not its validated canonical form.
    let basis = pending.as_ref().map(|p| {
        let mut b = serde_json::to_vec(&body).unwrap_or_default();
        b.extend_from_slice(&p.fingerprint());
        b
    });
    // A reclaim whose proof is still to be checked parses as the command it will be once
    // checked; that copy only names the surface and is never logged.
    let shape = match &body {
        CommandBody::ReclaimCustody(c) if c.signature.is_none() && pending.is_some() => {
            let mut c = c.clone();
            c.passkey_checked = true;
            CommandBody::ReclaimCustody(c)
        }
        b => b.clone(),
    };
    let command = match Command::parse(&shape) {
        Ok(c) => c,
        Err(r) => return refusal(r),
    };
    let canonical = command.canonical();
    if canonical.len() > MAX_COMMAND_BYTES {
        return refusal(too_large(route));
    }
    // Only passkeys of an accepted relying party are imported (`REQ-MG-7`, `DEC-19`).
    if let Err(r) = credentials::check_import(shared, &body) {
        return refusal(r);
    }
    let on_behalf_of = match header_str(headers, ON_BEHALF_OF) {
        Ok(v) => v,
        Err(r) => return refusal(r),
    };
    let obo = match on_behalf_of.as_deref().map(identity_id).transpose() {
        Ok(v) => v,
        Err(r) => return refusal(r),
    };
    let Ok(principal) = identity_id(&who.principal) else {
        return refusal(Refusal::CredentialInvalid);
    };
    let digest = request_digest_with(
        &principal,
        obo.as_ref(),
        command.surface(),
        basis.as_deref().unwrap_or(&canonical),
        pass_through.as_ref(),
    );
    let (client_key, derived) = match header_str(headers, CLIENT_KEY) {
        Err(r) => return refusal(r),
        Ok(Some(k)) if k.is_empty() || k.len() > MAX_CLIENT_KEY => {
            return refusal(Refusal::MalformedCommand);
        }
        Ok(Some(k)) => (k, false),
        Ok(None) => (derived_client_key(command.surface(), &digest), true),
    };
    // The client key's domain (`REQ-OP-10`, T-009-16): the principal's integrating party, or the
    // principal itself. A key the principal used before it was tied stays in its own domain, and
    // only it can reach that row: a repeat is answered from it, a changed request refused.
    let own = principal_domain(&who.principal);
    let party_started = Instant::now();
    let domain = match crate::service::party_of(shared, &who.principal).await {
        Some(party) => party_domain(&party),
        None => own.clone(),
    };
    let party_us = micros_since(party_started);
    if domain != own {
        match shared
            .store
            .find(&own, command.surface(), &client_key)
            .await
        {
            Ok(Some(row)) if row.new.body_digest != digest.0 => {
                return refusal(Refusal::ClientKeyReused);
            }
            Ok(Some(row)) => {
                let wait = answering::wait_for(shared, &command, &who.principal).await;
                return answer(shared, &who.principal, row, None, wait).await;
            }
            Ok(None) => {}
            Err(_) => return refusal(Refusal::DecisionUnavailable),
        }
    }
    let mut issued = None;
    // F-038 §7: a `key` proof is checked at the door, like a passkey's.
    let door = credentials::has_key_proof(&body);
    let (command, canonical) = if pending.is_none() && !door {
        (command, canonical)
    } else {
        // A repeat: its stored reply, without verifying anything again.
        match shared
            .store
            .find(&domain, command.surface(), &client_key)
            .await
        {
            Ok(Some(row)) if row.new.body_digest != digest.0 => {
                return refusal(Refusal::ClientKeyReused);
            }
            Ok(Some(row)) => {
                return answer(
                    shared,
                    &who.principal,
                    row,
                    None,
                    shared.config.decision_wait,
                )
                .await;
            }
            Ok(None) => {}
            Err(_) => return refusal(Refusal::DecisionUnavailable),
        }
        if door && let Err(r) = credentials::check_key_proof(shared, who, &body).await {
            return refusal(r);
        }
        match pending {
            None => (command, canonical),
            Some(p) => {
                match credentials::prepare(shared, who, obo.as_ref(), &mut body, p).await {
                    Ok(secret) => issued = secret,
                    Err(r) => return refusal(r),
                }
                let c = match Command::parse(&body) {
                    Ok(c) => c,
                    Err(r) => return refusal(r),
                };
                let canonical = c.canonical();
                if canonical.len() > MAX_COMMAND_BYTES {
                    return refusal(too_large(route));
                }
                (c, canonical)
            }
        }
    };
    let intake = command.intake();
    // REQ-CS-9 (amendment 0013): a principal's answering setting may have its transfers and
    // reservations answered with their decision. Read only for those two.
    let answering_started = Instant::now();
    let (by_setting, wait) = if answering::covers(&command) {
        let setting = answering::of(shared, &who.principal).await;
        (
            setting.as_ref().is_some_and(|a| a.decided),
            answering::wait(shared, setting.as_ref()),
        )
    } else {
        (false, shared.config.decision_wait)
    };
    let answering_us = micros_since(answering_started);
    let synchronous = intake == Intake::Decide || by_setting;
    let row = NewRow {
        op_ref: mint(),
        kind: InputKind::Command,
        caller: Some(RowCaller {
            principal: who.principal.clone(),
            credential: who.credential.clone(),
            on_behalf_of: obo.map(|o| o.to_did()),
        }),
        surface: command.surface(),
        client_key: Some(client_key),
        key_domain: Some(domain),
        key_derived: derived,
        body_digest: digest.0,
        body: canonical,
        pass_through,
        reserves: intake == Intake::Reserve,
        synchronous,
        received_at: shared.clock.now(),
        arrived_at: Some(arrived_at),
    };
    // Commit before any reply names the row (REQ-AR-7). A store that does not answer holds the
    // request open: nothing may be acknowledged that is not logged.
    let append_started = Instant::now();
    let appended = loop {
        match shared.store.append(row.clone()).await {
            Ok(a) => break a,
            Err(e) => {
                tracing::warn!(error = %e, "the store refused an append; retrying");
                tokio::time::sleep(shared.config.poll).await;
            }
        }
    };
    // NFR-1 (amendments 0007, 0008): a command that moves value is durably taken in; that is its
    // acknowledgement. A command answered with its decision is measured at its reply (NFR-16).
    if !synchronous {
        let append = micros_since(append_started);
        let us = u64::try_from(arrived.elapsed().as_micros()).unwrap_or(u64::MAX);
        shared.metrics.acknowledgement(us);
        shared.metrics.ack_parts(crate::metrics::AckParts {
            auth: auth_us,
            party: party_us,
            answering: answering_us,
            append,
        });
    }
    let reply = match appended {
        Appended::New(row) => {
            Metrics::inc(&shared.metrics.commands);
            shared.wake.notify_one();
            // An API key is shown on the reply that decides it, and never again (`F-028`).
            let key = issued.map(|s| {
                let op = OperationId::from_ref(OperationRef(row.new.op_ref.0));
                s.key_for(&ledger_core::id::CredentialId::mint(&op, 0))
                    .expose()
                    .to_owned()
            });
            let op_ref = row.new.op_ref;
            let reply = answer(shared, &who.principal, row, key, wait).await;
            // The row's `replied_at` (`T-009-25`): this reply, the first to its command. A
            // repeat's reply is not stamped: it would measure the retry, not the command.
            shared.stamp_reply(op_ref);
            reply
        }
        Appended::Existing(row) => {
            Metrics::inc(&shared.metrics.repeats);
            if row.new.body_digest != digest.0 {
                return refusal(Refusal::ClientKeyReused);
            }
            answer(shared, &who.principal, row, None, wait).await
        }
    };
    // NFR-16 (amendment 0008): from arrival to the reply that carries the decision, or
    // `ERR-DecisionUnavailable`.
    if synchronous {
        let us = u64::try_from(arrived.elapsed().as_micros()).unwrap_or(u64::MAX);
        shared.metrics.decided(us);
    }
    reply
}

/// Ask the core, retrying while it does not answer, for at most `NFR-16`; then the answer is
/// `ERR-DecisionUnavailable` (a 503 with `Retry-After`), never a request held open: an instance with
/// no stream to a core yet, or a core restarting, answers at once and the caller retries
/// (`DEC-27` (e′), (g), `T-038-21`).
async fn ask<S: Store, C: Core>(
    shared: &Shared<S, C>,
    who: &Authenticated,
    query: &Query,
) -> QueryReply {
    let caller = who.caller(None);
    let mut wait = shared.config.retry_base;
    let until = tokio::time::Instant::now() + crate::answering::NFR_16;
    loop {
        match shared.core.query(&caller, query, shared.clock.now()).await {
            Ok(r) => return r,
            Err(CoreError::Unavailable | CoreError::Ambiguous) => {
                if tokio::time::Instant::now() + wait > until {
                    return QueryReply::Refused {
                        error: Refusal::DecisionUnavailable,
                    };
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(shared.config.retry_max);
            }
        }
    }
}

async fn handle_operation<S: Store, C: Core>(
    State(shared): State<Arc<Shared<S, C>>>,
    Path(reference): Path<String>,
    headers: HeaderMap,
) -> Response {
    let who = match credentials::authenticate(&shared, &headers).await {
        Ok(a) => a,
        Err(r) => return logged(refusal(r), None, "operation"),
    };
    let reply = operation_reply(&shared, &who, &reference).await;
    logged(reply, Some(&who), "operation")
}

/// An operation's reply with its pass-through data, as far as `who` may see it (`REQ-PO-5`);
/// `ERR-Internal` if the store did not answer (`REQ-ER-2`).
async fn with_reference<S: Store, C: Core>(
    shared: &Shared<S, C>,
    who: &Authenticated,
    mut reply: OperationReply,
) -> Response {
    match Echo::new(shared, &who.principal)
        .of_operation(&reply.operation_reference)
        .await
    {
        Ok(reference) => {
            reply.reference = reference;
            (StatusCode::OK, axum::Json(reply)).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "the store did not answer an echo");
            refusal(Refusal::Internal)
        }
    }
}

/// Movements with their operations' pass-through data, as far as `who` may see each
/// (`REQ-PO-5`, `AC-M8.4`). `Err` if the store did not answer.
async fn movements_with_references<S: Store, C: Core>(
    shared: &Shared<S, C>,
    who: &Authenticated,
    movements: &mut [MovementBody],
) -> Result<(), ()> {
    let found = Echo::new(shared, &who.principal)
        .of_operations(movements.iter().map(|m| m.operation.as_str()))
        .await
        .map_err(|e| tracing::error!(error = %e, "the store did not answer an echo"))?;
    for m in movements {
        m.reference = key_of(&m.operation).and_then(|k| found.get(&k).cloned());
    }
    Ok(())
}

async fn operation_reply<S: Store, C: Core>(
    shared: &Shared<S, C>,
    who: &Authenticated,
    reference: &str,
) -> Response {
    // A reference that does not parse is a shape fault (amendment 0032, `REQ-ER-3`); a
    // well-formed one the caller does not reach is `ERR-UnknownOperation` below.
    let Ok(op) = OperationId::parse(reference) else {
        return refusal(Refusal::MalformedCommand);
    };
    // The received-log first: a command the core has not decided is `Received`, whether or not
    // the core is up (REQ-CS-7), to the principal that sent it, and to a principal that reaches
    // an account it pays from (`T-032-14`, `NFR-4`, stress finding F16): the holder an operator
    // acted for, or the operator reading a holder's command. They would see the operation once
    // decided (`REQ-QR-*`, `INV-16`); before `T-032-14` they were told `ERR-UnknownOperation`.
    let logged = shared
        .store
        .get(InputKey(*op.as_bytes()))
        .await
        .ok()
        .flatten()
        .filter(|r| r.new.kind == InputKind::Command);
    let sent_by_caller = |r: &Row| {
        r.new
            .caller
            .as_ref()
            .is_some_and(|c| c.principal == who.principal)
    };
    if let Some(r) = &logged
        && r.status != Status::Decided
        && (sent_by_caller(r) || reaches_a_payer(shared, who, r).await)
    {
        return with_reference(shared, who, OperationReply::received(op.to_did())).await;
    }
    let row = logged.filter(sent_by_caller);
    let query = Query::Operation {
        reference: op.to_did(),
    };
    match ask(shared, who, &query).await {
        QueryReply::Refused {
            error: Refusal::UnknownOperation,
        } => {
            // A command acknowledged at `Received` that the core refused before it could open
            // an operation (its credential, for one): show the refusal the core returned, so a
            // caller told `Received` is never left without the outcome (REQ-CS-6).
            let refused = row
                .as_ref()
                .and_then(|r| r.core_reply.as_deref())
                .and_then(|b| serde_json::from_slice::<CoreReply>(b).ok())
                .and_then(|r| match r.outcome {
                    Outcome::Refused { error, .. } => Some(error),
                    _ => None,
                });
            match refused {
                Some(error) => {
                    with_reference(shared, who, OperationReply::rejected(op.to_did(), error)).await
                }
                None => refusal(Refusal::UnknownOperation),
            }
        }
        QueryReply::Operation(o) => with_reference(shared, who, o).await,
        other => query_response(other),
    }
}

/// The accounts a logged command pays from: a transfer's sources, a reservation's or a
/// withdrawal's payer. Nothing for any other command (`T-032-14`).
fn payers(row: &Row) -> Vec<ledger_core::id::AccountId> {
    let Ok(body) = serde_json::from_slice::<CommandBody>(&row.new.body) else {
        return Vec::new();
    };
    match Command::parse(&body) {
        Ok(Command::Transfer(t)) => t.legs.iter().map(|l| l.from).collect(),
        Ok(Command::Reserve { payer, .. }) => vec![payer],
        Ok(Command::Withdraw(w)) => vec![w.payer],
        _ => Vec::new(),
    }
}

/// Whether `who` reaches an account the logged command pays from: the core's own answer to a
/// balance read of that account as `who` (`REQ-PV-1`, `INV-16`), so ingress decides no scope
/// itself (`T-032-14`).
async fn reaches_a_payer<S: Store, C: Core>(
    shared: &Shared<S, C>,
    who: &Authenticated,
    row: &Row,
) -> bool {
    for payer in payers(row) {
        let query = Query::Balances(crate::query::BalancesParams {
            account: Some(payer.to_did()),
            identity: None,
            asset: None,
        });
        if !matches!(ask(shared, who, &query).await, QueryReply::Refused { .. }) {
            return true;
        }
    }
    false
}

/// A query's answer as the surface sends it.
fn query_response(reply: QueryReply) -> Response {
    let ok = |v: serde_json::Value| (StatusCode::OK, axum::Json(v)).into_response();
    let json = |v: Result<serde_json::Value, serde_json::Error>| match v {
        Ok(v) => ok(v),
        Err(_) => refusal(Refusal::NotAuthorised),
    };
    match reply {
        QueryReply::Balances(b) => json(serde_json::to_value(b)),
        QueryReply::Page(p) => json(serde_json::to_value(p)),
        QueryReply::Statement(s) => json(serde_json::to_value(s)),
        QueryReply::Operation(o) => json(serde_json::to_value(o)),
        QueryReply::Assets { items } => json(serde_json::to_value(items)),
        QueryReply::TrialBalance(t) => json(serde_json::to_value(t)),
        QueryReply::CanSpend(c) => json(serde_json::to_value(c)),
        QueryReply::Undelivered { items } => json(serde_json::to_value(items)),
        QueryReply::Refused { error } => refusal(error),
        QueryReply::RecoveryMechanisms(m) => json(serde_json::to_value(m)),
        QueryReply::Custody(c) => json(serde_json::to_value(c)),
        QueryReply::Instructions { items } => json(serde_json::to_value(items)),
        QueryReply::Calls { items } => json(serde_json::to_value(items)),
        QueryReply::Evidence { items } => json(serde_json::to_value(items)),
        QueryReply::Anchor(a) => json(serde_json::to_value(a)),
        QueryReply::Verified(v) => json(serde_json::to_value(v)),
        QueryReply::Anchoring(a) => json(serde_json::to_value(a)),
        QueryReply::Signals(s) => json(serde_json::to_value(s)),
        QueryReply::IntegratingParties(p) => json(serde_json::to_value(p)),
        QueryReply::Alias(a) => json(serde_json::to_value(a)),
        QueryReply::Grants(g) => json(serde_json::to_value(g)),
        QueryReply::Account(a) => json(serde_json::to_value(a)),
        QueryReply::IdentityGrants(g) => json(serde_json::to_value(g)),
    }
}

/// A query route other than `operation(reference)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryRoute {
    /// `GET /v1/balances`.
    Balances,
    /// `GET /v1/movements`.
    Movements,
    /// `GET /v1/statements`.
    Statement,
    /// `GET /v1/assets`.
    Assets,
    /// `GET /v1/trial-balance`.
    TrialBalance,
    /// `GET /v1/accounts/{account}/can-spend`.
    CanSpend,
    /// `GET /v1/notifications/undelivered`.
    Undelivered,
    /// `GET /v1/identities/{identity}/recovery-terms` (`F-012`).
    RecoveryTerms,
    /// `GET /v1/accounts/{account}/recovery-mechanisms` (`F-012`).
    RecoveryMechanisms,
    /// `GET /v1/accounts/{account}/custody` (`F-012`).
    Custody,
    /// `GET /v1/settlement/instructions` (`F-016`).
    Instructions,
    /// `GET /v1/settlement/calls` (`F-027`).
    Calls,
    /// `GET /v1/settlement/evidence` (`REQ-RC-5`).
    Evidence,
    /// `GET /v1/settlement/anchor` (`F-017`).
    Anchor,
    /// `GET /v1/disclosures/verify` (`F-017`).
    VerifyDisclosure,
    /// `GET /v1/ops/anchors` (`F-017`).
    Anchoring,
    /// `GET /v1/ops/signals` (`F-018`).
    Signals,
    /// `GET /v1/ops/integrating-parties` (`REQ-ID-7`).
    IntegratingParties,
    /// `GET /v1/aliases?source=…&name=…` (`REQ-RC-7`).
    Alias,
    /// `GET /v1/accounts/{account}/grants` (`REQ-RC-7`).
    Grants,
    /// `GET /v1/accounts/{account}` (amendment 0028).
    Account,
    /// `GET /v1/identities/{identity}/grants` (amendment 0026).
    IdentityGrants,
}

/// `alias(source, name)`'s parameters (`REQ-RC-7`).
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AliasParams {
    /// The source, as `register_alias` takes it.
    pub source: Option<String>,
    /// The name within it.
    pub name: Option<String>,
}

/// A read's query string, parsed. A parameter the route does not know, a repeated one and a value
/// that does not parse into its type are shape faults, `ERR-MalformedCommand` (amendment 0032,
/// `REQ-ER-3`), on every route. A parameter that is absent is not: it is `Option` in every
/// parameter struct, and the bounds (`ERR-QueryUnbounded`, `REQ-CS-5`) decide it.
fn params<T: DeserializeOwned + Default>(raw: Option<&str>) -> Result<T, Refusal> {
    match raw {
        None | Some("") => Ok(T::default()),
        Some(q) => serde_urlencoded::from_str(q).map_err(|_| Refusal::MalformedCommand),
    }
}

/// A read's request, from its path parameter and query string. Every shape fault is
/// `ERR-MalformedCommand` and is found here, before the core is asked (`crate::query::shape`).
fn query_of(route: QueryRoute, param: Option<String>, raw: Option<&str>) -> Result<Query, Refusal> {
    let query = query_unchecked(route, param, raw)?;
    crate::query::shape(&query)?;
    Ok(query)
}

fn query_unchecked(
    route: QueryRoute,
    param: Option<String>,
    raw: Option<&str>,
) -> Result<Query, Refusal> {
    Ok(match route {
        QueryRoute::Balances => Query::Balances(params(raw)?),
        QueryRoute::Movements => Query::Movements(params(raw)?),
        // A statement with no query string, or without an `account` or `asset`, is unbounded:
        // `statement_request` decides it, with the rest of the bounds.
        QueryRoute::Statement => Query::Statement(params::<StatementParams>(raw)?),
        QueryRoute::Assets => {
            let _: AssetParams = params(raw)?;
            Query::Assets
        }
        QueryRoute::TrialBalance => Query::TrialBalance(params(raw)?),
        QueryRoute::CanSpend => Query::CanSpend {
            account: param.ok_or(Refusal::UnknownAccount)?,
            params: params(raw)?,
        },
        QueryRoute::Undelivered => {
            let _: AssetParams = params(raw)?;
            Query::Undelivered
        }
        QueryRoute::RecoveryTerms => {
            let _: AssetParams = params(raw)?;
            Query::RecoveryTerms {
                identity: param.ok_or(Refusal::NotAuthorised)?,
            }
        }
        QueryRoute::RecoveryMechanisms => {
            let _: AssetParams = params(raw)?;
            Query::RecoveryMechanisms {
                account: param.ok_or(Refusal::UnknownAccount)?,
            }
        }
        QueryRoute::Custody => {
            let _: AssetParams = params(raw)?;
            Query::Custody {
                account: param.ok_or(Refusal::UnknownAccount)?,
            }
        }
        QueryRoute::Instructions => {
            let _: AssetParams = params(raw)?;
            Query::Instructions
        }
        QueryRoute::Calls => {
            let _: AssetParams = params(raw)?;
            Query::Calls
        }
        QueryRoute::Evidence => {
            let _: AssetParams = params(raw)?;
            Query::Evidence
        }
        QueryRoute::Anchor => {
            let _: AssetParams = params(raw)?;
            Query::Anchor
        }
        QueryRoute::Anchoring => {
            let _: AssetParams = params(raw)?;
            Query::Anchoring
        }
        QueryRoute::VerifyDisclosure => {
            let p: crate::anchoring::VerifyParams = params(raw)?;
            Query::VerifyDisclosure {
                disclosure: p.disclosure.ok_or(Refusal::DisclosureUnsupported)?,
            }
        }
        QueryRoute::Signals => {
            let _: AssetParams = params(raw)?;
            Query::Signals
        }
        QueryRoute::IntegratingParties => {
            let _: AssetParams = params(raw)?;
            Query::IntegratingParties
        }
        QueryRoute::Alias => {
            let p: AliasParams = params(raw)?;
            // A source that cannot be one is a shape fault, found before a missing name is
            // (amendment 0032).
            p.source
                .as_deref()
                .map(crate::operator::alias_source)
                .transpose()?;
            Query::Alias {
                source: p.source.ok_or(Refusal::QueryUnbounded)?,
                name: p.name.ok_or(Refusal::QueryUnbounded)?,
            }
        }
        QueryRoute::Grants => {
            let _: AssetParams = params(raw)?;
            Query::Grants {
                account: param.ok_or(Refusal::UnknownAccount)?,
            }
        }
        QueryRoute::Account => {
            let _: AssetParams = params(raw)?;
            Query::Account {
                account: param.ok_or(Refusal::UnknownAccount)?,
            }
        }
        QueryRoute::IdentityGrants => {
            let _: AssetParams = params(raw)?;
            Query::IdentityGrants {
                identity: param.ok_or(Refusal::NotAuthorised)?,
            }
        }
    })
}

async fn handle_query<S: Store, C: Core>(
    shared: Arc<Shared<S, C>>,
    route: QueryRoute,
    name: &'static str,
    param: Option<String>,
    headers: HeaderMap,
    raw: Option<String>,
) -> Response {
    let who = match credentials::authenticate(&shared, &headers).await {
        Ok(a) => a,
        Err(r) => return logged(refusal(r), None, name),
    };
    // REQ-RC-7: an archived identifier in the path or the query string (`alias(source, name)`).
    if let Err(r) = shared
        .config
        .archived
        .check(param.as_deref(), raw.as_deref(), b"")
    {
        return logged(refusal(r), Some(&who), name);
    }
    let reply = match query_of(route, param, raw.as_deref()) {
        Ok(q) => {
            let mut answer = ask(&shared, &who, &q).await;
            // REQ-PO-5: each movement with its operation's pass-through data, for this reader.
            let echoed = match &mut answer {
                QueryReply::Page(p) => {
                    movements_with_references(&shared, &who, &mut p.movements).await
                }
                QueryReply::Statement(s) => {
                    movements_with_references(&shared, &who, &mut s.movements).await
                }
                _ => Ok(()),
            };
            if echoed.is_err() {
                return logged(refusal(Refusal::Internal), Some(&who), name);
            }
            // The signals: the core's, with ingress's own oldest undecided row (`F-018`).
            if let QueryReply::Signals(s) = &mut answer {
                s.oldest_undecided_age = shared
                    .store
                    .oldest_undecided()
                    .await
                    .ok()
                    .flatten()
                    .map(|t| s.now.saturating_sub(t));
                // Completion (`NFR-15`): ingress's own measurement.
                let c = shared.metrics.completions();
                if c.samples > 0 {
                    s.completion_p50 = Some(c.p50_us);
                    s.completion_p99 = Some(c.p99_us);
                }
            }
            query_response(answer)
        }
        Err(r) => refusal(r),
    };
    logged(reply, Some(&who), name)
}

async fn handle_metrics<S: Store, C: Core>(
    State(shared): State<Arc<Shared<S, C>>>,
    headers: HeaderMap,
) -> Response {
    let who = match credentials::authenticate(&shared, &headers).await {
        Ok(a) => a,
        Err(r) => return logged(refusal(r), None, "metrics"),
    };
    let now = shared.clock.now();
    let oldest = shared
        .store
        .oldest_undecided()
        .await
        .ok()
        .flatten()
        .map(|t| now.saturating_sub(t));
    let tick_age = shared
        .core
        .last_tick()
        .await
        .ok()
        .flatten()
        .map(|t| now.saturating_sub(t));
    let text = shared.metrics.render(oldest, tick_age);
    let resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        text,
    )
        .into_response();
    logged(resp, Some(&who), "metrics")
}

async fn handle_openapi<S: Store, C: Core>(
    State(shared): State<Arc<Shared<S, C>>>,
    headers: HeaderMap,
) -> Response {
    let who = match credentials::authenticate(&shared, &headers).await {
        Ok(a) => a,
        Err(r) => return logged(refusal(r), None, "openapi"),
    };
    logged(
        (StatusCode::OK, axum::Json(crate::openapi::document())).into_response(),
        Some(&who),
        "openapi",
    )
}

async fn handle_auth<S: Store, C: Core>(
    shared: Arc<Shared<S, C>>,
    route: AuthRoute,
    name: &'static str,
    body: Body,
) -> Response {
    let Ok(bytes) = axum::body::to_bytes(body, MAX_COMMAND_BYTES).await else {
        return logged(refusal(Refusal::CredentialInvalid), None, name);
    };
    match route {
        AuthRoute::Challenge => match credentials::challenge(&shared, &bytes) {
            Ok(c) => logged((StatusCode::OK, axum::Json(c)).into_response(), None, name),
            Err(r) => logged(refusal(r), None, name),
        },
        AuthRoute::Passkey => match credentials::sign_in(&shared, &bytes).await {
            Ok((reply, who)) => logged(
                (StatusCode::OK, axum::Json(reply)).into_response(),
                Some(&who),
                name,
            ),
            Err(r) => logged(refusal(r), None, name),
        },
    }
}

async fn handle_answering<S: Store, C: Core>(
    shared: Arc<Shared<S, C>>,
    body: Option<Body>,
    name: &'static str,
    headers: HeaderMap,
) -> Response {
    let who = match credentials::authenticate(&shared, &headers).await {
        Ok(a) => a,
        Err(r) => return logged(refusal(r), None, name),
    };
    // T-036-25: `set_answering` is a command, so a chainspec book refuses it until it is open.
    if body.is_some() && !crate::service::takes_commands(&shared).await {
        return logged(refusal(Refusal::InitialStateNotSealed), Some(&who), name);
    }
    let reply = match body {
        None => crate::answering::read(&shared, &who).await,
        Some(body) => match axum::body::to_bytes(body, MAX_COMMAND_BYTES).await {
            Ok(bytes) => crate::answering::set(&shared, &who, &bytes).await,
            Err(_) => refusal(Refusal::NotAuthorised),
        },
    };
    logged(reply, Some(&who), name)
}

async fn handle_unknown<S: Store, C: Core>(
    State(shared): State<Arc<Shared<S, C>>>,
    headers: HeaderMap,
) -> Response {
    match credentials::authenticate(&shared, &headers).await {
        Err(r) => logged(refusal(r), None, "unknown"),
        Ok(who) => logged(refusal(Refusal::NotAuthorised), Some(&who), "unknown"),
    }
}

/// The surface of one replica.
pub fn router<S: Store, C: Core>(shared: Arc<Shared<S, C>>) -> Router {
    let mut r: Router<Arc<Shared<S, C>>> = Router::new();
    for route in ROUTES {
        let name = route.name;
        r = match route.handler {
            Handler::Command(c) if route.path.contains('{') => r.route(
                route.path,
                post(
                    move |State(s): State<Arc<Shared<S, C>>>,
                          Path(p): Path<String>,
                          headers: HeaderMap,
                          body: Body| {
                        handle_command(s, c, name, Some(p), headers, body)
                    },
                ),
            ),
            Handler::Command(c) => r.route(
                route.path,
                post(
                    move |State(s): State<Arc<Shared<S, C>>>, headers: HeaderMap, body: Body| {
                        handle_command(s, c, name, None, headers, body)
                    },
                ),
            ),
            Handler::Operation => r.route(route.path, get(handle_operation::<S, C>)),
            Handler::Query(q) if route.path.contains('{') => r.route(
                route.path,
                get(
                    move |State(s): State<Arc<Shared<S, C>>>,
                          Path(p): Path<String>,
                          RawQuery(raw): RawQuery,
                          headers: HeaderMap| {
                        handle_query(s, q, name, Some(p), headers, raw)
                    },
                ),
            ),
            Handler::Query(q) => r.route(
                route.path,
                get(
                    move |State(s): State<Arc<Shared<S, C>>>,
                          RawQuery(raw): RawQuery,
                          headers: HeaderMap| {
                        handle_query(s, q, name, None, headers, raw)
                    },
                ),
            ),
            Handler::Metrics => r.route(route.path, get(handle_metrics::<S, C>)),
            Handler::OpenApi => r.route(route.path, get(handle_openapi::<S, C>)),
            Handler::Auth(a) => r.route(
                route.path,
                post(move |State(s): State<Arc<Shared<S, C>>>, body: Body| {
                    handle_auth(s, a, name, body)
                }),
            ),
            Handler::Answering(AnsweringRoute::Read) => r.route(
                route.path,
                get(
                    move |State(s): State<Arc<Shared<S, C>>>, headers: HeaderMap| {
                        handle_answering(s, None, name, headers)
                    },
                ),
            ),
            Handler::Answering(AnsweringRoute::Set) => r.route(
                route.path,
                post(
                    move |State(s): State<Arc<Shared<S, C>>>, headers: HeaderMap, body: Body| {
                        handle_answering(s, Some(body), name, headers)
                    },
                ),
            ),
        };
    }
    r.fallback(handle_unknown::<S, C>)
        .method_not_allowed_fallback(handle_unknown::<S, C>)
        .layer(middleware::from_fn(access_log))
        .with_state(shared)
}
