// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's link to the core: through ingress, and nothing else (`F-016` plan; `DEC-5`).
//!
//! Every input the adapter sends is a command on ingress's settlement surface, logged in the
//! received-log like any other, under a **client key the adapter derives from what it reports**
//! (`settle/obs/<instance>/<seq>/<digest>`, `settle/ack/<operation>`, ...). A retry after a crash
//! or a timeout names the same row, and ingress answers with the reply it stored: the core never
//! sees the input twice (`REQ-OP-10`).
//!
//! The link reads the call outbox with a query (`GET /v1/settlement/calls`, `F-027`): every
//! external movement and anchor as an envelope.
//!
//! Two transports: [`HttpTransport`] (feature `http`) for a deployment, and [`RouterTransport`],
//! ingress's router in process, for tests and the conformance suite. The bearer token is held
//! only by the transport; it has no `Debug` and never reaches a log or an error.

use std::sync::Arc;

use ingress::settlement::{
    AcknowledgementBody, AdapterDeclarationBody, AnchorAcknowledgementBody, AnchorFailureBody,
    AnchorReadBody, AttestationBody, CallBody, CheckBody, GapBody, HeadBody, InstructionBody,
    ObservationBody, PositionsBody, SettlementResultBody, SigningBody,
};
use ingress::wire::{AdapterLegBody, CommandResult};
use serde::Serialize;
use serde_json::Value;

use crate::signer::BoxFuture;

/// Why the core could not be reached. The input may or may not have been logged: retry with the
/// same client key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkError(pub String);

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ingress did not answer: {}", self.0)
    }
}

/// A request to ingress and its answer: the HTTP status and the JSON body.
pub trait Transport: Send + Sync + 'static {
    /// `method path`, with `client_key` as the client-key header and `body` as JSON.
    fn call<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        client_key: Option<&'a str>,
        body: Option<Value>,
    ) -> BoxFuture<'a, Result<(u16, Value), LinkError>>;
}

/// Ingress's router, in process: for tests and the conformance suite.
#[derive(Clone)]
pub struct RouterTransport {
    router: axum::Router,
    token: Arc<str>,
}

impl RouterTransport {
    /// Calls `router` with `token`.
    pub fn new(router: axum::Router, token: &str) -> RouterTransport {
        RouterTransport {
            router,
            token: Arc::from(token),
        }
    }
}

impl Transport for RouterTransport {
    fn call<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        client_key: Option<&'a str>,
        body: Option<Value>,
    ) -> BoxFuture<'a, Result<(u16, Value), LinkError>> {
        Box::pin(async move {
            let headers: Vec<(&str, &str)> = client_key
                .map(|k| vec![("client-key", k)])
                .unwrap_or_default();
            let (status, v) = ingress::testkit::call(
                &self.router,
                method,
                path,
                Some(&self.token),
                &headers,
                body,
            )
            .await;
            Ok((status.as_u16(), v))
        })
    }
}

/// Ingress over HTTP (feature `http`).
#[cfg(feature = "http")]
pub struct HttpTransport {
    base: String,
    token: zeroize::Zeroizing<String>,
    client: reqwest::Client,
}

#[cfg(feature = "http")]
impl HttpTransport {
    /// Ingress at `base` (`https://…`), authenticating with the token in the file at
    /// `token_file`. Each request times out after `timeout`; the adapter then retries with the
    /// same client key.
    pub fn new(
        base: &str,
        token_file: &std::path::Path,
        timeout: std::time::Duration,
    ) -> Result<HttpTransport, LinkError> {
        let token = zeroize::Zeroizing::new(
            std::fs::read_to_string(token_file)
                .map_err(|e| LinkError(format!("the token file could not be read: {e}")))?
                .trim()
                .to_owned(),
        );
        HttpTransport::with_token(base, token, timeout)
    }

    /// Ingress at `base`, authenticating with `token`, read by the caller from wherever the
    /// configuration says it is (`ProcessConfig::ingress_token`).
    pub fn with_token(
        base: &str,
        token: zeroize::Zeroizing<String>,
        timeout: std::time::Duration,
    ) -> Result<HttpTransport, LinkError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| LinkError(e.to_string()))?;
        Ok(HttpTransport {
            base: base.trim_end_matches('/').to_owned(),
            token,
            client,
        })
    }
}

