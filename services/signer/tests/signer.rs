// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The signer end to end, in process (`F-035`): round trips for every scheme and backend, the
//! refusals of unauthenticated and ungranted callers, and rotation of tokens and keys.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use ledger_core::anchor::clerk::{ClerkSigner, InProcessClerk};
use ledger_kernel::keys::{Custody, KeyRequest, RootKeys};
use signer::backend::Backends;
use signer::client::{
    ClientError, Patience, RemoteClerk, RemoteCustody, RemoteEd25519, RemoteSecp256k1,
    SignerClient, Token, recover_secp256k1,
};
use signer::config::Config;
use signer::kms::Kms;
use signer::kms::testing::FakeKms;
use signer::proto::{Op, Refusal};
use signer::server::Server;

async fn start(config: &serde_json::Value, backends: Backends) -> Arc<Server> {
    let c: Config = serde_json::from_value(config.clone()).unwrap();
    let server = Server::load(&c, backends).await.unwrap();
    let l = server.bind().unwrap();
    tokio::spawn(Arc::clone(&server).serve(l));
    server
}

fn connect(dir: &Path, client: &str, token: &Path) -> Result<SignerClient, ClientError> {
    SignerClient::with_token(
        &dir.join("signer.sock"),
        client,
        Token::from_file(token)?,
        Duration::from_secs(5),
    )
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// `F-035`: every scheme answers through the signer exactly as the key would in process.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_signing_round_trips_for_every_scheme() {
    let dir = tempfile::tempdir().unwrap();
    let uid = my_uid(dir.path());
    let (core_token, core_hash) = token(dir.path(), "core", 1);
    let (settle_token, settle_hash) = token(dir.path(), "settle", 2);
    start(
        &dev_config(dir.path(), uid, &core_hash, &settle_hash),
        Backends::default(),
    )
    .await;
    let d = dir.path().to_path_buf();
    blocking(move || {
        let core = Arc::new(connect(&d, "core", &core_token).unwrap());
        // The key ring: every question answered as the root answers it.
        let custody = RemoteCustody::new(Arc::clone(&core), "keyring", Patience::TEST).unwrap();
        let root = RootKeys::new(ROOT);
        for r in [
            KeyRequest::RegistrarKey,
            KeyRequest::AccountKey([1; 16]),
            KeyRequest::SignRegistrar(b"an account".to_vec()),
            KeyRequest::SignAccount([1; 16], b"a transfer".to_vec()),
            KeyRequest::EntryBlinding(ledger_kernel::keys::BlindingSeed {
                operation: [2; 16],
                kind: 4,
                ordinal: 0,
                index: 1,
            }),
        ] {
            assert_eq!(custody.answer(&r), root.answer(&r), "{}", r.kind());
        }
        // The clerk: CipherClerk's Schnorr, the same bytes as in process.
        let clerk = RemoteClerk::new(Arc::clone(&core), "clerk", Patience::TEST).unwrap();
        let local = InProcessClerk::from_secret(CLERK).unwrap();
        assert_eq!(clerk.public_key(), local.public_key());
        assert_eq!(clerk.sign(b"a disclosure"), local.sign(b"a disclosure"));
        // Ed25519: checked against the public key the signer published.
        let auth =
            RemoteEd25519::new(Arc::clone(&core), "agent-authority", Patience::TEST).unwrap();
        let sig = auth.sign(b"a receipt");
        let expect = ed25519_dalek::SigningKey::from_bytes(&AUTHORITY);
        assert_eq!(auth.public(), expect.verifying_key().to_bytes());
        assert_eq!(
            sig,
            ed25519_dalek::Signer::sign(&expect, b"a receipt").to_bytes()
        );
        // secp256k1, for the adapter.
        let settle = Arc::new(connect(&d, "settle", &settle_token).unwrap());
        let lane = RemoteSecp256k1::new(Arc::clone(&settle), "lane-1").unwrap();
        let digest = [9u8; 32];
        let sig = lane.sign_digest(digest).unwrap();
        assert_eq!(recover_secp256k1(&digest, &sig), Some(lane.public()));
        // A key of another scheme is the wrong one for an operation.
        assert_eq!(
            core.call("keyring", Op::Sign(vec![1])),
            Err(ClientError::Refused(Refusal::WrongScheme))
        );
    })
    .await;
}

