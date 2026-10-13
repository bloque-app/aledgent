// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Undeliverable notifications are parked, not retried for ever (`F-031`, stress finding F5;
//! `REQ-EV-4`, `REQ-RT-4`).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use ingress::auth::{CredentialKind, Token};
use ingress::boundary::Core;
use ingress::store::{Recipient, Store};
use ingress::testkit::{Deployment, USD, fast_config};
use serde_json::json;

/// A funded holder with an account and a token, and no recipient: (identity, account, token).
async fn silent_holder(
    d: &Deployment<ingress::store::mem::MemStore>,
    name: &str,
) -> (String, String, String) {
    let op = Some(d.operator.token.as_str());
    let (s, v) = d
        .call(
            "POST",
            "/v1/identities",
            op,
            &[("client-key", &format!("identity-{name}"))],
            Some(json!({ "kind": "person", "verifier": "0a" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let identity = v["result"]["identity"].as_str().unwrap().to_owned();
    let credential = v["result"]["credential"].as_str().unwrap().to_owned();
    let token = format!("token-{name}");
    d.auth
        .add_token(
            &Token::new(token.as_bytes()),
            &identity,
            &credential,
            CredentialKind::Holder,
        )
        .unwrap();
    let (s, v) = d
        .call(
            "POST",
            "/v1/accounts",
            op,
            &[("client-key", &format!("account-{name}"))],
            Some(json!({ "owner": identity, "kind": "customer", "custody": "delegated" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let account = v["result"]["account"].as_str().unwrap().to_owned();
    d.fund(&account, 1_000).await;
    (identity, account, token)
}

/// The undelivered notifications whose recipient is `principal`.
async fn owed_to(d: &Deployment<ingress::store::mem::MemStore>, principal: &str) -> Vec<String> {
    let page = d.core.outbox(None, true, 0).await.unwrap();
    page.items
        .iter()
        .filter(|i| i.recipient.as_deref() == Some(principal))
        .map(|i| i.notification.id.clone())
        .collect()
}

/// Let the delivery worker run a while, the clock moving so that backoffs run out.
async fn run_for(d: &Deployment<ingress::store::mem::MemStore>, wall: Duration) {
    let until = Instant::now() + wall;
    while Instant::now() < until {
        d.clock.advance(50);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `REQ-EV-4`: a principal with no recipient parks its notifications: no attempt is made or
/// recorded, however long it waits; and once it registers a recipient they are delivered.
#[tokio::test]
async fn req_ev_4_no_recipient_parks_without_attempts_and_delivers_once_registered() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 0).await;
    let (bob, bob_account, bob_token) = silent_holder(&d, "bob").await;
    // Bob initiates: his notifications are owed to him, and he has no recipient.
    let (s, v) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&bob_token),
            &[],
            Some(json!({ "legs": [{ "from": bob_account, "to": alice.account, "asset": USD, "amount": "5" }] })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    run_for(&d, Duration::from_secs(2)).await;
    let owed = owed_to(&d, &bob).await;
    assert!(!owed.is_empty(), "the transfer notifies its initiator");
    for id in &owed {
        assert_eq!(d.store.attempts(id).await.unwrap(), 0, "{id} was attempted");
    }
    assert!(
        d.replica
            .shared
            .metrics
            .delivery_parked
            .load(Ordering::Relaxed)
            > 0
    );
    // Registering a recipient releases them.
    d.recipients.add("mem://bob", b"secret-of-bob");
    d.store
        .put_recipient(Recipient {
            principal: bob.clone(),
            endpoint: "mem://bob".into(),
            secret: Some(b"secret-of-bob".to_vec()),
        })
        .await
        .unwrap();
    assert!(d.all_delivered(Duration::from_secs(10)).await);
    let got = d.recipients.delivered_ids();
    assert!(owed.iter().all(|id| got.contains(id)));
}

/// `REQ-RT-4`, bounded: a recipient that keeps failing gets at most `delivery_max_attempts`
/// attempts per notification; then the notification is parked, still undelivered and counted.
#[tokio::test]
async fn req_rt_4_retries_to_a_failing_recipient_are_bounded() {
    let mut config = fast_config("ingress-0");
    config.delivery_max_attempts = 3;
    config.delivery_backoff_base = 1;
    config.delivery_backoff_max = 1;
    let d = Deployment::start_with(config).await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    d.recipients.set_up(&alice.endpoint, false);
    let (s, v) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "5" }] })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    run_for(&d, Duration::from_secs(2)).await;
    let owed = owed_to(&d, &alice.identity).await;
    assert!(!owed.is_empty());
    for id in &owed {
        assert_eq!(d.store.attempts(id).await.unwrap(), 3, "{id}");
    }
    run_for(&d, Duration::from_secs(1)).await;
    for id in &owed {
        assert_eq!(
            d.store.attempts(id).await.unwrap(),
            3,
            "{id} kept being retried"
        );
    }
}