#[cfg(feature = "http")]
impl Transport for HttpTransport {
    fn call<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        client_key: Option<&'a str>,
        body: Option<Value>,
    ) -> BoxFuture<'a, Result<(u16, Value), LinkError>> {
        Box::pin(async move {
            let url = format!("{}{path}", self.base);
            let mut req = match method {
                "GET" => self.client.get(&url),
                _ => self.client.post(&url),
            };
            req = req.bearer_auth(self.token.as_str());
            if let Some(k) = client_key {
                req = req.header("client-key", k);
            }
            if let Some(b) = body {
                req = req.json(&b);
            }
            // The error text of a failed request names the URL and the cause, never a header.
            let resp = req
                .send()
                .await
                .map_err(|e| LinkError(e.without_url().to_string()))?;
            let status = resp.status().as_u16();
            let v = resp.json::<Value>().await.unwrap_or(Value::Null);
            Ok((status, v))
        })
    }
}

/// What the core answered to a settlement command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Decided: the operation's state, if any, and what the command produced.
    Decided {
        /// The state the core derived.
        state: Option<String>,
        /// What it produced.
        result: Box<SettlementResultBody>,
    },
    /// Refused, with the condition's code (`ERR-*`).
    Refused(String),
}

impl Answer {
    /// The verdict the adapter stores: the core's verdict, `already_applied` and
    /// `not_associated` for the two refusals that are verdicts (`REQ-SI-4`), `refused:<code>`
    /// for any other refusal, `decided` for a command with no verdict.
    pub fn verdict(&self) -> String {
        match self {
            Answer::Decided { result, .. } => match result.verdict {
                Some(v) => serde_json::to_value(v)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "decided".into()),
                None => "decided".into(),
            },
            Answer::Refused(code) if code == "ERR-ObservationDuplicate" => "already_applied".into(),
            Answer::Refused(code) if code == "ERR-ObservationUnassociated" => {
                "not_associated".into()
            }
            Answer::Refused(code) => format!("refused:{code}"),
        }
    }

    /// Whether the core took the observation into its store: every verdict, and
    /// `ERR-EvidenceInsufficient` (an unusable record is still ingested, `F-010` decision 3).
    pub fn ingested(&self) -> bool {
        match self {
            Answer::Decided { .. } => true,
            Answer::Refused(code) => matches!(
                code.as_str(),
                "ERR-ObservationDuplicate"
                    | "ERR-ObservationUnassociated"
                    | "ERR-EvidenceInsufficient"
            ),
        }
    }
}

/// The link. See the module documentation.
pub struct CoreLink<T: Transport> {
    transport: T,
}

fn answer(status: u16, v: Value) -> Result<Answer, LinkError> {
    if (200..300).contains(&status) {
        let state = v["state"].as_str().map(str::to_owned);
        let result: Option<CommandResult> = serde_json::from_value(v["result"].clone()).ok();
        return Ok(Answer::Decided {
            state,
            result: result.and_then(|r| r.settlement).unwrap_or_default(),
        });
    }
    match v["error"].as_str() {
        Some(code) if (400..500).contains(&status) => Ok(Answer::Refused(code.to_owned())),
        _ => Err(LinkError(format!("status {status}"))),
    }
}

fn json<T: Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

impl<T: Transport> CoreLink<T> {
    /// A link over `transport`.
    pub fn new(transport: T) -> CoreLink<T> {
        CoreLink { transport }
    }

    async fn post(&self, path: &str, key: &str, body: Value) -> Result<Answer, LinkError> {
        let (status, v) = self
            .transport
            .call("POST", path, Some(key), Some(body))
            .await?;
        answer(status, v)
    }