/// A Vault in this process: an AWS IAM login (the role and the signed STS request checked), then
/// a KV v1 read of the core's keys under the token it gave.
async fn fake_vault(logins: Arc<std::sync::atomic::AtomicUsize>) -> String {
    use axum::Json;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use base64::Engine as _;
    let login = move |Json(v): Json<serde_json::Value>| {
        let logins = Arc::clone(&logins);
        async move {
            let b64 = |k: &str| {
                base64::engine::general_purpose::STANDARD
                    .decode(v[k].as_str().unwrap_or_default())
                    .unwrap_or_default()
            };
            let headers = String::from_utf8(b64("iam_request_headers")).unwrap_or_default();
            let ok = v["role"] == "aledgent-signer-iam"
                && v["iam_http_request_method"] == "POST"
                && b64("iam_request_url") == b"https://sts.us-east-2.amazonaws.com/"
                && headers.contains("AWS4-HMAC-SHA256 Credential=AKIDTEST/")
                && headers.contains("X-Vault-AWS-IAM-Server-ID");
            if !ok {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({})));
            }
            logins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                StatusCode::OK,
                Json(serde_json::json!({ "auth": { "client_token": "vault-token-1" } })),
            )
        }
    };
    let read = |h: HeaderMap| async move {
        if h.get("x-vault-token").and_then(|t| t.to_str().ok()) != Some("vault-token-1") {
            return (StatusCode::FORBIDDEN, Json(serde_json::json!({})));
        }
        (
            StatusCode::OK,
            Json(serde_json::json!({ "data": {
                "keyring": hex::encode(ROOT),
                "clerk": hex::encode(CLERK),
                "agent-authority": hex::encode(AUTHORITY),
            }})),
        )
    };
    let app = axum::Router::new()
        .route("/v1/auth/aledgent-signer/login", post(login))
        .route("/v1/secret/aledgent-signer/core", get(read));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    at
}

/// Owner ruling `key-custody-opt-in-kms`: the keys come from Vault, which the signer logs in to
/// with AWS IAM auth (a signed STS request, no secret configured) and reads itself; a lane key may
/// opt into AWS KMS (in memory here).
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_vault_keys_and_an_opt_in_kms_lane_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let uid = my_uid(dir.path());
    let (core_token, core_hash) = token(dir.path(), "core", 1);
    let logins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let address = fake_vault(Arc::clone(&logins)).await;
    let fake = Arc::new(FakeKms::default());
    fake.add_secp256k1("arn:lane", LANE);
    let src = |field: &str| serde_json::json!({ "vault": { "path": "secret/aledgent-signer/core", "field": field } });
    let config = serde_json::json!({
        "listen": dir.path().join("signer.sock"),
        // An http:// Vault in the test process only.
        "development": true,
        "aws_region": "us-east-2",
        "vault": { "address": address, "auth": { "aws_iam": {
            "mount": "aledgent-signer", "role": "aledgent-signer-iam",
            "region": "us-east-2", "server_id": "vault.test" } } },
        "clients": [{ "name": "core", "uids": [uid], "token_sha256": [core_hash],
                      "keys": ["keyring", "clerk", "agent-authority", "lane-1"] }],
        "keys": [
            { "id": "keyring", "scheme": "key_ring", "source": src("keyring") },
            { "id": "clerk", "scheme": "clerk", "source": src("clerk") },
            { "id": "agent-authority", "scheme": "ed25519", "source": src("agent-authority") },
            { "id": "lane-1", "scheme": "secp256k1", "source": { "kms": { "key_id": "arn:lane" } } }
        ]
    });
    let c: Config = serde_json::from_value(config.clone()).unwrap();
    let vault = signer::vault::Vault::new(c.vault.as_ref().unwrap())
        .unwrap()
        .with_credentials(aws_credential_types::Credentials::new(
            "AKIDTEST",
            "not-a-real-secret",
            None,
            None,
            "test",
        ));
    let backends = Backends {
        kms: Some(Arc::clone(&fake) as Arc<dyn Kms>),
        secrets: Some(Arc::new(vault)),
    };
    start(&config, backends).await;
    assert_eq!(
        logins.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one login"
    );
    let d = dir.path().to_path_buf();
    let f = Arc::clone(&fake);
    blocking(move || {
        let core = Arc::new(connect(&d, "core", &core_token).unwrap());
        let custody = RemoteCustody::new(Arc::clone(&core), "keyring", Patience::TEST).unwrap();
        let r = KeyRequest::SignAccount([5; 16], b"m".to_vec());
        assert_eq!(custody.answer(&r), RootKeys::new(ROOT).answer(&r));
        let clerk = RemoteClerk::new(Arc::clone(&core), "clerk", Patience::TEST).unwrap();
        assert_eq!(
            clerk.sign(b"d"),
            InProcessClerk::from_secret(CLERK).unwrap().sign(b"d")
        );
        let auth =
            RemoteEd25519::new(Arc::clone(&core), "agent-authority", Patience::TEST).unwrap();
        let _ = auth.sign(b"a receipt");
        let lane = RemoteSecp256k1::new(Arc::clone(&core), "lane-1").unwrap();
        for i in 0..4u8 {
            let d = [i; 32];
            let s = lane.sign_digest(d).unwrap();
            assert_eq!(recover_secp256k1(&d, &s), Some(lane.public()));
        }
        assert_eq!(
            f.signs.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "KMS signed the lane"
        );
    })
    .await;
}

