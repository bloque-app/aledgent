// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-042`: ingress's feed key in Vault Transit, used in place. The key is built from `keys.feed` on a
//! `vault_transit` engine as a deployment writes it; it signs what the core host checks with the
//! public key it is given; and ingress's `/ready` and `/metrics` read it. Vault is a mock in this
//! process; the real Vault is `services/signer/tests/transit_e2e.rs`, in CI.
#![cfg(feature = "vault-transit")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use base64::Engine as _;
use ed25519_dalek::Signer as _;
use ingress::config::DeploymentConfig;
use ingress::feed::FeedKeys;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const TOKEN: &str = "a-development-token-of-at-least-32-bytes";

struct Vault {
    key: ed25519_dalek::SigningKey,
    down: AtomicBool,
    asked: Mutex<Vec<serde_json::Value>>,
}

fn denied() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "errors": ["permission denied"] })),
    )
}

async fn key(State(v): State<Arc<Vault>>, h: HeaderMap) -> (StatusCode, Json<serde_json::Value>) {
    if h.get("x-vault-token").and_then(|t| t.to_str().ok()) != Some(TOKEN) {
        return denied();
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({ "data": {
            "type": "ed25519", "exportable": false, "latest_version": 1,
            "keys": { "1": { "public_key": B64.encode(v.key.verifying_key().to_bytes()) } },
        }})),
    )
}

async fn sign(
    State(v): State<Arc<Vault>>,
    h: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    if v.down.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({})));
    }
    if h.get("x-vault-token").and_then(|t| t.to_str().ok()) != Some(TOKEN) {
        return denied();
    }
    v.asked.lock().unwrap().push(body.clone());
    let input = B64
        .decode(body["input"].as_str().unwrap_or_default())
        .unwrap_or_default();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "data": {
            "signature": format!("vault:v1:{}", B64.encode(v.key.sign(&input).to_bytes())),
        }})),
    )
}

async fn vault() -> (Arc<Vault>, String) {
    let v = Arc::new(Vault {
        key: ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]),
        down: AtomicBool::new(false),
        asked: Mutex::new(Vec::new()),
    });
    let app = axum::Router::new()
        .route("/v1/transit/keys/ingress-feed", get(key))
        .route("/v1/transit/sign/ingress-feed", post(sign))
        .with_state(Arc::clone(&v));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (v, at)
}

/// The deployment template's, in the development profile, with the feed key on a Transit engine whose
/// Vault logs in with a token in a file.
fn deployment(address: &str, token_file: &std::path::Path) -> DeploymentConfig {
    let text = include_str!("../../../deploy/templates/ingress.json");
    let mut filled = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("\"<FILL-NUMBER") {
        filled.push_str(&rest[..at]);
        let end = rest[at + 1..].find('"').map(|e| at + e + 2).unwrap();
        filled.push('1');
        rest = &rest[end..];
    }
    filled.push_str(rest);
    let mut v: serde_json::Value = serde_json::from_str(&filled).unwrap();
    v["profile"] = "development".into();
    v["stream"].as_object_mut().unwrap().remove("feed_key");
    v["engines"] = serde_json::json!({ "vault": { "vault_transit": {
        "vault": { "address": address, "auth": { "token_file": { "path": token_file } } },
        "mount": "transit",
        // Not ready after a second of Vault not answering (30 s by default).
        "unready_after_ms": 1000,
    }}});
    v["keys"] = serde_json::json!({
        "feed": { "engine": "vault", "key": { "name": "ingress-feed", "version": 1 } },
    });
    serde_json::from_value(v).unwrap()
}

/// The feed key a deployment names on a Transit engine signs what the core host verifies with its
/// public key, and when Vault does not answer ingress is not ready and says so in its metrics.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_feed_key_in_transit_signs_for_the_core_host_and_reports_vault() {
    let (v, address) = vault().await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("vault.token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let c = deployment(&address, &token_file);
    assert!(c.has_feed_key());
    let signer = tokio::task::spawn_blocking(move || c.feed_signer().unwrap().unwrap())
        .await
        .unwrap();
    let signer = signer.0;
    assert_eq!(signer.public(), v.key.verifying_key().to_bytes());
    let keys = FeedKeys::of(&[signer.public()]).unwrap();
    let msg = [7u8; 32];
    let sig = {
        let signer = Arc::clone(&signer);
        tokio::task::spawn_blocking(move || signer.sign(&msg))
            .await
            .unwrap()
            .unwrap()
    };
    assert!(keys.verifies(&msg, Some(&hex::encode(sig))));
    assert!(!keys.verifies(&[8u8; 32], Some(&hex::encode(sig))));
    assert_eq!(v.asked.lock().unwrap()[0]["key_version"], 1);
    assert!(signer.ready());
    assert!(
        signer
            .metrics_text()
            .contains("signer_transit_signs_total 1")
    );
    // Vault stops answering: that signature fails, ingress is not ready, the metrics say so.
    v.down.store(true, Ordering::SeqCst);
    let failed = {
        let signer = Arc::clone(&signer);
        tokio::task::spawn_blocking(move || signer.sign(&msg))
            .await
            .unwrap()
    };
    assert!(failed.unwrap_err().contains("unavailable"));
    // One failed request does not take ingress out of the load balancer; the failures are counted.
    assert!(signer.ready(), "one failed request");
    assert!(signer.metrics_text().contains("reason=\"unavailable\"} 1"));
    // Vault going on not answering for the window does.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let failed = {
        let signer = Arc::clone(&signer);
        tokio::task::spawn_blocking(move || signer.sign(&msg))
            .await
            .unwrap()
    };
    assert!(failed.is_err());
    assert!(!signer.ready());
    assert!(signer.metrics_text().contains("signer_vault_ready 0"));
    assert!(signer.metrics_text().contains("reason=\"unavailable\"} 2"));
    // And back: the same key signs again, and ingress is ready.
    v.down.store(false, Ordering::SeqCst);
    let again = {
        let signer = Arc::clone(&signer);
        tokio::task::spawn_blocking(move || signer.sign(&msg))
            .await
            .unwrap()
    };
    assert!(again.is_ok());
    assert!(signer.ready());
}
