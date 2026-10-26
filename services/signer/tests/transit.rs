// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Vault Transit engine (`F-042`) against a Vault in this process: the requests it makes, and what it
//! does when Vault misbehaves. The same engine against a real Vault is `tests/transit_e2e.rs`, in CI.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use base64::Engine as _;
use ed25519_dalek::Signer as _;
use signer::client::{ClientError, OnFailure, Patience, RemoteEd25519};
use signer::engine::Engine;
use signer::proto::{Refusal, Scheme};
use signer::transit::{Policy, Reason, TransitEngine, VaultSession};
use signer::vault_config::{VaultAuth, VaultConfig};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Clone)]
enum SignMode {
    Good,
    /// Answers with this version in `vault:v<N>:`.
    ReportVersion(u32),
    /// Signs with a key that is not the key's.
    WrongKey,
    /// Answers this status.
    Status(u16),
}

struct Knobs {
    valid_token: String,
    lease: u64,
    sign: SignMode,
    key_type: String,
    exportable: bool,
    derived: bool,
    plaintext_backup: bool,
    supports_signing: bool,
    min_encryption_version: u32,
    versions: Vec<u32>,
    login_status: u16,
}

struct Mock {
    knobs: Mutex<Knobs>,
    logins: AtomicUsize,
    sign_requests: AtomicUsize,
    last_sign: Mutex<Option<serde_json::Value>>,
}

fn key_of(version: u32) -> ed25519_dalek::SigningKey {
    let mut seed = [0x42u8; 32];
    seed[0] = version as u8;
    ed25519_dalek::SigningKey::from_bytes(&seed)
}

fn permission_denied() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "errors": ["permission denied"] })),
    )
}

impl Mock {
    fn allowed(&self, h: &HeaderMap) -> bool {
        let valid = self.knobs.lock().unwrap().valid_token.clone();
        h.get("x-vault-token").and_then(|t| t.to_str().ok()) == Some(valid.as_str())
    }

    fn set(&self, f: impl FnOnce(&mut Knobs)) {
        f(&mut self.knobs.lock().unwrap());
    }

    fn logins(&self) -> usize {
        self.logins.load(Ordering::SeqCst)
    }

    fn sign_requests(&self) -> usize {
        self.sign_requests.load(Ordering::SeqCst)
    }
}

async fn login(State(m): State<Arc<Mock>>) -> (StatusCode, Json<serde_json::Value>) {
    let status = m.knobs.lock().unwrap().login_status;
    if status != 200 {
        return (
            StatusCode::from_u16(status).unwrap(),
            Json(serde_json::json!({ "errors": ["no"] })),
        );
    }
    let n = m.logins.fetch_add(1, Ordering::SeqCst) + 1;
    let token = format!("token-{n}");
    let lease = {
        let mut k = m.knobs.lock().unwrap();
        k.valid_token = token.clone();
        k.lease
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({ "auth": { "client_token": token, "lease_duration": lease } })),
    )
}

async fn key(State(m): State<Arc<Mock>>, h: HeaderMap) -> (StatusCode, Json<serde_json::Value>) {
    if !m.allowed(&h) {
        return permission_denied();
    }
    let k = m.knobs.lock().unwrap();
    let keys: serde_json::Map<String, serde_json::Value> = k
        .versions
        .iter()
        .map(|v| {
            (
                v.to_string(),
                serde_json::json!({
                    "name": "ed25519", "creation_time": "2026-10-25T00:00:00Z",
                    "public_key": B64.encode(key_of(*v).verifying_key().to_bytes()),
                }),
            )
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "data": {
            "type": k.key_type, "exportable": k.exportable, "derived": k.derived,
            "allow_plaintext_backup": k.plaintext_backup, "supports_signing": k.supports_signing,
            "min_encryption_version": k.min_encryption_version, "min_available_version": 0,
            "latest_version": k.versions.iter().max(), "keys": keys,
        }})),
    )
}