/// `F-035`, `REQ-AU-8`: a caller is refused unless its user id, its name and its token all
/// match, and then it may use only the keys it is granted.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_8_unauthenticated_and_ungranted_callers_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let uid = my_uid(dir.path());
    let (core_token, core_hash) = token(dir.path(), "core", 1);
    let (settle_token, settle_hash) = token(dir.path(), "settle", 2);
    let (wrong_token, _) = token(dir.path(), "wrong", 3);
    let mut config = dev_config(dir.path(), uid, &core_hash, &settle_hash);
    // A third caller configured with another user id than this process's.
    config["clients"].as_array_mut().unwrap().push(serde_json::json!({
        "name": "elsewhere", "uids": [uid.wrapping_add(1)], "token_sha256": [core_hash], "keys": ["keyring"]
    }));
    let server = start(&config, Backends::default()).await;
    let d = dir.path().to_path_buf();
    blocking(move || {
        let refused = Err(ClientError::Refused(Refusal::Unauthenticated));
        // A wrong token, another caller's token, an unknown caller, a wrong user id.
        assert_eq!(connect(&d, "core", &wrong_token).map(|_| ()), refused);
        assert_eq!(connect(&d, "core", &settle_token).map(|_| ()), refused);
        assert_eq!(connect(&d, "nobody", &core_token).map(|_| ()), refused);
        assert_eq!(connect(&d, "elsewhere", &core_token).map(|_| ()), refused);
        // A raw connection that never says hello gets nothing.
        let mut raw = std::os::unix::net::UnixStream::connect(d.join("signer.sock")).unwrap();
        signer::proto::write(
            &mut raw,
            &signer::proto::Request {
                key: "keyring".into(),
                op: Op::Public,
            },
        )
        .unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert!(!matches!(
            signer::proto::read::<signer::proto::Response, _>(&mut raw),
            Ok(signer::proto::Response::Ok(_))
        ));
        // Granted keys only.
        let settle = connect(&d, "settle", &settle_token).unwrap();
        assert_eq!(
            settle.call("keyring", Op::KeyRing(KeyRequest::RegistrarKey.encode())),
            Err(ClientError::Refused(Refusal::NotGranted))
        );
        assert_eq!(
            settle.call("no-such-key", Op::Public),
            Err(ClientError::Refused(Refusal::UnknownKey))
        );
        assert!(settle.keys().iter().all(|k| k.id == "lane-1"));
    })
    .await;
    assert!(server.refused.load(std::sync::atomic::Ordering::Relaxed) >= 6);
}

