// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The OpenAPI 3.1 description of the surface (accepted proposal of `F-009`; `REQ-CS-4`,
//! `REQ-ER-1`).
//!
//! It is generated, never written by hand: the paths from [`crate::surface::ROUTES`] (the same
//! table the router is built from), the bodies from the wire types in [`crate::wire`], and the
//! refusal schema from SPEC §10's closed list as `ledger-core` carries it
//! ([`ledger_core::Refusal::ALL`]): one alternative per condition, its code and its published
//! description as constants, and the HTTP status the surface sends it with
//! (`x-http-status`, [`crate::surface::status_of`]). The contract test checks every refusal the
//! surface can send against this document, and the build fails if they differ.
//!
//! The document is committed as `services/ingress/openapi.json`; a test regenerates it and fails
//! if the committed file is stale (`UPDATE_OPENAPI=1` rewrites it).

use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::{Map, Value, json};

use ledger_core::Refusal;

use crate::query::{
    AssetBody, AssetParams, BalancesBody, BalancesParams, CanSpendBody, MovementsParams, PageBody,
    StatementBody, StatementParams, TrialBalanceBody,
};
use crate::refusal::IngressRefusal;
use crate::surface::{
    AnsweringRoute, AuthRoute, CLIENT_KEY, CommandRoute, Handler, ON_BEHALF_OF, QueryRoute, ROUTES,
    status_of,
};

use crate::wire::{
    AdapterSupportBody, CaptureBody, CommandReply, DeprecateAssetBody, EmptyBody, GrantBody,
    GrantIssuanceBody, GrantOperatorBody, IssueBody, JustifiedBody, NotificationBody,
    OpenAccountBody, OpenIdentityBody, OperationReply, RedeemBody, RegisterAssetBody, ReserveBody,
    ResolveBody, SubmitEvidenceBody, TransferBody, UndeliveredBody, WithdrawBody,
};
use crate::wire::{
    CompleteRecoveryBody, CustodyViewBody, InitiateRecoveryBody, NameSuccessorBody,
    ReclaimCustodyBody, RecoveryMechanismsBody, RegisterCredentialBody,
};

/// A type's schema inline, for the parameters of a query string.
fn inline<T: JsonSchema>() -> Value {
    serde_json::to_value(
        SchemaSettings::draft2020_12()
            .into_generator()
            .into_root_schema_for::<T>(),
    )
    .unwrap_or_default()
}

/// What `trial_balance` does and which parameters it takes. `amount` is `can_spend`'s: the two
/// routes share `AssetParams`, so `trial_balance` accepts it and ignores it.
const TRIAL_BALANCE_DESCRIPTION: &str = "Checks that the book adds up, from one view. It is the \
    operator's: any other caller is refused as `ERR-NotAuthorised`. `asset` narrows the check to \
    one asset. The route also accepts `amount`, which belongs to `can_spend`, and ignores it. Any \
    other parameter, and an `asset` that is not a valid symbol, is refused as \
    `ERR-MalformedCommand`.";

/// What `can_spend` does and which parameters it takes.
const CAN_SPEND_DESCRIPTION: &str = "Says whether a spend of `amount` minor units of `asset` from \
    the account would pass the checks before the kernel, and names the reason if it would not. \
    Both parameters are required: a missing one is refused as `ERR-QueryUnbounded`. Any other \
    parameter, an `asset` that is not a valid symbol and an `amount` that is not a count of \
    minor units are refused as `ERR-MalformedCommand`.";

/// The OpenAPI version the document declares.
pub const OPENAPI_VERSION: &str = "3.1.0";

