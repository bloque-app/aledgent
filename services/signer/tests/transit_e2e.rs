// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Vault Transit engine end to end (`F-042`): against a real Vault, in dev
//! mode, in CI (the `vault transit (F-042)` job of `.github/workflows/ci.yml`). It needs the cargo
//! feature `test-vault-auth`, because the AWS login the component uses in production cannot run
//! against a dev-mode Vault: the test logs in with a short-lived token that it asks the root for,
//! through the one test-only hook. A release build refuses that feature (`src/lib.rs`).
//!
//! Without `ALEDGENT_TEST_VAULT` the tests skip, unless `ALEDGENT_REQUIRE_VAULT_TEST=1`, which CI
//! sets: a missing Vault then fails them instead.
//!
//! ```text
//! ALEDGENT_TEST_VAULT=http://127.0.0.1:8200 ALEDGENT_TEST_VAULT_TOKEN=<root> \
//!   cargo test -p signer --features test-vault-auth --test transit_e2e
//! ```
#![cfg(feature = "test-vault-auth")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::json;
use signer::client::{ClientError, OnFailure, Patience, RemoteEd25519};
use signer::engine::Engine;
use signer::proto::Scheme;
use signer::transit::{Policy, TransitEngine, VaultSession};
use signer::vault_config::{VaultAuth, VaultConfig};
use signer::vault_session::TestLogin;

mod common;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The logins a test made: how many, and the token of the last.
#[derive(Default)]
struct Logins {
    count: AtomicUsize,
    last: Mutex<String>,
}

impl Logins {
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

struct Vault {
    address: String,
    root: String,
    http: reqwest::Client,
}

fn vault() -> Option<Vault> {
    match (
        std::env::var("ALEDGENT_TEST_VAULT"),
        std::env::var("ALEDGENT_TEST_VAULT_TOKEN"),
    ) {
        (Ok(address), Ok(root)) => Some(Vault {
            address: address.trim_end_matches('/').to_owned(),
            root,
            http: reqwest::Client::new(),
        }),
        _ if std::env::var("ALEDGENT_REQUIRE_VAULT_TEST").as_deref() == Ok("1") => {
            panic!(
                "ALEDGENT_REQUIRE_VAULT_TEST=1 and no ALEDGENT_TEST_VAULT / ALEDGENT_TEST_VAULT_TOKEN"
            )
        }
        _ => {
            eprintln!("skipped: no ALEDGENT_TEST_VAULT");
            None
        }
    }
}

impl Vault {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let mut r = self
            .http
            .request(method, format!("{}/v1/{path}", self.address))
            .header("X-Vault-Token", token);
        if !body.is_null() {
            r = r.json(&body);
        }
        let r = r.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(json!({})))
    }

    async fn root(
        &self,
        method: reqwest::Method,
        path: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        self.call(method, path, &self.root, body).await
    }

    /// Transit mounted, the keys of these tests and the policy a signer's role has: idempotent.
    async fn setup(&self) {
        const POST: reqwest::Method = reqwest::Method::POST;
        const PUT: reqwest::Method = reqwest::Method::PUT;
        // Mounted already by CI; a second mount answers 400.
        let _ = self
            .root(POST, "sys/mounts/transit", json!({ "type": "transit" }))
            .await;
        let (s, _) = self
            .root(
                POST,
                "transit/keys/lane-1",
                json!({ "type": "ed25519", "exportable": false }),
            )
            .await;
        assert!(s == 200 || s == 204, "creating lane-1: {s}");
        // A second version, so the pin means something.
        let (_, k) = self
            .root(reqwest::Method::GET, "transit/keys/lane-1", json!(null))
            .await;
        if k["data"]["latest_version"].as_u64() < Some(2) {
            let (s, _) = self
                .root(POST, "transit/keys/lane-1/rotate", json!({}))
                .await;
            assert!(s == 200 || s == 204, "rotating lane-1: {s}");
        }
        for (name, body) in [
            ("leaky", json!({ "type": "ed25519", "exportable": true })),
            ("ecdsa", json!({ "type": "ecdsa-p256" })),
        ] {
            let (s, _) = self.root(POST, &format!("transit/keys/{name}"), body).await;
            assert!(s == 200 || s == 204, "creating {name}: {s}");
        }
        let hcl = r#"
path "transit/keys/lane-1" { capabilities = ["read"] }
path "transit/sign/lane-1" { capabilities = ["update"] }
"#;
        let (s, b) = self
            .root(PUT, "sys/policies/acl/lane-sign", json!({ "policy": hcl }))
            .await;
        assert!(s == 200 || s == 204, "policy: {s} {b}");
    }

    /// A login for the component: a token of the `lane-sign` policy and nothing else, living `ttl`.
    fn login(self: &Arc<Self>, ttl: &str, logins: Arc<Logins>) -> TestLogin {
        let (v, ttl) = (Arc::clone(self), ttl.to_owned());
        Arc::new(move || {
            let (v, ttl, logins) = (Arc::clone(&v), ttl.clone(), Arc::clone(&logins));
            Box::pin(async move {
                let (s, b) = v
                    .root(
                        reqwest::Method::POST,
                        "auth/token/create",
                        json!({ "policies": ["lane-sign"], "ttl": ttl, "renewable": false }),
                    )
                    .await;
                if s != 200 {
                    return Err(format!("creating a token: {s}"));
                }
                let token = b["auth"]["client_token"]
                    .as_str()
                    .ok_or("no token")?
                    .to_owned();
                logins.count.fetch_add(1, Ordering::SeqCst);
                *logins.last.lock().unwrap() = token.clone();
                let lease = b["auth"]["lease_duration"].as_u64().ok_or("no lease")?;
                Ok((token, Some(Duration::from_secs(lease))))
            })
        })
    }
}

