// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `NFR-8`, `REQ-AU-8`, `T-016-01`: no key material in the environment, the logs, the errors or
//! the store. Alone in its own test binary, so the subscriber it installs sees every event of
//! the run.

use std::io::Write;
use std::sync::{Arc, Mutex};

use settle::signer::{DevSigner, Signer};
use settle::store::Store;
use settle::testkit::{ADAPTER_TOKEN, Kit, KitOptions, TEST_LANE_SECRET};

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

async fn everything_stored(k: &Kit) -> String {
    let s = &k.store;
    format!(
        "{:?}{:?}{:?}{:?}{:?}{:?}{:?}",
        s.instructions().await.unwrap(),
        s.extrinsics().await.unwrap(),
        s.observations().await.unwrap(),
        s.reports().await.unwrap(),
        s.gaps().await.unwrap(),
        s.alerts().await.unwrap(),
        k.adapter.status(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nfr_8_no_key_material_reaches_the_environment_the_logs_the_errors_or_the_store() {
    let buf = Buffer::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    // A key from a file, as the development signer reads one.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lane.key");
    std::fs::write(&path, hex::encode(TEST_LANE_SECRET)).unwrap();
    let signer = DevSigner::from_file(&path).unwrap();
    let mut errors = vec![format!("{signer:?}")];
    std::fs::write(&path, "not a key").unwrap();
    errors.push(DevSigner::from_file(&path).unwrap_err().to_string());
    errors.push(
        DevSigner::from_file(&dir.path().join("absent"))
            .unwrap_err()
            .to_string(),
    );
    errors.push(
        DevSigner::from_env("ALEDGENT_SETTLE_NO_SUCH_KEY")
            .unwrap_err()
            .to_string(),
    );

    // A run: a withdrawal signed, broadcast, observed and settled; a refusing adapter.
    let mut k = Kit::start().await;
    k.step().await.unwrap();
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    for _ in 0..4 {
        k.step().await.unwrap();
        k.chain.advance(1);
    }
    assert_eq!(k.state(&alice, &op).await, "Settled");
    let mut refusing = Kit::start_with(KitOptions {
        registered: false,
        ..KitOptions::default()
    })
    .await;
    assert!(!refusing.step().await.unwrap().serving);

    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("signed and stored"), "the adapter logged");
    let stored = everything_stored(&k).await + &everything_stored(&refusing).await;
    let environment: String = std::env::vars()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect();
    let secret = hex::encode(TEST_LANE_SECRET);
    let public = hex::encode(signer.public());
    for (what, text) in [
        ("the log", &log),
        ("the store", &stored),
        ("the errors", &errors.join("\n")),
        ("the environment", &environment),
    ] {
        // The header words are checked where the service writes; the environment is the runner's
        // too, and CI puts the branch name there (`GITHUB_HEAD_REF`), so only key material counts.
        let header_words: &[&str] = if what == "the environment" {
            &[]
        } else {
            &["Bearer", "authorization"]
        };
        for needle in [secret.as_str(), &secret.to_uppercase(), ADAPTER_TOKEN]
            .into_iter()
            .chain(header_words.iter().copied())
        {
            assert!(!text.contains(needle), "{needle:?} reached {what}");
        }
    }
    // The public key is not secret, but nothing needs it either: the account is what shows.
    assert!(!log.contains(&public));
}