fn reference(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// The refusal schema: one alternative per condition of SPEC §10.
pub fn refusal_schema() -> Value {
    let alternatives: Vec<Value> = Refusal::ALL
        .iter()
        .map(|r| {
            json!({
                "title": r.code(),
                "type": "object",
                "additionalProperties": false,
                "required": ["error", "description"],
                "properties": {
                    "error": { "const": r.code() },
                    "description": { "const": r.description() },
                },
                "x-http-status": status_of(*r).as_u16(),
            })
        })
        .chain(IngressRefusal::ALL.iter().map(|r| {
            // Refusals only ingress sends until the next core carries them (amendment 0037, R2).
            json!({
                "title": r.code(),
                "type": "object",
                "additionalProperties": false,
                "required": ["error", "description"],
                "properties": {
                    "error": { "const": r.code() },
                    "description": { "const": r.description() },
                },
                "x-http-status": r.status().as_u16(),
            })
        }))
        .collect();
    json!({
        "title": "Refusal",
        "description": "A refusal names one condition of the closed list (REQ-CS-4, REQ-ER-1). It is \
                        always a 4xx, never a success body carrying an error.",
        "oneOf": alternatives,
    })
}

fn request_schema(route: CommandRoute) -> &'static str {
    match route {
        CommandRoute::Transfer => "TransferBody",
        CommandRoute::Reserve => "ReserveBody",
        CommandRoute::Capture => "CaptureBody",
        CommandRoute::Issue => "IssueBody",
        CommandRoute::Redeem => "RedeemBody",
        CommandRoute::Withdraw => "WithdrawBody",
        CommandRoute::OpenIdentity => "OpenIdentityBody",
        CommandRoute::OpenAccount => "OpenAccountBody",
        CommandRoute::GrantAuthority => "GrantBody",
        CommandRoute::Release | CommandRoute::CloseAccount | CommandRoute::RevokeAuthority => {
            "EmptyBody"
        }
        CommandRoute::FreezeAccount
        | CommandRoute::UnfreezeAccount
        | CommandRoute::Resume
        | CommandRoute::ProposeReversal
        | CommandRoute::ApproveReversal
        | CommandRoute::RevokeIssuance
        | CommandRoute::CancelUpgrade
        | CommandRoute::RevokeOperator => "JustifiedBody",
        CommandRoute::Resolve => "ResolveBody",
        CommandRoute::SubmitEvidence => "SubmitEvidenceBody",
        CommandRoute::RegisterAsset => "RegisterAssetBody",
        CommandRoute::DeprecateAsset => "DeprecateAssetBody",
        CommandRoute::BindAsset => "BindAssetBody",
        CommandRoute::GrantIssuance => "GrantIssuanceBody",
        CommandRoute::GrantOperator => "GrantOperatorBody",
        CommandRoute::DeclareAdapterSupport => "AdapterSupportBody",
        CommandRoute::RegisterCredential => "RegisterCredentialRequest",
        CommandRoute::RemoveCredential
        | CommandRoute::DelegateCustody
        | CommandRoute::CancelRecovery
        | CommandRoute::ClaimSuccession => "EmptyBody",
        CommandRoute::ReclaimCustody => "ReclaimCustodyBody",
        CommandRoute::NameSuccessor => "NameSuccessorBody",
        CommandRoute::InitiateRecovery => "InitiateRecoveryBody",
        CommandRoute::ImportCredentials => "ImportCredentialsBody",
        CommandRoute::CompleteRecovery => "CompleteRecoveryBody",
        CommandRoute::RegisterIntegratingParty => "RegisterIntegratingPartyBody",
        CommandRoute::TiePrincipal => "TiePrincipalBody",
        CommandRoute::RegisterAlias => "RegisterAliasBody",
        CommandRoute::Upgrade => "UpgradeBody",
        CommandRoute::RescheduleUpgrade => "RescheduleUpgradeBody",
        CommandRoute::Settlement(s) => s.schema(),
        CommandRoute::Anchoring(a) => a.schema(),
    }
}

fn refusals() -> Value {
    json!({
        "description": "Refused, with the named condition. The status is the alternative's x-http-status.",
        "content": { "application/json": { "schema": reference("Refusal") } }
    })
}

fn path_parameters(path: &str) -> Vec<Value> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .map(|name| {
            json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" },
                "description": "A did:aledgent identifier."
            })
        })
        .collect()
}

