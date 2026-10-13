// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The system-account recipe through the surface (`REQ-RC-7`, amendment 0024, ruling
//! `system-accounts-operator`; `ops/runbooks/system-accounts.md`): the operator opens an account
//! under its own identity, names it by its role, grants a service account-scoped authority; the
//! service resolves the role, and the operator reads the grants on the account.

use axum::http::StatusCode;
use ingress::testkit::{Deployment, USD};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn req_rc_7_a_system_account_by_its_role_and_its_grants() {
    let d = Deployment::start().await;
    let op = d.operator.token.clone();
    let issuer = d.party("issuer", 0).await;
    let (s, v) = d
        .call(
            "POST",
            "/v1/accounts",
            Some(&op),
            &[("client-key", "system-issuer-fee")],
            Some(
                json!({ "owner": d.operator.identity, "kind": "customer", "custody": "delegated" }),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "open_account: {v}");
    let fee = v["result"]["account"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let alias = json!({ "source": "system-role", "name": "issuer/fee", "justification": "issue 1: the issuer's fee account" });
    let path = format!("/v1/ops/accounts/{fee}/aliases");
    let (s, v) = d
        .call("POST", &path, Some(&op), &[], Some(alias.clone()))
        .await;
    assert_eq!(s, StatusCode::OK, "register_alias: {v}");
    // A repeat under the same derived client key is the same request, answered as before.
    let (s, _) = d.call("POST", &path, Some(&op), &[], Some(alias)).await;
    assert_eq!(s, StatusCode::OK, "a repeat");
    // Another registration of the same name is refused: an alias names one account for good.
    let again = json!({ "source": "system-role", "name": "issuer/fee", "justification": "again" });
    let (s, v) = d.call("POST", &path, Some(&op), &[], Some(again)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    // Not the service's to register.
    let theirs = json!({ "source": "system-role", "name": "issuer/other", "justification": "x" });
    let (s, _) = d
        .call("POST", &path, Some(&issuer.token), &[], Some(theirs))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let query = "/v1/aliases?source=system-role&name=issuer/fee";
    // Before its grant the service cannot reach the account: the role is unknown to it.
    let (s, v) = d.call("GET", query, Some(&issuer.token), &[], None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = d
        .call(
            "POST",
            "/v1/grants",
            Some(&op),
            &[],
            Some(json!({
                "kind": "delegated",
                "principal": issuer.identity,
                "subject": { "account": fee },
                "ops": ["transfer"],
                "per_window": { "length": 86_400_000u64, "limit": "100000" },
                "assets": [USD],
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "the account-scoped grant: {v}");
    let (s, v) = d.call("GET", query, Some(&issuer.token), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["account"], fee.as_str());
    // The hex spelling of the source is the same source.
    let hex = "/v1/aliases?source=73797374656d2d726f6c650000000000&name=issuer/fee";
    let (s, v) = d.call("GET", hex, Some(&op), &[], None).await;
    assert_eq!((s, v["account"].clone()), (StatusCode::OK, json!(fee)));

    let grants = format!("/v1/accounts/{fee}/grants");
    let (s, v) = d.call("GET", &grants, Some(&op), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["terms"]["principal"], issuer.identity.as_str());
    assert_eq!(items[0]["grantor"], d.operator.identity.as_str());
    assert!(items[0].get("revoked").is_none());
    // A grantee does not read the grants on the account.
    let (s, _) = d.call("GET", &grants, Some(&issuer.token), &[], None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
