// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `T-040-10` and `T-041-06` (amendment 0037, R2; `REQ-CS-4`, `REQ-ER-1`, `US-A10`): the refusal
//! `ERR-CommandApprovalRequired`, which the core's closed list carries from rules version 1
//! (ingress answered it from a type of its own before). Its published shape is tested here: the
//! body, the status and the OpenAPI alternative say the same, and the description is the one
//! `SPEC.md` §10 holds, word for word.

use axum::http::StatusCode;
use ingress::openapi;
use ingress::surface::{refusal, status_of};
use ledger_core::Refusal;
use serde_json::Value;

/// `SPEC.md`, as it is in this checkout.
const SPEC: &str = include_str!("../../../SPEC.md");

/// The condition `SPEC.md` §10 publishes for `code`.
fn published_condition(code: &str) -> String {
    let prefix = format!("| `{code}` | ");
    let line = SPEC
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("{code} is not in SPEC §10"));
    line[prefix.len()..]
        .strip_suffix(" |")
        .expect("a table row ends with a pipe")
        .to_owned()
}

#[test]
fn the_description_is_the_one_spec_10_holds() {
    let r = Refusal::CommandApprovalRequired;
    assert_eq!(
        r.description(),
        published_condition(r.code()),
        "{}",
        r.code()
    );
}

#[test]
fn the_status_is_the_one_of_not_authorised() {
    // The owner's answer to Q-D (amendment 0037, R2): the same web status as `ERR-NotAuthorised`.
    assert_eq!(
        status_of(Refusal::CommandApprovalRequired),
        status_of(Refusal::NotAuthorised)
    );
    assert_eq!(
        status_of(Refusal::CommandApprovalRequired),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn req_cs_4_the_refusal_matches_its_openapi_description() {
    let doc = openapi::document();
    let alternatives = doc["components"]["schemas"]["Refusal"]["oneOf"]
        .as_array()
        .unwrap();
    {
        let r = Refusal::CommandApprovalRequired;
        assert!(
            Refusal::ALL.contains(&r),
            "{}: the core's closed list carries the line (T-041-06)",
            r.code()
        );
        let published = alternatives
            .iter()
            .find(|a| a["properties"]["error"]["const"] == r.code())
            .unwrap_or_else(|| panic!("{} is not published", r.code()));
        let resp = refusal(r);
        assert_eq!(
            u64::from(resp.status().as_u16()),
            published["x-http-status"].as_u64().unwrap()
        );
        assert!(resp.status().is_client_error(), "a refusal is a 4xx");
        assert!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none()
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body.as_object().unwrap().len(),
            2,
            "only error and description"
        );
        assert_eq!(body["error"], published["properties"]["error"]["const"]);
        assert_eq!(
            body["description"],
            published["properties"]["description"]["const"]
        );
        assert_eq!(published["additionalProperties"], false);
        assert_eq!(
            published["required"],
            serde_json::json!(["error", "description"])
        );
    }
}