fn operation(route: &crate::surface::Route) -> Value {
    let mut op = Map::new();
    op.insert("operationId".into(), json!(route.name));
    op.insert("summary".into(), json!(route.summary));
    let mut params = path_parameters(route.path);
    let mut responses = Map::new();
    match route.handler {
        Handler::Command(c) => {
            params.push(json!({
                "name": CLIENT_KEY,
                "in": "header",
                "required": false,
                "schema": { "type": "string", "maxLength": 256 },
                "description": "The client key (REQ-OP-10). Without one, a key is derived from the \
                                request and returned (REQ-OP-11)."
            }));
            params.push(json!({
                "name": ON_BEHALF_OF,
                "in": "header",
                "required": false,
                "schema": { "type": "string" },
                "description": "The identity the caller acts for, where different (REQ-PN-1)."
            }));
            op.insert(
                "requestBody".into(),
                json!({
                    "required": !matches!(
                        c,
                        CommandRoute::Release
                            | CommandRoute::CloseAccount
                            | CommandRoute::RevokeAuthority
                            | CommandRoute::RemoveCredential
                            | CommandRoute::DelegateCustody
                            | CommandRoute::CancelRecovery
                            | CommandRoute::NameSuccessor
                            | CommandRoute::ClaimSuccession
                    ),
                    "content": { "application/json": { "schema": reference(request_schema(c)) } }
                }),
            );
            responses.insert(
                "200".into(),
                json!({
                    "description": "Decided. For a transfer or a reservation, the reply to a repeat once the core has decided (REQ-CS-3).",
                    "content": { "application/json": { "schema": reference("CommandReply") } }
                }),
            );
            if matches!(
                c,
                CommandRoute::Transfer | CommandRoute::Reserve | CommandRoute::Withdraw
            ) {
                responses.insert(
                    "202".into(),
                    json!({
                        "description": "Received: an acknowledgement, not an outcome (REQ-CS-2). \
                                        terminal is false.",
                        "content": { "application/json": { "schema": reference("CommandReply") } }
                    }),
                );
            }
        }
        Handler::Operation => {
            responses.insert(
                "200".into(),
                json!({
                    "description": "The operation's state, with its class (REQ-OP-7).",
                    "content": { "application/json": { "schema": reference("OperationReply") } }
                }),
            );
        }
        Handler::Query(q) => {
            let (params_schema, reply, array) = match q {
                QueryRoute::Balances => (Some(inline::<BalancesParams>()), "BalancesBody", false),
                QueryRoute::Movements => (Some(inline::<MovementsParams>()), "PageBody", false),
                QueryRoute::Statement => {
                    // `account` and `asset` are `Option` in the struct, so that a missing one is
                    // `ERR-QueryUnbounded` and not a shape fault (amendment 0032): the document
                    // still says they are required.
                    let mut schema = inline::<StatementParams>();
                    schema["required"] = json!(["account", "asset"]);
                    (Some(schema), "StatementBody", false)
                }
                QueryRoute::Assets => (None, "AssetBody", true),
                QueryRoute::TrialBalance => {
                    (Some(inline::<AssetParams>()), "TrialBalanceBody", false)
                }
                QueryRoute::CanSpend => (Some(inline::<AssetParams>()), "CanSpendBody", false),
                QueryRoute::Undelivered => (None, "UndeliveredBody", true),
                QueryRoute::RecoveryTerms | QueryRoute::RecoveryMechanisms => {
                    (None, "RecoveryMechanismsBody", false)
                }
                QueryRoute::Custody => (None, "CustodyViewBody", false),
                QueryRoute::Instructions => (None, "InstructionBody", true),
                QueryRoute::Calls => (None, "CallBody", true),
                QueryRoute::Evidence => (None, "PendingEvidenceBody", true),
                QueryRoute::Anchor => (None, "AnchorReadBody", false),
                QueryRoute::VerifyDisclosure => (
                    Some(inline::<crate::anchoring::VerifyParams>()),
                    "VerifiedBody",
                    false,
                ),
                QueryRoute::Anchoring => (None, "AnchoringBody", false),
                QueryRoute::Signals => (None, "SignalsBody", false),
                QueryRoute::IntegratingParties => (None, "IntegratingPartiesBody", false),
                QueryRoute::Alias => (
                    Some(inline::<crate::surface::AliasParams>()),
                    "AliasBody",
                    false,
                ),
                QueryRoute::Grants => (None, "GrantsBody", false),
                QueryRoute::Account => (None, "AccountViewBody", false),
                QueryRoute::IdentityGrants => (None, "IdentityGrantsBody", false),
            };
            if let Some(s) = params_schema {
                // For the routes with a struct of their own, the parameters' doc comment is the
                // operation's description, so the rules that span several parameters (exactly one
                // subject, the bounds) are written once. The two that share `AssetParams` say
                // their own.
                let described: Option<&str> = match q {
                    QueryRoute::Balances | QueryRoute::Movements | QueryRoute::Statement => {
                        s["description"].as_str()
                    }
                    QueryRoute::TrialBalance => Some(TRIAL_BALANCE_DESCRIPTION),
                    QueryRoute::CanSpend => Some(CAN_SPEND_DESCRIPTION),
                    _ => None,
                };
                if let Some(d) = described {
                    op.insert("description".into(), json!(d));
                }
                let required: Vec<Value> = s["required"].as_array().cloned().unwrap_or_default();
                if let Some(props) = s["properties"].as_object() {
                    for (name, schema) in props {
                        params.push(json!({
                            "name": name,
                            "in": "query",
                            "required": required.contains(&json!(name)),
                            "schema": schema,
                        }));
                    }
                }
            }
            let schema = if array {
                json!({ "type": "array", "items": reference(reply) })
            } else {
                reference(reply)
            };
            responses.insert(
                "200".into(),
                json!({
                    "description": "The answer, from one consistent view (REQ-QR-2).",
                    "content": { "application/json": { "schema": schema } }
                }),
            );
        }
        Handler::Metrics => {
            responses.insert(
                "200".into(),
                json!({
                    "description": "Prometheus text format.",
                    "content": { "text/plain": { "schema": { "type": "string" } } }
                }),
            );
        }
        Handler::OpenApi => {
            responses.insert(
                "200".into(),
                json!({
                    "description": "This document.",
                    "content": { "application/json": { "schema": { "type": "object" } } }
                }),
            );
        }
        Handler::Auth(a) => {
            let (request, reply, what) = match a {
                AuthRoute::Challenge => (
                    "ChallengeRequest",
                    "ChallengeReply",
                    "A challenge, valid until expires_at, for one ceremony; a register challenge that names an identity serves that identity's set only.",
                ),
                AuthRoute::Passkey => (
                    "PasskeySignInBody",
                    "SessionReply",
                    "A session token: present it as a bearer until expires_at. It is a secret.",
                ),
            };
            // No bearer: the body is what these routes check (`F-028`).
            op.insert("security".into(), json!([]));
            op.insert(
                "requestBody".into(),
                json!({
                    "required": true,
                    "content": { "application/json": { "schema": reference(request) } }
                }),
            );
            responses.insert(
                "200".into(),
                json!({
                    "description": what,
                    "content": { "application/json": { "schema": reference(reply) } }
                }),
            );
        }
        Handler::Answering(a) => {
            if a == AnsweringRoute::Set {
                op.insert(
                    "requestBody".into(),
                    json!({
                        "required": true,
                        "content": { "application/json": { "schema": reference("AnsweringBody") } }
                    }),
                );
            }
            responses.insert(
                "200".into(),
                json!({
                    "description": "The caller's answering setting as it now stands (REQ-CS-9).",
                    "content": { "application/json": { "schema": reference("AnsweringReply") } }
                }),
            );
        }
    }
    responses.insert("4XX".into(), refusals());
    // `ERR-DecisionUnavailable` (503) and `ERR-Internal` (500): the same body, a server status
    // (amendment 0006 item 8). The 503 carries `Retry-After` (`DEC-27` (g), `T-038-23`).
    let mut server = refusals();
    server["headers"] = json!({ "Retry-After": { "$ref": "#/components/headers/RetryAfter" } });
    responses.insert("5XX".into(), server);
    if !params.is_empty() {
        op.insert("parameters".into(), Value::Array(params));
    }
    op.insert("responses".into(), Value::Object(responses));
    Value::Object(op)
}

