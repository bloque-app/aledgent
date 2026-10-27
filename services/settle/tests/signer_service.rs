// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-035`: the adapter's lane key behind the signer service. The adapter holds the key's
//! identifier and public half; each signature comes from the signer's process and recovers to
//! the key, as the chain checks it. `F-044`: a native account's Ed25519 key, the same way.

use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;
use std::time::Duration;

use settle::signer::{EngineEd25519, PassAccount, ServiceSigner, Signature, Signer, recover};
use sha2::Digest as _;

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_a_lane_signs_through_the_signer_service() {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join(".uid");
    std::fs::write(&probe, b"").unwrap();
    let uid = std::fs::metadata(&probe).unwrap().uid();
    let token = hex::encode([6u8; 32]);
    let token_file = dir.path().join("settle.token");
    std::fs::write(&token_file, &token).unwrap();
    let lane = dir.path().join("lane.hex");
    std::fs::write(&lane, hex::encode([0x11u8; 32])).unwrap();
    let config: signer::config::Config = serde_json::from_value(serde_json::json!({
        "listen": dir.path().join("signer.sock"),
        "development": true,
        "clients": [{ "name": "settle", "uids": [uid], "token_sha256": [sha256_hex(token.as_bytes())], "keys": ["lane-1"] }],
        "keys": [{ "id": "lane-1", "scheme": "secp256k1", "source": { "file": { "path": lane } } }]
    }))
    .unwrap();
    let server = signer::server::Server::load(&config, signer::backend::Backends::default())
        .await
        .unwrap();
    let listener = server.bind().unwrap();
    tokio::spawn(Arc::clone(&server).serve(listener));
    let endpoint = signer::client::SignerEndpoint {
        socket: dir.path().join("signer.sock"),
        client: "settle".into(),
        token_file,
        timeout_ms: Some(5_000),
    };
    let client = tokio::task::spawn_blocking(move || {
        signer::client::SignerClient::connect(&endpoint).unwrap()
    })
    .await
    .unwrap();
    let device = ServiceSigner::new(Arc::new(client), "lane-1").unwrap();
    let local = settle::signer::DevSigner::from_secret(&[0x11; 32]).unwrap();
    assert_eq!(device.public(), local.public());
    let pass: Arc<dyn Signer> = Arc::new(PassAccount::new(Arc::new(device), [3; 32]));
    let message = b"a signer payload".to_vec();
    let sig = tokio::time::timeout(Duration::from_secs(10), pass.sign(message.clone()))
        .await
        .unwrap()
        .unwrap();
    let Signature::Ecdsa(sig) = sig else {
        panic!("a PASS device signs ECDSA");
    };
    // The device signs the message's blake2b-256, as MultiSignature::Ecdsa verifies it.
    let digest = settle::chain::blake2_256(&message);
    assert_eq!(
        recover(&digest, &sig).map(settle::signer::Public::Ecdsa),
        Some(local.public())
    );
    assert_eq!(
        server.answered.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

/// `F-044`: a native account's Ed25519 key on the signer service, in the form written before
/// `F-042` (`{ "signer": { "key": … } }`). Its account is its public key, and its signature over the
/// message's own bytes verifies against that account.
#[tokio::test(flavor = "multi_thread")]
async fn f_044_a_native_lane_signs_through_the_signer_service() {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join(".uid");
    std::fs::write(&probe, b"").unwrap();
    let uid = std::fs::metadata(&probe).unwrap().uid();
    let token = hex::encode([7u8; 32]);
    let token_file = dir.path().join("settle.token");
    std::fs::write(&token_file, &token).unwrap();
    let lane = dir.path().join("lane.hex");
    std::fs::write(&lane, hex::encode([0x12u8; 32])).unwrap();
    let config: signer::config::Config = serde_json::from_value(serde_json::json!({
        "listen": dir.path().join("signer.sock"),
        "development": true,
        "clients": [{ "name": "settle", "uids": [uid], "token_sha256": [sha256_hex(token.as_bytes())], "keys": ["lane-1"] }],
        "keys": [{ "id": "lane-1", "scheme": "ed25519", "source": { "file": { "path": lane } } }]
    }))
    .unwrap();
    let server = signer::server::Server::load(&config, signer::backend::Backends::default())
        .await
        .unwrap();
    let listener = server.bind().unwrap();
    tokio::spawn(Arc::clone(&server).serve(listener));
    let endpoint = signer::client::SignerEndpoint {
        socket: dir.path().join("signer.sock"),
        client: "settle".into(),
        token_file,
        timeout_ms: Some(5_000),
    };
    let client = tokio::task::spawn_blocking(move || {
        signer::client::SignerClient::connect(&endpoint).unwrap()
    })
    .await
    .unwrap();
    let lane: Arc<dyn Signer> =
        Arc::new(EngineEd25519::from_service(Arc::new(client), "lane-1").unwrap());
    let local = settle::signer::DevEd25519::from_secret(&[0x12; 32]);
    assert_eq!(lane.public(), local.public());
    assert_eq!(lane.account(), local.account());
    assert_eq!(lane.pass_user(), None);
    let message = b"a signer payload".to_vec();
    let sig = tokio::time::timeout(Duration::from_secs(10), lane.sign(message.clone()))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(sig, Signature::Ed25519(_)));
    assert!(sig.verify(&message, &lane.account()));
    assert!(!sig.verify(b"another payload", &lane.account()));
}