async fn sign(
    State(m): State<Arc<Mock>>,
    h: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    m.sign_requests.fetch_add(1, Ordering::SeqCst);
    if !m.allowed(&h) {
        return permission_denied();
    }
    *m.last_sign.lock().unwrap() = Some(body.clone());
    let mode = m.knobs.lock().unwrap().sign.clone();
    // Pure Ed25519 over the bytes: no prehash, no hash algorithm.
    if body.get("prehashed").is_some() || body.get("hash_algorithm").is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "errors": ["not pure"] })),
        );
    }
    let version = body["key_version"].as_u64().unwrap_or(0) as u32;
    let input = B64
        .decode(body["input"].as_str().unwrap_or_default())
        .unwrap_or_default();
    let (signing, reported) = match mode {
        SignMode::Status(s) => {
            return (
                StatusCode::from_u16(s).unwrap(),
                Json(serde_json::json!({ "errors": ["boom"] })),
            );
        }
        SignMode::Good => (key_of(version), version),
        SignMode::ReportVersion(n) => (key_of(version), n),
        SignMode::WrongKey => (key_of(200), version),
    };
    let sig = signing.sign(&input).to_bytes();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "data": {
            "signature": format!("vault:v{reported}:{}", B64.encode(sig)),
            "key_version": version,
        }})),
    )
}

async fn mock() -> (Arc<Mock>, String) {
    let m = Arc::new(Mock {
        knobs: Mutex::new(Knobs {
            valid_token: String::new(),
            lease: 3600,
            sign: SignMode::Good,
            key_type: "ed25519".into(),
            exportable: false,
            derived: false,
            plaintext_backup: false,
            supports_signing: true,
            min_encryption_version: 0,
            versions: vec![1],
            login_status: 200,
        }),
        logins: AtomicUsize::new(0),
        sign_requests: AtomicUsize::new(0),
        last_sign: Mutex::new(None),
    });
    let app = axum::Router::new()
        .route("/v1/auth/aws/login", post(login))
        .route("/v1/transit/keys/lane-1", get(key))
        .route("/v1/transit/sign/lane-1", post(sign))
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
        // Not ready at the first failure, so these tests see it at once; the window has its own test.
        unready_after: Duration::ZERO,
    }
}

fn session(address: &str, policy: Policy) -> VaultSession {
    VaultSession::new(&VaultConfig {
        address: address.to_owned(),
        auth: VaultAuth::AwsIam {
            mount: "aws".into(),
            role: "feed-signer".into(),
            region: Some("us-east-1".into()),
            server_id: None,
        },
    })
    .unwrap()
    .with_policy(policy)
    .with_credentials(aws_credential_types::Credentials::new(
        "AKIDTEST",
        "not-a-real-secret",
        None,
        None,
        "test",
    ))
}

const TRY_ONLY: Patience = Patience {
    wait: Duration::ZERO,
    on_failure: OnFailure::Panic,
};

/// An engine and a key of it, used the way a component uses one.
struct K {
    engine: TransitEngine,
    key: RemoteEd25519,
}

impl K {
    fn public(&self) -> [u8; 32] {
        self.key.public()
    }

    fn metrics(&self) -> Arc<signer::transit::Metrics> {
        self.engine.metrics()
    }

    fn ready(&self) -> bool {
        self.engine.health().ready
    }
}

fn locator(version: u32) -> serde_json::Value {
    serde_json::json!({ "name": "lane-1", "version": version })
}

async fn engine(address: &str, policy: Policy) -> Arc<TransitEngine> {
    let s = session(address, policy);
    Arc::new(
        tokio::task::spawn_blocking(move || TransitEngine::with_session(s, "transit").unwrap())
            .await
            .unwrap(),
    )
}

async fn connect(address: &str, version: u32, policy: Policy) -> Result<Arc<K>, String> {
    let s = session(address, policy);
    tokio::task::spawn_blocking(move || {
        let engine = TransitEngine::with_session(s, "transit")?;
        let h = engine
            .open(Scheme::Ed25519, &locator(version))
            .map_err(|e| e.to_string())?;
        let key = RemoteEd25519::from_key(h, TRY_ONLY).map_err(|e| e.to_string())?;
        Ok(Arc::new(K { engine, key }))
    })
    .await
    .unwrap()
}

async fn try_sign(k: &Arc<K>, msg: &'static [u8]) -> Result<[u8; 64], ClientError> {
    let k = Arc::clone(k);
    tokio::task::spawn_blocking(move || k.key.try_sign(msg))
        .await
        .unwrap()
}

