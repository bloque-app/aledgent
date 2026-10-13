// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `REQ-AU-8`, `T-009-01`: no credential reaches the log. Alone in its own test binary, so the
//! subscriber it installs sees every event of the run.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use ingress::testkit::{Deployment, USD};
use serde_json::json;

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `REQ-AU-8`: a scan of everything logged over a run of commands, queries, deliveries and
/// refused requests finds no token and no verification secret.
#[tokio::test]
async fn req_au_8_a_log_scan_finds_no_credential() {
    let buf = Buffer::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let (s, r) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "5" }] })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let path = format!(
        "/v1/operations/{}",
        r["operation_reference"].as_str().unwrap()
    );
    let _ = d.call("GET", &path, Some(&alice.token), &[], None).await;
    let _ = d
        .call("GET", "/metrics", Some("wrong-token-value"), &[], None)
        .await;
    // F-028: an API key issued and used, and a passkey's session issued and used.
    let (api_key, session) = credentials(&d, &alice).await;
    assert!(d.all_delivered(Duration::from_secs(10)).await);
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("access"), "the access log ran");
    for secret in [
        d.operator.token.as_str(),
        alice.token.as_str(),
        bob.token.as_str(),
        "wrong-token-value",
        "secret-of-",
        "Bearer",
        "authorization",
        api_key.as_str(),
        api_key.rsplit('.').next().unwrap(),
        session.as_str(),
        session.rsplit('.').next().unwrap(),
    ] {
        assert!(!log.contains(secret), "{secret:?} reached the log");
    }
}

/// Issue and use an API key, and register a passkey, sign in and use the session (`F-028`).
async fn credentials(d: &Deployment, alice: &ingress::testkit::Party) -> (String, String) {
    use authn::webauthn::testing::Soft;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    d.auth.set_passkeys(ingress::auth::Passkeys::new(
        authn::token::AuthnKey::new([3; 32]),
        vec![authn::webauthn::RelyingParty {
            rp_id: "api.bloque.app".into(),
            origins: vec!["https://api.bloque.app".into()],
            registrations: true,
        }],
        true,
        120_000,
        900_000,
    ));
    let balances = format!("/v1/balances?identity={}", alice.identity);
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[],
            Some(json!({ "marker": "ordinary", "scheme": "api-key" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let api_key = v["api_key"].as_str().unwrap().to_owned();
    let (s, _) = d.call("GET", &balances, Some(&api_key), &[], None).await;
    assert_eq!(s, StatusCode::OK);
    let challenge = |purpose: &'static str| async move {
        let (_, v) = d
            .call(
                "POST",
                "/v1/auth/challenge",
                None,
                &[],
                Some(json!({ "purpose": purpose })),
            )
            .await;
        B64.decode(v["challenge"].as_str().unwrap()).unwrap()
    };
    let soft = Soft::new(21, "api.bloque.app");
    let (cd, att) = soft.register(&challenge("register").await, "https://api.bloque.app");
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[],
            Some(json!({
                "marker": "ordinary",
                "scheme": "webauthn-es256",
                "passkey": { "client_data_json": B64.encode(cd), "attestation_object": B64.encode(att) },
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let passkey = v["result"]["credential"].as_str().unwrap().to_owned();
    let (ad, cd, sig) = soft.assert(&challenge("authenticate").await, "https://api.bloque.app");
    let (s, v) = d
        .call(
            "POST",
            "/v1/auth/passkey",
            None,
            &[],
            Some(json!({
                "credential": passkey,
                "assertion": {
                    "raw_id": B64.encode(&soft.raw_id),
                    "authenticator_data": B64.encode(ad),
                    "client_data_json": B64.encode(cd),
                    "signature": B64.encode(sig),
                },
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let session = v["session"].as_str().unwrap().to_owned();
    let (s, _) = d.call("GET", &balances, Some(&session), &[], None).await;
    assert_eq!(s, StatusCode::OK);
    (api_key, session)
}