fn vault_config(address: &str) -> VaultConfig {
    VaultConfig {
        address: address.to_owned(),
        auth: VaultAuth::AwsIam {
            mount: "aws".into(),
            role: "unused: the test logs in itself".into(),
            region: Some("us-east-1".into()),
            server_id: None,
        },
    }
}

const TRY_ONLY: Patience = Patience {
    wait: Duration::ZERO,
    on_failure: OnFailure::Panic,
};

fn locator(key: &str, version: u32) -> serde_json::Value {
    json!({ "name": key, "version": version })
}

fn engine_with(address: &str, login: TestLogin) -> Result<TransitEngine, String> {
    let session = VaultSession::new(&vault_config(address))
        .unwrap()
        .with_policy(Policy {
            heartbeat: Duration::ZERO,
            relogin_cooldown: Duration::ZERO,
            ..Policy::default()
        })
        .with_test_login(login);
    TransitEngine::with_session(session, "transit")
}

/// An engine and a key of it, used the way a component uses one.
struct K {
    engine: TransitEngine,
    key: RemoteEd25519,
}

impl K {
    fn public(&self) -> [u8; 32] {
        self.key.public()
    }
}

async fn connect(
    v: &Arc<Vault>,
    key: &str,
    version: u32,
    ttl: &str,
    logins: &Arc<Logins>,
) -> Result<Arc<K>, String> {
    let (address, key, login) = (
        v.address.clone(),
        key.to_owned(),
        v.login(ttl, Arc::clone(logins)),
    );
    tokio::task::spawn_blocking(move || {
        let engine = engine_with(&address, login)?;
        let h = engine
            .open(Scheme::Ed25519, &locator(&key, version))
            .map_err(|e| e.to_string())?;
        let key = RemoteEd25519::from_key(h, TRY_ONLY).map_err(|e| e.to_string())?;
        Ok(Arc::new(K { engine, key }))
    })
    .await
    .unwrap()
}

async fn sign(k: &Arc<K>, msg: &'static [u8]) -> Result<[u8; 64], ClientError> {
    let k = Arc::clone(k);
    tokio::task::spawn_blocking(move || k.key.try_sign(msg))
        .await
        .unwrap()
}

/// Real Vault: the key signs, Vault itself verifies what it signed, the signature is pure
/// Ed25519 under the public key Transit published, and each pinned version is its own key.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_real_transit_key_signs_and_verifies_per_pinned_version() {
    let Some(v) = vault().map(Arc::new) else {
        return;
    };
    v.setup().await;
    let logins = Arc::new(Logins::default());
    let (k1, k2) = (
        connect(&v, "lane-1", 1, "60s", &logins).await.unwrap(),
        connect(&v, "lane-1", 2, "60s", &logins).await.unwrap(),
    );
    assert_ne!(
        k1.public(),
        k2.public(),
        "a new version is a new public key"
    );
    for k in [&k1, &k2] {
        let msg: &'static [u8] = b"a feed of commands";
        let sig = sign(k, msg).await.unwrap();
        // Independent of this crate: ed25519-dalek over the bytes, under the published key.
        ed25519_dalek::VerifyingKey::from_bytes(&k.public())
            .unwrap()
            .verify_strict(msg, &ed25519_dalek::Signature::from_bytes(&sig))
            .unwrap();
    }
    // And Vault, asked to verify what the pinned version signed, says yes; for the other, no.
    let msg = b"a feed of commands";
    let sig = sign(&k1, msg).await.unwrap();
    for (version, valid) in [(1, true), (2, false)] {
        let (s, b) = v
            .root(
                reqwest::Method::POST,
                "transit/verify/lane-1",
                json!({ "input": B64.encode(msg), "signature": format!("vault:v{version}:{}", B64.encode(sig)) }),
            )
            .await;
        assert_eq!(s, 200, "{b}");
        assert_eq!(b["data"]["valid"], valid, "version {version}: {b}");
    }
    assert!(
        k1.engine
            .metrics()
            .render()
            .contains("signer_transit_signs_total 2")
    );
}