fn schemas() -> Map<String, Value> {
    let mut settings = SchemaSettings::draft2020_12();
    settings.definitions_path = "/components/schemas".into();
    let mut g = settings.into_generator();
    fn add<T: JsonSchema>(g: &mut schemars::SchemaGenerator) {
        let _ = g.subschema_for::<T>();
    }
    add::<TransferBody>(&mut g);
    add::<ReserveBody>(&mut g);
    add::<CaptureBody>(&mut g);
    add::<EmptyBody>(&mut g);
    add::<IssueBody>(&mut g);
    add::<RedeemBody>(&mut g);
    add::<WithdrawBody>(&mut g);
    add::<OpenIdentityBody>(&mut g);
    add::<OpenAccountBody>(&mut g);
    add::<GrantBody>(&mut g);
    add::<CommandReply>(&mut g);
    add::<OperationReply>(&mut g);
    add::<UndeliveredBody>(&mut g);
    add::<NotificationBody>(&mut g);
    add::<JustifiedBody>(&mut g);
    add::<ResolveBody>(&mut g);
    add::<SubmitEvidenceBody>(&mut g);
    add::<RegisterAssetBody>(&mut g);
    add::<DeprecateAssetBody>(&mut g);
    add::<crate::wire::BindAssetBody>(&mut g);
    add::<GrantIssuanceBody>(&mut g);
    add::<GrantOperatorBody>(&mut g);
    add::<AdapterSupportBody>(&mut g);
    add::<BalancesBody>(&mut g);
    add::<PageBody>(&mut g);
    add::<StatementBody>(&mut g);
    add::<AssetBody>(&mut g);
    add::<TrialBalanceBody>(&mut g);
    add::<crate::query::SignalsBody>(&mut g);
    add::<CanSpendBody>(&mut g);
    add::<RegisterCredentialBody>(&mut g);
    add::<crate::wire::RegisterCredentialRequest>(&mut g);
    add::<crate::wire::ChallengeRequest>(&mut g);
    add::<crate::wire::ChallengeReply>(&mut g);
    add::<crate::wire::PasskeySignInBody>(&mut g);
    add::<crate::wire::SessionReply>(&mut g);
    add::<ReclaimCustodyBody>(&mut g);
    add::<NameSuccessorBody>(&mut g);
    add::<InitiateRecoveryBody>(&mut g);
    add::<crate::wire::ImportCredentialsBody>(&mut g);
    add::<CompleteRecoveryBody>(&mut g);
    add::<crate::wire::RegisterIntegratingPartyBody>(&mut g);
    add::<crate::wire::TiePrincipalBody>(&mut g);
    add::<crate::wire::IntegratingPartiesBody>(&mut g);
    add::<crate::wire::RegisterAliasBody>(&mut g);
    add::<crate::wire::UpgradeBody>(&mut g);
    add::<crate::wire::RescheduleUpgradeBody>(&mut g);
    add::<crate::wire::AliasBody>(&mut g);
    add::<crate::wire::GrantsBody>(&mut g);
    add::<crate::wire::AccountViewBody>(&mut g);
    add::<crate::wire::IdentityGrantsBody>(&mut g);
    add::<RecoveryMechanismsBody>(&mut g);
    add::<CustodyViewBody>(&mut g);
    add::<crate::settlement::SubmitOutboundBody>(&mut g);
    add::<crate::settlement::ConfigureSettlementBody>(&mut g);
    add::<crate::settlement::AdapterDeclarationBody>(&mut g);
    add::<crate::settlement::AcknowledgementBody>(&mut g);
    add::<crate::settlement::SigningBody>(&mut g);
    add::<crate::settlement::ObservationBody>(&mut g);
    add::<crate::settlement::CheckBody>(&mut g);
    add::<crate::settlement::HeadBody>(&mut g);
    add::<crate::settlement::GapBody>(&mut g);
    add::<crate::settlement::InstructionBody>(&mut g);
    add::<crate::settlement::CallBody>(&mut g);
    add::<crate::settlement::PendingEvidenceBody>(&mut g);
    add::<crate::settlement::EvidenceCheckedBody>(&mut g);
    add::<crate::settlement::AnchorAcknowledgementBody>(&mut g);
    add::<crate::settlement::AnchorFailureBody>(&mut g);
    add::<crate::settlement::PositionsBody>(&mut g);
    add::<crate::settlement::AttestationBody>(&mut g);
    add::<crate::settlement::AnchorReadBody>(&mut g);
    add::<crate::anchoring::DisclosureBody>(&mut g);
    add::<crate::anchoring::AnchoringJustifiedBody>(&mut g);
    add::<crate::anchoring::VerifiedBody>(&mut g);
    add::<crate::anchoring::AnchoringBody>(&mut g);
    add::<crate::wire::AnsweringBody>(&mut g);
    add::<crate::wire::AnsweringReply>(&mut g);
    let mut defs = g.take_definitions(true);
    defs.insert("Refusal".into(), refusal_schema());
    defs
}