    /// The instructions the adapter has not acknowledged (`REQ-SO-7`).
    pub async fn instructions(&self) -> Result<Result<Vec<InstructionBody>, String>, LinkError> {
        let (status, v) = self
            .transport
            .call("GET", "/v1/settlement/instructions", None, None)
            .await?;
        if status == 200 {
            return serde_json::from_value(v)
                .map(Ok)
                .map_err(|e| LinkError(format!("the outbox did not parse: {e}")));
        }
        match answer(status, v)? {
            Answer::Refused(code) => Ok(Err(code)),
            Answer::Decided { .. } => Ok(Err("unexpected".into())),
        }
    }

    /// The call outbox (`F-027` T-027-01): every call the executor has not taken over, as
    /// envelopes.
    pub async fn calls(&self) -> Result<Result<Vec<CallBody>, String>, LinkError> {
        let (status, v) = self
            .transport
            .call("GET", "/v1/settlement/calls", None, None)
            .await?;
        if status == 200 {
            return serde_json::from_value(v)
                .map(Ok)
                .map_err(|e| LinkError(format!("the call outbox did not parse: {e}")));
        }
        match answer(status, v)? {
            Answer::Refused(code) => Ok(Err(code)),
            Answer::Decided { .. } => Ok(Err("unexpected".into())),
        }
    }

    /// `declare_adapter`.
    pub async fn declare(
        &self,
        key: &str,
        serving: bool,
        lease: u64,
        support: Option<Vec<AdapterLegBody>>,
    ) -> Result<Answer, LinkError> {
        let body = AdapterDeclarationBody {
            serving,
            lease,
            support,
        };
        self.post("/v1/settlement/declarations", key, json(&body))
            .await
    }

    /// What the layer's metadata states about an asset location the adapter binds (`DEC-20`).
    pub async fn attest(&self, key: &str, body: &AttestationBody) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/attestations", key, json(body))
            .await
    }

    /// `ack_instruction`.
    pub async fn acknowledge(
        &self,
        key: &str,
        body: &AcknowledgementBody,
    ) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/acknowledgements", key, json(body))
            .await
    }

    /// `ack_instruction`: the one transaction signed for an instruction, before it is submitted,
    /// or that none ever will be (the owner's ruling of 2026-09-24).
    pub async fn sign(&self, key: &str, body: &SigningBody) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/signings", key, json(body)).await
    }

    /// `record_observation`: a record.
    pub async fn observe(&self, key: &str, body: &ObservationBody) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/observations", key, json(body))
            .await
    }

    /// `record_observation`: a canonical check.
    pub async fn check(&self, key: &str, body: &CheckBody) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/checks", key, json(body)).await
    }

    /// `record_observation`: the finalised head.
    pub async fn head(&self, key: &str, body: &HeadBody) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/heads", key, json(body)).await
    }

    /// `record_observation`: a gap.
    pub async fn gap(&self, key: &str, body: &GapBody) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/gaps", key, json(body)).await
    }

    /// The checkpoint in flight and whether a comparison is due (`F-017`).
    pub async fn anchor(&self) -> Result<Result<AnchorReadBody, String>, LinkError> {
        let (status, v) = self
            .transport
            .call("GET", "/v1/settlement/anchor", None, None)
            .await?;
        if status == 200 {
            return serde_json::from_value(v)
                .map(Ok)
                .map_err(|e| LinkError(format!("the anchor read did not parse: {e}")));
        }
        match answer(status, v)? {
            Answer::Refused(code) => Ok(Err(code)),
            Answer::Decided { .. } => Ok(Err("unexpected".into())),
        }
    }

    /// `ack_instruction` of an anchor (`F-017`).
    pub async fn acknowledge_anchor(
        &self,
        key: &str,
        body: &AnchorAcknowledgementBody,
    ) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/anchor-acknowledgements", key, json(body))
            .await
    }

    /// An anchor's extrinsic provably did not execute (`F-017`).
    pub async fn anchor_not_executed(
        &self,
        key: &str,
        body: &AnchorFailureBody,
    ) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/anchor-failures", key, json(body))
            .await
    }

    /// The layer's positions at a finalized block (`F-017`).
    pub async fn positions(&self, key: &str, body: &PositionsBody) -> Result<Answer, LinkError> {
        self.post("/v1/settlement/positions", key, json(body)).await
    }
}