/// Real Vault refuses to hand over a key it will not keep: an exportable key, a key of another
/// type, a version that is not there.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_real_exportable_or_non_ed25519_key_is_refused() {
    let Some(v) = vault().map(Arc::new) else {
        return;
    };
    v.setup().await;
    // These keys are outside the lane-sign policy: the root signs in for the test of the check.
    let logins = Arc::new(Logins::default());
    for (key, want) in [("leaky", "exportable"), ("ecdsa", "ed25519")] {
        let (address, root, name) = (v.address.clone(), v.root.clone(), key.to_owned());
        let e = tokio::task::spawn_blocking(move || {
            let login: TestLogin = Arc::new(move || {
                let root = root.clone();
                Box::pin(async move { Ok((root, None)) })
            });
            let engine = engine_with(&address, login)?;
            engine
                .open(Scheme::Ed25519, &locator(&name, 1))
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(e.contains(want), "{key}: {e}");
    }
    let e = connect(&v, "lane-1", 9, "60s", &logins)
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(e.contains("version 9 does not exist"), "{e}");
}

/// Real tokens: one nearing the end of its lease is replaced before it is used, and one Vault has
/// revoked is met with a real 403, one login, and a signature.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_real_token_is_renewed_and_a_revoked_one_is_replaced_once() {
    let Some(v) = vault().map(Arc::new) else {
        return;
    };
    v.setup().await;
    let logins = Arc::new(Logins::default());
    let k = connect(&v, "lane-1", 1, "6s", &logins).await.unwrap();
    assert_eq!(logins.count(), 1);
    sign(&k, b"1").await.unwrap();
    // Two thirds of six seconds is four.
    tokio::time::sleep(Duration::from_millis(4300)).await;
    sign(&k, b"2").await.unwrap();
    assert_eq!(logins.count(), 2, "renewed before use");
    assert_eq!(
        k.engine
            .metrics()
            .relogins_after_refusal
            .load(Ordering::SeqCst),
        0
    );
    // Revoke the token the key now holds: Vault answers 403, the key logs in, and signs.
    let token = logins.last.lock().unwrap().clone();
    let (st, b) = v
        .root(
            reqwest::Method::POST,
            "auth/token/revoke",
            json!({ "token": token }),
        )
        .await;
    assert!(st == 200 || st == 204, "revoking: {st} {b}");
    sign(&k, b"3").await.unwrap();
    assert_eq!(logins.count(), 3, "one login after the revocation");
    assert_eq!(
        k.engine
            .metrics()
            .relogins_after_refusal
            .load(Ordering::SeqCst),
        1
    );
}

/// A signer's role can sign with its key and read it, and do nothing else with Transit.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_the_role_a_key_runs_under_reaches_only_its_own_key() {
    let Some(v) = vault().map(Arc::new) else {
        return;
    };
    v.setup().await;
    let logins = Arc::new(Logins::default());
    // The `lane-sign` policy does not reach `ecdsa`: the check at connect is denied, not wrong.
    let e = connect(&v, "ecdsa", 1, "60s", &logins)
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(e.contains("denied"), "{e}");
}

/// The common suite every engine passes, against the real Vault: the public half is the one Vault
/// published, a missing key is refused, and no Vault token appears in an error or a `Debug`.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_the_real_transit_engine_passes_the_common_suite() {
    let Some(v) = vault().map(Arc::new) else {
        return;
    };
    v.setup().await;
    let (_, k) = v
        .root(reqwest::Method::GET, "transit/keys/lane-1", json!(null))
        .await;
    let public = B64
        .decode(k["data"]["keys"]["1"]["public_key"].as_str().unwrap())
        .unwrap();
    let logins = Arc::new(Logins::default());
    let (address, login) = (v.address.clone(), v.login("60s", Arc::clone(&logins)));
    tokio::task::spawn_blocking(move || {
        let engine = engine_with(&address, login).unwrap();
        common::suite::run(&common::suite::Setup {
            engine: &engine,
            cases: vec![common::suite::Case {
                scheme: Scheme::Ed25519,
                locator: locator("lane-1", 1),
                public,
            }],
            missing: locator("ghost", 1),
            // Vault's service tokens begin `hvs.`.
            secret_marker: "hvs.",
        });
    })
    .await
    .unwrap();
}