fn verifies(public: [u8; 32], msg: &[u8], sig: [u8; 64]) -> bool {
    ed25519_dalek::VerifyingKey::from_bytes(&public)
        .unwrap()
        .verify_strict(msg, &ed25519_dalek::Signature::from_bytes(&sig))
        .is_ok()
}

/// The common suite every engine passes: against the mock, with the token as the secret that must not
/// appear anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_the_transit_engine_passes_the_common_suite() {
    let (m, at) = mock().await;
    m.set(|k| k.versions = vec![1, 2]);
    let e = engine(&at, fast()).await;
    let public = key_of(2).verifying_key().to_bytes().to_vec();
    tokio::task::spawn_blocking(move || {
        common::suite::run(&common::suite::Setup {
            engine: &*e,
            cases: vec![common::suite::Case {
                scheme: Scheme::Ed25519,
                locator: locator(2),
                public,
            }],
            missing: serde_json::json!({ "name": "ghost", "version": 1 }),
            secret_marker: "token-",
        });
    })
    .await
    .unwrap();
}

/// A good signature: pure Ed25519 over the bytes, for the pinned version, with the public key Vault
/// gave, and a body with nothing in it that would hash the bytes first.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_transit_key_signs_pure_ed25519_for_the_pinned_version() {
    let (m, at) = mock().await;
    m.set(|k| k.versions = vec![1, 2]);
    let k = connect(&at, 2, fast()).await.unwrap();
    assert_eq!(k.public(), key_of(2).verifying_key().to_bytes());
    let sig = try_sign(&k, b"a feed").await.unwrap();
    assert!(verifies(k.public(), b"a feed", sig));
    let asked = m.last_sign.lock().unwrap().clone().unwrap();
    assert_eq!(asked["key_version"], 2);
    assert_eq!(asked["input"], B64.encode(b"a feed"));
    // `input` and `key_version`, and nothing else: no `prehashed`, no `hash_algorithm`.
    assert_eq!(asked.as_object().unwrap().len(), 2, "{asked}");
    assert!(asked.get("prehashed").is_none() && asked.get("hash_algorithm").is_none());
    assert_eq!(sig, key_of(2).sign(b"a feed").to_bytes());
    let text = k.metrics().render();
    assert!(
        text.contains("signer_transit_sign_latency_us_count 1"),
        "{text}"
    );
    assert!(text.contains("signer_transit_signs_total 1"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_signature_of_another_version_is_refused() {
    let (m, at) = mock().await;
    let k = connect(&at, 1, fast()).await.unwrap();
    m.set(|k| k.sign = SignMode::ReportVersion(2));
    let e = try_sign(&k, b"x").await.unwrap_err();
    assert!(
        matches!(&e, ClientError::Protocol(t) if t.contains("version_mismatch")),
        "{e:?}"
    );
    assert!(!e.is_transient());
    assert_eq!(k.metrics().failures(Reason::Version), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_bad_signature_from_the_server_is_caught_by_the_local_verify() {
    let (m, at) = mock().await;
    let k = connect(&at, 1, fast()).await.unwrap();
    m.set(|k| k.sign = SignMode::WrongKey);
    let e = try_sign(&k, b"x").await.unwrap_err();
    assert!(
        matches!(&e, ClientError::Protocol(t) if t.contains("bad_signature")),
        "{e:?}"
    );
    assert_eq!(k.metrics().failures(Reason::BadSignature), 1);
    m.set(|k| k.sign = SignMode::Good);
    assert!(try_sign(&k, b"x").await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_an_expired_token_is_replaced_once_and_the_request_succeeds() {
    let (m, at) = mock().await;
    let k = connect(&at, 1, fast()).await.unwrap();
    assert_eq!(m.logins(), 1);
    assert!(try_sign(&k, b"1").await.is_ok());
    m.set(|k| k.valid_token = "expired".into());
    let sig = try_sign(&k, b"2").await.unwrap();
    assert!(verifies(k.public(), b"2", sig));
    assert_eq!(m.logins(), 2);
    assert_eq!(k.metrics().relogins_after_refusal.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nfr_8_concurrent_requests_share_one_login() {
    let (m, at) = mock().await;
    let k = connect(&at, 1, fast()).await.unwrap();
    m.set(|k| k.valid_token = "expired".into());
    let mut all = Vec::new();
    for _ in 0..12 {
        let k = Arc::clone(&k);
        all.push(tokio::task::spawn_blocking(move || k.key.try_sign(b"x")));
    }
    for a in all {
        assert!(a.await.unwrap().is_ok());
    }
    assert_eq!(m.logins(), 2, "the first, and one for all twelve");
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_the_token_is_replaced_at_two_thirds_of_its_lease() {
    let (m, at) = mock().await;
    m.set(|k| k.lease = 1);
    let k = connect(&at, 1, fast()).await.unwrap();
    assert!(try_sign(&k, b"1").await.is_ok());
    assert_eq!(m.logins(), 1);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(try_sign(&k, b"2").await.is_ok());
    assert_eq!(m.logins(), 2);
    assert_eq!(k.metrics().relogins_after_refusal.load(Ordering::SeqCst), 0);
}

/// A Vault that answers 5xx: bounded attempts, then that request fails, the engine says it is not ready,
/// and when Vault returns the same key signs again.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_5xx_fails_that_request_and_never_the_process() {
    let (m, at) = mock().await;
    let k = connect(&at, 1, fast()).await.unwrap();
    assert!(k.ready());
    m.set(|k| k.sign = SignMode::Status(503));
    let before = m.sign_requests();
    let e = try_sign(&k, b"x").await.unwrap_err();
    assert!(
        matches!(&e, ClientError::Refused(Refusal::Unavailable(t)) if t.contains("unavailable")),
        "{e:?}"
    );
    assert!(e.is_transient());
    assert_eq!(m.sign_requests() - before, 3, "bounded: three attempts");
    assert!(!k.ready());
    assert!(k.engine.metrics_text().contains("signer_vault_ready 0"));
    assert_eq!(k.metrics().failures(Reason::Unavailable), 1);
    m.set(|k| k.sign = SignMode::Good);
    assert!(try_sign(&k, b"x").await.is_ok());
    assert!(k.ready());
}

/// A 400 from Vault is not worth waiting for.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_400_is_a_protocol_error_and_a_403_that_remains_is_not_granted() {
    let (m, at) = mock().await;
    let policy = Policy {
        relogin_cooldown: Duration::from_secs(30),
        ..fast()
    };
    let k = connect(&at, 1, policy).await.unwrap();
    m.set(|k| k.sign = SignMode::Status(400));
    let e = try_sign(&k, b"x").await.unwrap_err();
    assert!(matches!(e, ClientError::Protocol(_)), "{e:?}");
    assert!(!e.is_transient());
    m.set(|k| {
        k.sign = SignMode::Good;
        k.valid_token = "nobody has this".into();
    });
    for _ in 0..5 {
        let e = try_sign(&k, b"x").await.unwrap_err();
        assert_eq!(e, ClientError::Refused(Refusal::NotGranted));
        assert!(!e.is_transient());
    }
    assert_eq!(m.logins(), 1, "one login, however many refusals");
    assert_eq!(k.metrics().failures(Reason::Denied), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_login_vault_refuses_stops_the_start() {
    let (m, at) = mock().await;
    m.set(|k| k.login_status = 503);
    let e = connect(&at, 1, fast()).await.map(|_| ()).unwrap_err();
    assert!(e.contains("unavailable"), "{e}");
    m.set(|k| k.login_status = 400);
    let e = connect(&at, 1, fast()).await.map(|_| ()).unwrap_err();
    assert!(e.contains("login"), "{e}");
}

/// A key that could leave Vault, or is not what was asked for, is refused at open.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_key_that_could_leave_vault_or_is_not_the_pinned_one_is_refused() {
    let (m, at) = mock().await;
    let says = |e: Result<Arc<K>, String>, what: &str| {
        let e = match e {
            Err(e) => e,
            Ok(_) => panic!("{what}: the key was accepted"),
        };
        assert!(e.contains(what), "{what}: {e}");
    };
    m.set(|k| k.exportable = true);
    says(connect(&at, 1, fast()).await, "exportable");
    m.set(|k| {
        k.exportable = false;
        k.derived = true;
    });
    says(connect(&at, 1, fast()).await, "derived");
    m.set(|k| {
        k.derived = false;
        k.plaintext_backup = true;
    });
    says(connect(&at, 1, fast()).await, "plaintext backup");
    m.set(|k| {
        k.plaintext_backup = false;
        k.supports_signing = false;
    });
    says(connect(&at, 1, fast()).await, "does not support signing");
    m.set(|k| {
        k.supports_signing = true;
        k.key_type = "ecdsa-p256".into();
    });
    says(connect(&at, 1, fast()).await, "not \"ed25519\"");
    m.set(|k| {
        k.key_type = "ed25519".into();
        k.versions = vec![1, 2];
        k.min_encryption_version = 2;
    });
    says(
        connect(&at, 1, fast()).await,
        "below min_encryption_version",
    );
    says(connect(&at, 3, fast()).await, "version 3 does not exist");
    assert!(connect(&at, 2, fast()).await.is_ok());
}

/// The scheme and the locator are checked before Vault is asked.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_the_engine_offers_ed25519_and_wants_a_pinned_version() {
    let (_m, at) = mock().await;
    let e = engine(&at, fast()).await;
    let e2 = Arc::clone(&e);
    tokio::task::spawn_blocking(move || {
        assert_eq!(e2.schemes(), [Scheme::Ed25519]);
        assert_eq!(
            e2.open(Scheme::Clerk, &locator(1)).unwrap_err(),
            ClientError::Refused(Refusal::WrongScheme)
        );
        let no_version = serde_json::json!({ "name": "lane-1" });
        let err = e2.open(Scheme::Ed25519, &no_version).unwrap_err();
        assert!(err.to_string().contains("pin the key version"), "{err}");
    })
    .await
    .unwrap();
}

/// While idle the key is read again: one that became exportable makes the engine not ready, and a
/// signature is still verified and given.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_key_that_changes_under_the_pin_makes_the_engine_not_ready() {
    let (m, at) = mock().await;
    let policy = Policy {
        heartbeat: Duration::from_millis(40),
        ..fast()
    };
    let k = connect(&at, 1, policy).await.unwrap();
    assert!(k.ready());
    m.set(|k| k.exportable = true);
    let mut ready = true;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if !k.ready() {
            ready = false;
            break;
        }
    }
    assert!(!ready, "the heartbeat did not notice");
    assert!(try_sign(&k, b"x").await.is_ok());
}

/// One failed request, or a blip, does not make the engine not ready (every replica would leave its load
/// balancer at once); Vault failing for the whole window does, and every failure is counted either way.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_one_failed_request_does_not_make_the_engine_not_ready() {
    let (m, at) = mock().await;
    let policy = Policy {
        unready_after: Duration::from_millis(400),
        ..fast()
    };
    let k = connect(&at, 1, policy).await.unwrap();
    m.set(|k| k.sign = SignMode::Status(503));
    // A whole request failed (three attempts): still ready, and counted.
    assert!(try_sign(&k, b"x").await.is_err());
    assert!(k.ready(), "one failed request");
    assert_eq!(k.metrics().failures(Reason::Unavailable), 1);
    assert!(k.metrics().readiness().consecutive_failures >= 3);
    // A blip that ends: ready throughout, and the next request succeeds.
    m.set(|k| k.sign = SignMode::Good);
    assert!(try_sign(&k, b"x").await.is_ok());
    assert!(k.ready());
    // Failing for the whole window, and still: not ready, with every failure counted.
    m.set(|k| k.sign = SignMode::Status(503));
    assert!(try_sign(&k, b"x").await.is_err());
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(try_sign(&k, b"x").await.is_err());
    assert!(!k.ready(), "failing for longer than the window");
    assert_eq!(k.metrics().failures(Reason::Unavailable), 3);
    assert!(k.engine.metrics_text().contains("signer_vault_ready 0"));
    // Vault answers again: ready at once.
    m.set(|k| k.sign = SignMode::Good);
    assert!(try_sign(&k, b"x").await.is_ok());
    assert!(k.ready());
}
