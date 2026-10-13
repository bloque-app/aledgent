// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `grants(identity)` through the surface (SPEC §8.2, amendment 0026): `GET
//! /v1/identities/{identity}/grants` answers every grant over the identity to the identity itself
//! (`a0026-q1` (a)), the operator and the identities principal; a holder gets the ones it holds;
//! anyone else gets `ERR-NotAuthorised`.

use axum::http::StatusCode;
use ingress::testkit::Deployment;
use serde_json::{Value, json};

async fn read(d: &Deployment, path: &str, token: &str, who: &str) -> Vec<Value> {
    let (s, v) = d.call("GET", path, Some(token), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{who} reads the grants: {v}");
    v["items"].as_array().cloned().unwrap_or_default()
}

fn ids(items: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = items
        .iter()
        .map(|g| g["grant"].as_str().unwrap_or_default().to_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn amendment_0026_the_grants_over_an_identity_are_read_by_who_may_and_refused_to_anyone_else()
{
    let d = Deployment::start().await;
    let op = d.operator.token.clone();
    let user = d.party("user", 0).await;
    let onboarding = d.party("onboarding", 0).await;
    let custodian = d.party("custodian", 0).await;
    let other = d.party("other", 0).await;
    let stranger = d.party("stranger", 0).await;

    // Onboarding is the identities principal: the operator's `open_identity` grant (amendment 0025).
    let (s, v) = d
        .call(
            "POST",
            "/v1/ops/operator-grants",
            Some(&op),
            &[("client-key", "a0026-onboarding")],
            Some(json!({
                "principal": onboarding.identity,
                "kinds": ["open_identity"],
                "justification": "onboarding registers users",
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Two grants over the user's identity, neither made by the user: onboarding's to the custodian, the
    // operator's to another service.
    let mut given = Vec::new();
    for (key, grantor, grantee) in [
        ("a0026-custodian", &onboarding.token, &custodian.identity),
        ("a0026-other", &op, &other.identity),
    ] {
        let (s, v) = d
            .call(
                "POST",
                "/v1/grants",
                Some(grantor),
                &[("client-key", key)],
                Some(json!({
                    "kind": "delegated",
                    "principal": grantee,
                    "subject": { "identity": user.identity },
                    "ops": ["open_account"],
                    "per_window": { "length": 86_400_000u64, "limit": "1000" },
                })),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{key}: {v}");
        given.push(v["result"]["grant"].as_str().unwrap().to_owned());
    }
    given.sort();

    let path = format!("/v1/identities/{}/grants", user.identity);
    // The identity itself, the operator and the identities principal: every one.
    for (who, token) in [
        ("the identity itself", user.token.as_str()),
        ("the operator", op.as_str()),
        ("the identities principal", onboarding.token.as_str()),
    ] {
        let items = read(&d, &path, token, who).await;
        assert_eq!(ids(&items), given, "{who}");
        for g in &items {
            assert_ne!(g["grantor"], json!(user.identity), "{who}: {g}");
            assert_eq!(g["terms"]["subject"], json!({ "identity": user.identity }));
        }
    }
    // A holder: its own only.
    let items = read(&d, &path, &custodian.token, "the custodian").await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["terms"]["principal"], json!(custodian.identity));

    // Anyone else: refused.
    let (s, v) = d.call("GET", &path, Some(&stranger.token), &[], None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "a stranger is refused: {v}");
    assert_eq!(v["error"], "ERR-NotAuthorised", "{v}");
}
