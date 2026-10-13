// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `account(account)` through the surface (SPEC §8.2, amendment 0028): `GET /v1/accounts/{account}`
//! answers the account's owner, kind, custody and status to its owner, the operator, the
//! identities principal, a holder of a grant over the owner's identity and a holder of a grant
//! over the account; anyone else gets `ERR-UnknownAccount`, as for an account that does not exist.

use axum::http::StatusCode;
use ingress::testkit::Deployment;
use serde_json::{Value, json};

async fn ok(d: &Deployment, path: &str, token: &str, who: &str) -> Value {
    let (s, v) = d.call("GET", path, Some(token), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{who} reads the account: {v}");
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn amendment_0028_an_accounts_owner_is_read_by_who_may_and_refused_to_anyone_else() {
    let d = Deployment::start().await;
    let op = d.operator.token.clone();
    let user = d.party("user", 0).await;
    let account = user.account.clone().unwrap_or_default();
    let onboarding = d.party("onboarding", 0).await;
    let custodian = d.party("custodian", 0).await;
    let issuer = d.party("issuer", 0).await;
    let stranger = d.party("stranger", 0).await;

    // Onboarding is the identities principal: the operator's `open_identity` grant (amendment 0025).
    let (s, v) = d
        .call(
            "POST",
            "/v1/ops/operator-grants",
            Some(&op),
            &[("client-key", "a0028-onboarding")],
            Some(json!({
                "principal": onboarding.identity,
                "kinds": ["open_identity"],
                "justification": "onboarding registers users",
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // The custodian holds a grant over the user's identity, which onboarding gives (amendment 0025); the issuer
    // one over the account, which the operator gives.
    for (key, grantor, grantee, subject, ops) in [
        (
            "a0028-custodian",
            &onboarding.token,
            &custodian.identity,
            json!({ "identity": user.identity }),
            json!(["open_account", "transfer"]),
        ),
        (
            "a0028-issuer",
            &op,
            &issuer.identity,
            json!({ "account": account }),
            json!(["transfer"]),
        ),
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
                    "subject": subject,
                    "ops": ops,
                    "per_window": { "length": 86_400_000u64, "limit": "1000" },
                })),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{key}: {v}");
    }

    let path = format!("/v1/accounts/{account}");
    for (who, token) in [
        ("the owner", user.token.as_str()),
        ("the operator", op.as_str()),
        ("the identities principal", onboarding.token.as_str()),
        (
            "a holder of a grant over the owner",
            custodian.token.as_str(),
        ),
        (
            "a holder of a grant over the account",
            issuer.token.as_str(),
        ),
    ] {
        let v = ok(&d, &path, token, who).await;
        assert_eq!(
            v,
            json!({
                "account": account,
                "owner": user.identity,
                "kind": "customer",
                "custody": "delegated",
                "status": "active",
                "closed": false,
                "opened_at": v["opened_at"],
            }),
            "{who}"
        );
        assert!(v["opened_at"].is_u64(), "{v}");
        assert!(user.identity.starts_with("did:aledgent:id:"), "{v}");
    }

    // Anyone else: as an account that does not exist.
    let (s, v) = d.call("GET", &path, Some(&stranger.token), &[], None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a stranger is refused: {v}");
    assert_eq!(v["error"], "ERR-UnknownAccount", "{v}");
    // A grant over the owner reaches only that owner's accounts.
    let theirs = format!(
        "/v1/accounts/{}",
        stranger.account.clone().unwrap_or_default()
    );
    let (s, v) = d
        .call("GET", &theirs, Some(&custodian.token), &[], None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
}
