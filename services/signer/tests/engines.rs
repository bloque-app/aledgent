// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The common suite against the sidecar and in-process engines (`F-042`), and the rules of the
//! interface that are not an engine's own: which patience a key of an API provider may be used under.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::suite::{Case, Setup, run};
use common::*;
use ledger_core::anchor::clerk::{ClerkSigner, InProcessClerk};
use ledger_kernel::keys::RootKeys;
use serde_json::json;
use signer::backend::Backends;
use signer::client::{ClientError, OnFailure, Patience, RemoteEd25519, SignerClient, Token};
use signer::config::Config;
use signer::engine::in_process::InProcessEngine;
use signer::engine::sidecar::SidecarEngine;
use signer::engine::{Engine, EngineKind, KeyHandle};
use signer::proto::{Op, Scheme};
use signer::server::Server;

fn expected() -> [(Scheme, Vec<u8>); 4] {
    [
        (
            Scheme::KeyRing,
            RootKeys::new(ROOT).registrar_key().0.to_vec(),
        ),
        (
            Scheme::Clerk,
            InProcessClerk::from_secret(CLERK)
                .unwrap()
                .public_key()
                .to_vec(),
        ),
        (
            Scheme::Ed25519,
            ed25519_dalek::SigningKey::from_bytes(&AUTHORITY)
                .verifying_key()
                .to_bytes()
                .to_vec(),
        ),
        (
            Scheme::Secp256k1,
            k256::ecdsa::SigningKey::from_slice(&LANE)
                .unwrap()
                .verifying_key()
                .to_encoded_point(true)
                .as_bytes()
                .to_vec(),
        ),
    ]
}

/// The sidecar: every scheme through the signer on its socket.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_the_sidecar_engine_passes_the_common_suite() {
    let dir = tempfile::tempdir().unwrap();
    let uid = my_uid(dir.path());
    let (core_token, core_hash) = token(dir.path(), "core", 1);
    let (_, settle_hash) = token(dir.path(), "settle", 2);
    let mut config = dev_config(dir.path(), uid, &core_hash, &settle_hash);
    // One caller that may use all four keys.
    config["clients"][0]["keys"] = json!(["keyring", "clerk", "agent-authority", "lane-1"]);
    let c: Config = serde_json::from_value(config).unwrap();
    let server = Server::load(&c, Backends::default()).await.unwrap();
    let l = server.bind().unwrap();
    tokio::spawn(Arc::clone(&server).serve(l));
    let sock = dir.path().join("signer.sock");
    tokio::task::spawn_blocking(move || {
        let client = SignerClient::with_token(
            &sock,
            "core",
            Token::from_file(&core_token).unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();
        let engine = SidecarEngine::new(Arc::new(client));
        let ids = ["keyring", "clerk", "agent-authority", "lane-1"];
        let cases = expected()
            .into_iter()
            .zip(ids)
            .map(|((scheme, public), id)| Case {
                scheme,
                locator: json!({ "id": id }),
                public,
            })
            .collect();
        run(&Setup {
            engine: &engine,
            cases,
            missing: json!({ "id": "no-such-key" }),
            secret_marker: "5a5a5a5a",
        });
    })
    .await
    .unwrap();
}

/// In-process: every scheme from a key file.
#[test]
fn nfr_8_the_in_process_engine_passes_the_common_suite() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        secret_file(dir.path(), "root", ROOT),
        secret_file(dir.path(), "clerk", CLERK),
        secret_file(dir.path(), "authority", AUTHORITY),
        secret_file(dir.path(), "lane", LANE),
    ];
    let cases = expected()
        .into_iter()
        .zip(files)
        .map(|((scheme, public), file)| Case {
            scheme,
            locator: json!({ "file": file }),
            public,
        })
        .collect();
    run(&Setup {
        engine: &InProcessEngine,
        cases,
        missing: json!({ "file": dir.path().join("absent.hex") }),
        secret_marker: "5a5a5a5a",
    });
    assert_eq!(InProcessEngine.kind(), EngineKind::InProcess);
}

/// There is no environment locator: an `env` field is refused, and so is anything else.
#[test]
fn nfr_8_an_in_process_key_is_never_read_from_the_environment() {
    // SAFETY-free: the variable is only named, never set; the locator has no such field.
    for locator in [
        json!({ "env": "FEED_KEY" }),
        json!({ "file": "/k", "env": "X" }),
        json!({}),
    ] {
        let e = InProcessEngine.open(Scheme::Ed25519, &locator).unwrap_err();
        assert!(matches!(e, ClientError::Config(_)), "{e:?}");
    }
}

/// A key held by an API provider is used through `try_*`: a patience that stops the process is refused.
#[test]
fn nfr_8_a_key_of_an_api_provider_is_refused_under_a_patience_that_exits() {
    #[derive(Debug)]
    struct Remote;
    impl KeyHandle for Remote {
        fn kind(&self) -> EngineKind {
            EngineKind::ApiProvider
        }
        fn scheme(&self) -> Scheme {
            Scheme::Ed25519
        }
        fn public(&self) -> Vec<u8> {
            ed25519_dalek::SigningKey::from_bytes(&[1; 32])
                .verifying_key()
                .to_bytes()
                .to_vec()
        }
        fn call(&self, _: &Op) -> Result<Vec<u8>, ClientError> {
            Err(ClientError::Io("down".into()))
        }
    }
    let h: Arc<dyn KeyHandle> = Arc::new(Remote);
    let e = RemoteEd25519::from_key(Arc::clone(&h), Patience::PRODUCTION)
        .err()
        .unwrap();
    assert!(
        matches!(&e, ClientError::Config(t) if t.contains("try_*")),
        "{e:?}"
    );
    let ok = Patience {
        wait: Duration::ZERO,
        on_failure: OnFailure::Panic,
    };
    assert!(RemoteEd25519::from_key(h, ok).is_ok());
}