/// The document.
pub fn document() -> Value {
    let mut paths = Map::new();
    for route in ROUTES {
        let entry = paths
            .entry(route.path.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(m) = entry {
            m.insert(route.method.to_ascii_lowercase(), operation(route));
        }
    }
    json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": "aledgent-ingress",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "The command and query surfaces of aledgent (CTR-CMD, CTR-QRY) and its \
                            notifications (CTR-EVT). Amounts are decimal-integer strings of minor \
                            units. Identifiers are did:aledgent strings. Times are milliseconds \
                            since the Unix epoch. Every route requires a bearer credential, \
                            except the two under /v1/auth, which check what their body carries \
                            (F-028)."
        },
        "security": [ { "bearer": [] } ],
        "paths": paths,
        "webhooks": {
            "notification": {
                "post": {
                    "summary": "A notification, pushed at least once, in no particular order \
                                (REQ-EV-2). Its id is stable across attempts (REQ-EV-3). It is \
                                signed: aledgent-signature is v1=HMAC-SHA256 over the id, \
                                aledgent-attempt-time and the body (REQ-EV-4). It names the \
                                latest anchor final when its transition was recorded, or null \
                                when none was (REQ-EV-1, AC-M2.4).",
                    "parameters": [
                        { "name": "aledgent-notification-id", "in": "header", "required": true,
                          "schema": { "type": "string" } },
                        { "name": "aledgent-attempt-time", "in": "header", "required": true,
                          "schema": { "type": "string" } },
                        { "name": "aledgent-signature", "in": "header", "required": true,
                          "schema": { "type": "string" } }
                    ],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": reference("NotificationBody") } }
                    },
                    "responses": { "2XX": { "description": "Received. Anything else is retried." } }
                }
            }
        },
        "components": {
            "securitySchemes": { "bearer": { "type": "http", "scheme": "bearer" } },
            "headers": {
                "RetryAfter": {
                    "description": "On ERR-DecisionUnavailable (503): seconds to wait before \
                                    retrying with the same client key (DEC-27 (g)).",
                    "schema": { "type": "integer", "minimum": 0 }
                }
            },
            "schemas": schemas(),
        }
    })
}

/// The document as the committed file holds it.
pub fn document_text() -> String {
    let mut s = serde_json::to_string_pretty(&document()).unwrap_or_default();
    s.push('\n');
    s
}