/// `F-035`, the key-rotation runbook: a token rotates by listing two hashes, then removing the
/// old one, which is refused on its very next request; a key rotates by adding a new one,
/// moving the caller to it, and removing the old.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_tokens_and_keys_rotate_online() {
    let dir = tempfile::tempdir().unwrap();
    let uid = my_uid(dir.path());
    let (old_token, old_hash) = token(dir.path(), "core-old", 1);
    let (new_token, new_hash) = token(dir.path(), "core-new", 4);
    let (_, settle_hash) = token(dir.path(), "settle", 2);
    let mut config = dev_config(dir.path(), uid, &old_hash, &settle_hash);
    let server = start(&config, Backends::default()).await;
    let d = dir.path().to_path_buf();
    let (old_client, ot) = (d.clone(), old_token.clone());
    let old = blocking(move || connect(&old_client, "core", &ot).unwrap()).await;
    // Step 1: both tokens.
    config["clients"][0]["token_sha256"] = serde_json::json!([old_hash, new_hash]);
    server
        .reload(&serde_json::from_value(config.clone()).unwrap())
        .await
        .unwrap();
    let (dd, nt) = (d.clone(), new_token.clone());
    let new = blocking(move || connect(&dd, "core", &nt).unwrap()).await;
    let old = blocking(move || {
        assert!(old.call("clerk", Op::Public).is_ok());
        old
    })
    .await;
    // Step 2: the old token removed. The open connection is refused at once.
    config["clients"][0]["token_sha256"] = serde_json::json!([new_hash]);
    server
        .reload(&serde_json::from_value(config.clone()).unwrap())
        .await
        .unwrap();
    let new = blocking(move || {
        assert_eq!(
            old.call("clerk", Op::Public),
            Err(ClientError::Refused(Refusal::Unauthenticated))
        );
        assert!(new.call("clerk", Op::Public).is_ok());
        new
    })
    .await;
    // A key rotates: a second clerk key, the caller moved to it, the old one removed.
    let clerk2 = secret_file(dir.path(), "clerk2", [0x08; 32]);
    config["keys"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(
            { "id": "clerk-2", "scheme": "clerk", "source": { "file": { "path": clerk2 } } }
        ));
    config["clients"][0]["keys"] = serde_json::json!(["keyring", "clerk-2", "agent-authority"]);
    server
        .reload(&serde_json::from_value(config.clone()).unwrap())
        .await
        .unwrap();
    blocking(move || {
        assert_eq!(
            new.call("clerk", Op::Sign(b"m".to_vec())),
            Err(ClientError::Refused(Refusal::NotGranted))
        );
        let sig = new.call("clerk-2", Op::Sign(b"m".to_vec())).unwrap();
        let local = InProcessClerk::from_secret([0x08; 32]).unwrap();
        assert_eq!(sig, local.sign(b"m").to_vec());
    })
    .await;
    // A configuration that does not load leaves the old one in place.
    let mut broken = config.clone();
    broken["clients"][0]["keys"] = serde_json::json!(["missing"]);
    let c: Config = serde_json::from_value(broken).unwrap();
    assert!(server.reload(&c).await.is_err());
    let nt = new_token.clone();
    blocking(move || assert!(connect(&d, "core", &nt).is_ok())).await;
}

/// `F-035`: an adapter that cannot return an error fails closed when the signer refuses.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_refused_custody_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let uid = my_uid(dir.path());
    let (core_token, core_hash) = token(dir.path(), "core", 1);
    let (_, settle_hash) = token(dir.path(), "settle", 2);
    let mut config = dev_config(dir.path(), uid, &core_hash, &settle_hash);
    let server = start(&config, Backends::default()).await;
    let d = dir.path().to_path_buf();
    let custody = blocking(move || {
        let core = Arc::new(connect(&d, "core", &core_token).unwrap());
        RemoteCustody::new(core, "keyring", Patience::TEST).unwrap()
    })
    .await;
    config["clients"][0]["keys"] = serde_json::json!(["clerk"]);
    server
        .reload(&serde_json::from_value(config).unwrap())
        .await
        .unwrap();
    let r = tokio::task::spawn_blocking(move || {
        custody.answer(&KeyRequest::SignAccount([1; 16], b"m".to_vec()))
    })
    .await;
    let panic = r.unwrap_err().into_panic();
    let msg = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(
        msg.contains("ALERT") && msg.contains("fail closed"),
        "{msg}"
    );
}
