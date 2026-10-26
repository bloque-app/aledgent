// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The session with Vault (`F-042`): the token's life and the retries, against a Vault in this process,
//! through the signer's KV read, which runs over the session.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use signer::vault::{SecretStore, Vault};
use signer::vault_config::{VaultAuth, VaultConfig};
use signer::vault_session::Policy;

struct Mock {
    valid_token: Mutex<String>,
    lease: AtomicUsize,
    login_status: AtomicUsize,
    read_status: AtomicUsize,
    logins: AtomicUsize,
    login_attempts: AtomicUsize,
    reads: AtomicUsize,
}

async fn login(State(m): State<Arc<Mock>>) -> (StatusCode, Json<serde_json::Value>) {
    m.login_attempts.fetch_add(1, Ordering::SeqCst);
    let status = m.login_status.load(Ordering::SeqCst) as u16;
    if status != 200 {
        return (
            StatusCode::from_u16(status).unwrap(),
            Json(serde_json::json!({ "errors": ["no"] })),
        );
    }
    let n = m.logins.fetch_add(1, Ordering::SeqCst) + 1;
    let token = format!("token-{n}");
    *m.valid_token.lock().unwrap() = token.clone();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "auth": {
            "client_token": token, "lease_duration": m.lease.load(Ordering::SeqCst),
        }})),
    )
}

async fn read(State(m): State<Arc<Mock>>, h: HeaderMap) -> (StatusCode, Json<serde_json::Value>) {
    m.reads.fetch_add(1, Ordering::SeqCst);
    let status = m.read_status.load(Ordering::SeqCst) as u16;
    if status != 200 {
        return (
            StatusCode::from_u16(status).unwrap(),
            Json(serde_json::json!({ "errors": ["boom"] })),
        );
    }
    let valid = m.valid_token.lock().unwrap().clone();
    if h.get("x-vault-token").and_then(|t| t.to_str().ok()) != Some(valid.as_str()) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "errors": ["permission denied"] })),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({ "data": { "field": "value" } })),
    )
}

async fn mock() -> (Arc<Mock>, String) {
    let m = Arc::new(Mock {
        valid_token: Mutex::new(String::new()),
        lease: AtomicUsize::new(3600),
        login_status: AtomicUsize::new(200),
        read_status: AtomicUsize::new(200),
        logins: AtomicUsize::new(0),
        login_attempts: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
    });
    let app = axum::Router::new()
        .route("/v1/auth/aws/login", post(login))
        .route("/v1/secret/x", get(read))
        .with_state(Arc::clone(&m));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (m, at)
}

/// Short pauses, so the tests are fast; the shape is the production policy's.
fn fast() -> Policy {
    Policy {
        attempt_timeout: Duration::from_millis(500),
        max_attempts: 3,
        backoff: Duration::from_millis(5),
        deadline: Duration::from_secs(3),
        relogin_cooldown: Duration::ZERO,
        renew_retry: Duration::from_millis(50),
        heartbeat: Duration::ZERO,
    }
}

fn vault(address: &str, policy: Policy) -> Arc<Vault> {
    let c = VaultConfig {
        address: address.to_owned(),
        auth: VaultAuth::AwsIam {
            mount: "aws".into(),
            role: "r".into(),
            region: Some("us-east-1".into()),
            server_id: None,
        },
    };
    Arc::new(
        Vault::new(&c)
            .unwrap()
            .with_policy(policy)
            .with_credentials(aws_credential_types::Credentials::new(
                "AKIDTEST",
                "not-a-real-secret",
                None,
                None,
                "test",
            )),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_an_expired_token_is_replaced_once_and_the_read_succeeds() {
    let (m, at) = mock().await;
    let v = vault(&at, fast());
    assert!(v.read("secret/x").await.is_ok());
    assert_eq!(m.logins.load(Ordering::SeqCst), 1);
    *m.valid_token.lock().unwrap() = "expired".into();
    assert_eq!(v.read("secret/x").await.unwrap()["field"].as_str(), "value");
    assert_eq!(m.logins.load(Ordering::SeqCst), 2);
    assert_eq!(v.metrics().relogins_after_refusal.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nfr_8_concurrent_reads_that_meet_an_expired_token_share_one_login() {
    let (m, at) = mock().await;
    let v = vault(&at, fast());
    v.read("secret/x").await.unwrap();
    *m.valid_token.lock().unwrap() = "expired".into();
    let mut all = Vec::new();
    for _ in 0..12 {
        let v = Arc::clone(&v);
        all.push(tokio::spawn(async move { v.read("secret/x").await }));
    }
    for a in all {
        assert!(a.await.unwrap().is_ok());
    }
    assert_eq!(
        m.logins.load(Ordering::SeqCst),
        2,
        "the first, and one for all twelve"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_the_token_is_replaced_at_two_thirds_of_its_lease() {
    let (m, at) = mock().await;
    m.lease.store(1, Ordering::SeqCst);
    let v = vault(&at, fast());
    v.read("secret/x").await.unwrap();
    assert_eq!(m.logins.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(800)).await;
    v.read("secret/x").await.unwrap();
    assert_eq!(m.logins.load(Ordering::SeqCst), 2);
    assert_eq!(v.metrics().relogins_after_refusal.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_denied_read_does_not_log_in_again_and_again() {
    let (m, at) = mock().await;
    let v = vault(
        &at,
        Policy {
            relogin_cooldown: Duration::from_secs(30),
            ..fast()
        },
    );
    v.read("secret/x").await.unwrap();
    *m.valid_token.lock().unwrap() = "nobody has this".into();
    for _ in 0..5 {
        let e = v.read("secret/x").await.unwrap_err();
        assert!(e.contains("denied") || e.contains("403"), "{e}");
    }
    assert_eq!(
        m.logins.load(Ordering::SeqCst),
        1,
        "one login, however many refusals"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_login_vault_refuses_fails_the_read_with_bounded_attempts() {
    let (m, at) = mock().await;
    m.login_status.store(503, Ordering::SeqCst);
    let v = vault(&at, fast());
    let e = v.read("secret/x").await.unwrap_err();
    assert!(e.contains("unavailable"), "{e}");
    assert_eq!(m.login_attempts.load(Ordering::SeqCst), 3, "bounded");
    // A role Vault does not know: not retried.
    m.login_status.store(400, Ordering::SeqCst);
    let before = m.login_attempts.load(Ordering::SeqCst);
    let v = vault(&at, fast());
    let e = v.read("secret/x").await.unwrap_err();
    assert!(e.contains("login"), "{e}");
    assert_eq!(m.login_attempts.load(Ordering::SeqCst) - before, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_5xx_is_retried_a_bounded_number_of_times_and_the_next_read_succeeds() {
    let (m, at) = mock().await;
    let v = vault(&at, fast());
    v.read("secret/x").await.unwrap();
    m.read_status.store(503, Ordering::SeqCst);
    let before = m.reads.load(Ordering::SeqCst);
    let e = v.read("secret/x").await.unwrap_err();
    assert!(e.contains("unavailable"), "{e}");
    assert_eq!(m.reads.load(Ordering::SeqCst) - before, 3);
    assert!(!v.metrics().readiness().ready);
    m.read_status.store(200, Ordering::SeqCst);
    assert!(v.read("secret/x").await.is_ok());
    assert!(v.metrics().readiness().ready);
}
