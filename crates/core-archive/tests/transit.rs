// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The seal against a real Vault (`F-044`): the writer wraps file keys to the public half of a
//! Transit `rsa-4096` key with no Vault call, and Vault's own `decrypt` opens them, per version
//! after a rotation, under a policy that allows `decrypt` on that key and nothing else.
//!
//! CI runs it in the `vault transit (F-042)` job against Vault 2.0.3 in dev mode
//! (`.github/workflows/ci.yml`); elsewhere it is skipped unless `ALEDGENT_TEST_VAULT` names a dev
//! Vault and `ALEDGENT_TEST_VAULT_TOKEN` its root token. It mounts Transit at a path of its own and
//! removes it at the end.

mod common;

use std::time::Duration;

use core_archive::age::Identity;
use core_archive::seal::{
    Keys, Recipients, Sealed, TokenSource, TransitKey, TransitUnwrap as _, Unsealed, VaultTransit,
};
use core_archive::store::{FsStore, ObjectStore as _, StoreError};
use serde_json::{Value, json};
use zeroize::Zeroizing;

struct Vault {
    address: String,
    token: String,
    agent: ureq::Agent,
}

impl Vault {
    fn from_env() -> Option<Vault> {
        match (
            std::env::var("ALEDGENT_TEST_VAULT"),
            std::env::var("ALEDGENT_TEST_VAULT_TOKEN"),
        ) {
            (Ok(address), Ok(token)) if !address.is_empty() && !token.is_empty() => Some(Vault {
                address: address.trim_end_matches('/').to_owned(),
                token,
                agent: ureq::Agent::new_with_config(
                    ureq::Agent::config_builder()
                        .http_status_as_error(false)
                        .timeout_global(Some(Duration::from_secs(60)))
                        .build(),
                ),
            }),
            _ => {
                assert!(
                    std::env::var("ALEDGENT_REQUIRE_VAULT_TEST").is_err(),
                    "ALEDGENT_TEST_VAULT and ALEDGENT_TEST_VAULT_TOKEN are required in this job"
                );
                eprintln!("skipped: ALEDGENT_TEST_VAULT is not set");
                None
            }
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let url = format!("{}/v1/{path}", self.address);
        let mut resp = match (method, body) {
            ("GET", _) => self
                .agent
                .get(&url)
                .header("x-vault-token", &self.token)
                .call(),
            ("DELETE", _) => self
                .agent
                .delete(&url)
                .header("x-vault-token", &self.token)
                .call(),
            (_, b) => self
                .agent
                .post(&url)
                .header("x-vault-token", &self.token)
                .send(b.unwrap_or(json!({})).to_string()),
        }
        .unwrap();
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap();
        assert!(status < 300, "{method} {path}: {status} {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    fn public_key(&self, mount: &str, version: u32) -> String {
        let k = self.call("GET", &format!("{mount}/keys/archive"), None);
        k["data"]["keys"][version.to_string()]["public_key"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn token_with(&self, policy: &str, rules: &str) -> Zeroizing<String> {
        self.call(
            "POST",
            &format!("sys/policies/acl/{policy}"),
            Some(json!({ "policy": rules })),
        );
        let t = self.call(
            "POST",
            "auth/token/create",
            Some(json!({ "policies": [policy], "ttl": "10m", "no_default_policy": true })),
        );
        Zeroizing::new(t["auth"]["client_token"].as_str().unwrap().to_owned())
    }
}

fn record(n: u64) -> Vec<u8> {
    format!("record {n}: entries and replies").into_bytes()
}

#[test]
fn f_044_vault_transit_opens_what_the_writer_sealed_to_its_public_key() {
    let Some(vault) = Vault::from_env() else {
        return;
    };
    let mount = format!(
        "transit-archive-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    vault.call(
        "POST",
        &format!("sys/mounts/{mount}"),
        Some(json!({ "type": "transit" })),
    );
    vault.call(
        "POST",
        &format!("{mount}/keys/archive"),
        Some(json!({ "type": "rsa-4096", "exportable": false })),
    );
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let writer = |version: u32| {
        let transit =
            TransitKey::new("archive", version, &vault.public_key(&mount, version)).unwrap();
        Sealed::new(
            Box::new(FsStore::open(tmp.path()).unwrap()),
            Some(Recipients::new(transit, vec![recovery.recipient()]).unwrap()),
            Keys::none(),
            Unsealed::Refuse,
        )
    };
    // v1 seals 1..=3; Vault never sees the writer.
    let w1 = writer(1);
    for n in 1..=3 {
        w1.put_once(&format!("segments/{n}.seg"), &record(n))
            .unwrap();
    }
    // A rotation: v2 seals 4.
    vault.call("POST", &format!("{mount}/keys/archive/rotate"), None);
    writer(2).put_once("segments/4.seg", &record(4)).unwrap();

    // A reader whose token may decrypt with this key and do nothing else, read from a file as
    // Vault Agent's sink leaves it.
    let decrypt_only = vault.token_with(
        &format!("{mount}-restore"),
        &format!("path \"{mount}/decrypt/archive\" {{ capabilities = [\"update\"] }}"),
    );
    let token_file = tmp.path().join("vault-token");
    std::fs::write(&token_file, decrypt_only.as_bytes()).unwrap();
    let reader = |token: TokenSource| {
        Sealed::new(
            Box::new(FsStore::open(tmp.path()).unwrap()),
            None,
            Keys {
                transit: Some(Box::new(
                    VaultTransit::new(&vault.address, &mount, token).unwrap(),
                )),
                recovery: Vec::new(),
            },
            Unsealed::Refuse,
        )
    };
    let r = reader(TokenSource::File(token_file.clone()));
    for n in 1..=4 {
        assert_eq!(
            r.get(&format!("segments/{n}.seg"), 1 << 20)
                .unwrap()
                .unwrap(),
            record(n),
            "segment {n}"
        );
    }

    // A token without decrypt is denied, and says so without the token.
    let other = vault.token_with(
        &format!("{mount}-none"),
        &format!("path \"{mount}/keys/archive\" {{ capabilities = [\"read\"] }}"),
    );
    let e = reader(TokenSource::Fixed(other.clone()))
        .get("segments/1.seg", 1 << 20)
        .unwrap_err();
    assert!(
        matches!(&e, StoreError::Sealed(m) if m.contains("denied") && !m.contains(other.as_str())),
        "{e}"
    );

    // Raising min_decryption_version loses v1 through Vault: the infrastructure never does it
    // (NFR-22). The recovery key still opens v1's objects.
    vault.call(
        "POST",
        &format!("{mount}/keys/archive/config"),
        Some(json!({ "min_decryption_version": 2 })),
    );
    let fresh = reader(TokenSource::File(token_file));
    assert!(matches!(
        fresh.get("segments/1.seg", 1 << 20),
        Err(StoreError::Sealed(_))
    ));
    assert_eq!(
        fresh.get("segments/4.seg", 1 << 20).unwrap().unwrap(),
        record(4)
    );
    let by_recovery = Sealed::new(
        Box::new(FsStore::open(tmp.path()).unwrap()),
        None,
        Keys {
            transit: None,
            recovery: vec![Identity::parse(&recovery.to_bech32()).unwrap()],
        },
        Unsealed::Refuse,
    );
    assert_eq!(
        by_recovery.get("segments/1.seg", 1 << 20).unwrap().unwrap(),
        record(1)
    );

    // Vault not answering is a retryable failure, not a refusal.
    let down = VaultTransit::new(
        "http://127.0.0.1:9",
        &mount,
        TokenSource::Fixed(decrypt_only.clone()),
    )
    .unwrap();
    assert!(matches!(
        down.unwrap("archive", 1, &[0u8; 512]),
        Err(StoreError::Unavailable(_))
    ));

    vault.call("DELETE", &format!("sys/mounts/{mount}"), None);
}
