// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The signer's configuration: one JSON file, no secret in it.
//!
//! ```json
//! {
//!   "listen": "/run/aledgent-signer/signer.sock",
//!   "socket_mode": "660",
//!   "clients": [
//!     { "name": "core", "uids": [990], "token_sha256": ["<hex>"],
//!       "keys": ["keyring", "clerk", "agent-authority", "agent-package"] }
//!   ],
//!   "vault": {
//!     "address": "https://vault.example.com",
//!     "auth": { "aws_iam": { "mount": "aledgent-signer", "role": "<core signer role>",
//!                            "region": "<region>", "server_id": "vault.example.com" } }
//!   },
//!   "keys": [
//!     { "id": "keyring", "scheme": "key_ring", "source": { "vault": { "path": "secret/<core signer secret>", "field": "keyring" } } },
//!     { "id": "clerk", "scheme": "clerk", "source": { "vault": { "path": "secret/<core signer secret>", "field": "clerk" } } }
//!   ]
//! }
//! ```
//!
//! That is the core node's signer. The adapter's (a sidecar in its task) is the same shape: the
//! client `settle` with its keys `lane-1` and `anchor` from `secret/<settle signer secret>`, under
//! the role `<settle signer role>`, or a lane key from AWS KMS if opted in
//! (`{ "kms": { "key_id": "arn:aws:kms:…" } }`).
//!
//! - A **client** is authenticated by its peer user id (one of `uids`) and its token (one of
//!   `token_sha256`, the SHA-256 of the token file's trimmed bytes). Two hashes at once is how a
//!   token rotates. It may use only the keys it lists.
//! - A **key source** is `vault` (the default, owner ruling `key-custody-opt-in-kms`: a field of a
//!   KV v1 secret, 32 bytes of hex, which the signer reads itself at start after logging in to
//!   Vault with AWS IAM auth, so that no secret is in its environment), `kms` (opt-in, secp256k1
//!   lane and anchor keys only: the key is AWS KMS's and never leaves it), or `file` (a hex secret
//!   in a file: development and tests only, and refused unless `"development": true`).
//! - **Vault's login** is `aws_iam` (the role of the instance or the ECS task signs an STS
//!   `GetCallerIdentity`; nothing secret is configured; its `region` is required) or `token_file` (a Vault token in a file:
//!   development and tests only, refused unless `"development": true`).
//!
//! Every reference is checked at load: a client granted a key that does not exist, a duplicate
//! identifier, a token hash that is not 32 bytes of hex, a client with no user id. A
//! configuration that does not check does not start (`REQ-AU-8`: never a default).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::proto::Scheme;

/// The configuration file (module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The Unix socket to listen on.
    pub listen: PathBuf,
    /// Its permission bits, octal (default `660`): the callers' group may connect, no one else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_mode: Option<String>,
    /// Who may call.
    pub clients: Vec<ClientConfig>,
    /// The keys.
    pub keys: Vec<KeyConfig>,
    /// Whether `file` key sources are allowed: development and tests only.
    #[serde(default)]
    pub development: bool,
    /// The AWS region of the KMS keys. Required when a key's source is `kms`; there is no
    /// default, and the environment's region is not consulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_region: Option<String>,
    /// Vault, where `vault` key sources are read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<VaultConfig>,
}

/// Where Vault is and how the signer logs in (module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    /// Its address, `https://…` (an `http://` address is for development only).
    pub address: String,
    /// How the signer logs in.
    pub auth: VaultAuth,
}

/// How the signer logs in to Vault.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum VaultAuth {
    /// Vault's AWS auth method, `iam` type: an STS `GetCallerIdentity` signed with the process's
    /// AWS credentials (the instance's or the ECS task's role), which Vault replays to STS.
    AwsIam {
        /// The auth method's mount (`auth/<mount>/login`).
        mount: String,
        /// The Vault role, bound to the instance or task role's ARN.
        role: String,
        /// The STS region the login is signed for. Required: there is no default, and a
        /// configuration without it does not start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        /// The `X-Vault-AWS-IAM-Server-ID` value, if the mount requires one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_id: Option<String>,
    },
    /// A Vault token in a file: development and tests only.
    TokenFile {
        /// The file.
        path: PathBuf,
    },
}

/// One caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// Its name, as its [`crate::client::SignerEndpoint::client`] says.
    pub name: String,
    /// The user ids its process may run as (the socket's peer credentials).
    pub uids: Vec<u32>,
    /// The SHA-256 of each token it may present, hex.
    pub token_sha256: Vec<String>,
    /// The keys it may use.
    pub keys: Vec<String>,
}

/// One key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// Its identifier.
    pub id: String,
    /// Its scheme.
    pub scheme: SchemeName,
    /// Where it comes from.
    pub source: KeySource,
}

/// [`Scheme`], as the file names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemeName {
    /// [`Scheme::KeyRing`].
    KeyRing,
    /// [`Scheme::Clerk`].
    Clerk,
    /// [`Scheme::Ed25519`].
    Ed25519,
    /// [`Scheme::Secp256k1`].
    Secp256k1,
}

impl From<SchemeName> for Scheme {
    fn from(s: SchemeName) -> Scheme {
        match s {
            SchemeName::KeyRing => Scheme::KeyRing,
            SchemeName::Clerk => Scheme::Clerk,
            SchemeName::Ed25519 => Scheme::Ed25519,
            SchemeName::Secp256k1 => Scheme::Secp256k1,
        }
    }
}

/// Where a key comes from (module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KeySource {
    /// A field of a KV v1 secret in Vault (its value under `.data`), 32 bytes of hex: the
    /// default.
    Vault {
        /// The secret's path, mount included (`secret/<core signer secret>`).
        path: String,
        /// The field.
        field: String,
    },
    /// An AWS KMS `ECC_SECG_P256K1` key (opt-in, secp256k1 only). The key never leaves KMS.
    Kms {
        /// Its key id, ARN or alias.
        key_id: String,
    },
    /// A 32-byte secret in a file, hex: development and tests only.
    File {
        /// The file.
        path: PathBuf,
    },
}

impl Config {
    /// Read and check the file at `path`.
    pub fn load(path: &std::path::Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let c: Config =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        c.check()?;
        Ok(c)
    }

    /// Check every reference (module documentation).
    pub fn check(&self) -> Result<(), String> {
        let mut ids = BTreeSet::new();
        for k in &self.keys {
            if k.id.is_empty() || !ids.insert(k.id.as_str()) {
                return Err(format!("key {:?}: empty or listed twice", k.id));
            }
            if matches!(k.source, KeySource::File { .. }) && !self.development {
                return Err(format!(
                    "key {}: a key from a file is for development only (NFR-8); set \
                     \"development\": true to allow it",
                    k.id
                ));
            }
            if matches!(k.source, KeySource::Kms { .. }) && k.scheme != SchemeName::Secp256k1 {
                return Err(format!(
                    "key {}: AWS KMS is an opt-in for secp256k1 lane and anchor keys only; keep \
                     a {:?} key in Vault",
                    k.id, k.scheme
                ));
            }
            if matches!(k.source, KeySource::Vault { .. }) && self.vault.is_none() {
                return Err(format!("key {}: a Vault key needs \"vault\"", k.id));
            }
            if matches!(k.source, KeySource::Kms { .. }) && !has_region(&self.aws_region) {
                return Err(format!(
                    "key {}: an AWS KMS key needs \"aws_region\", the region of the KMS keys \
                     (e.g. \"us-east-1\"); there is no default",
                    k.id
                ));
            }
        }
        let mut names = BTreeSet::new();
        for c in &self.clients {
            if c.name.is_empty() || !names.insert(c.name.as_str()) {
                return Err(format!("client {:?}: empty or listed twice", c.name));
            }
            if c.uids.is_empty() {
                return Err(format!("client {}: no user id (peer credentials)", c.name));
            }
            if c.token_sha256.is_empty() {
                return Err(format!("client {}: no token", c.name));
            }
            for t in &c.token_sha256 {
                if hex::decode(t).map(|b| b.len()) != Ok(32) {
                    return Err(format!(
                        "client {}: token_sha256 is 32 bytes of hex",
                        c.name
                    ));
                }
            }
            for k in &c.keys {
                if !ids.contains(k.as_str()) {
                    return Err(format!("client {}: no key {k}", c.name));
                }
            }
        }
        if let Some(v) = &self.vault {
            if matches!(v.auth, VaultAuth::TokenFile { .. }) && !self.development {
                return Err(
                    "vault: a token in a file is for development only (NFR-8); use aws_iam, or \
                     set \"development\": true"
                        .into(),
                );
            }
            if !v.address.starts_with("https://") && !self.development {
                return Err("vault: the address is https://".into());
            }
            if let VaultAuth::AwsIam { region, .. } = &v.auth
                && !has_region(region)
            {
                return Err(
                    "vault.auth.aws_iam: \"region\" is required, the AWS region the STS login is \
                     signed for (e.g. \"us-east-1\"); there is no default"
                        .into(),
                );
            }
        }
        if let Some(m) = &self.socket_mode {
            u32::from_str_radix(m, 8).map_err(|_| format!("socket_mode {m}: octal"))?;
        }
        Ok(())
    }

    /// The socket's permission bits.
    pub fn mode(&self) -> u32 {
        self.socket_mode
            .as_deref()
            .and_then(|m| u32::from_str_radix(m, 8).ok())
            .unwrap_or(0o660)
    }
}

/// A region is set and not blank.
fn has_region(region: &Option<String>) -> bool {
    region.as_deref().is_some_and(|r| !r.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "listen": "/tmp/s.sock",
            "clients": [{ "name": "core", "uids": [1], "token_sha256": [hex::encode([1u8; 32])], "keys": ["k"] }],
            "keys": [{ "id": "k", "scheme": "key_ring", "source": { "file": { "path": "/x" } } }],
            "development": true
        }))
        .unwrap()
    }

    /// The deployment template (`deploy/templates/signer.json`), its numbers filled: it checks,
    /// and it holds the core's four keys and ingress's feed key, which the template's ingress.json
    /// names.
    #[test]
    fn the_canary_template_checks() {
        let text = include_str!("../../../deploy/templates/signer.json");
        let mut filled = String::new();
        let mut rest = text;
        while let Some(at) = rest.find("\"<FILL-NUMBER") {
            filled.push_str(&rest[..at]);
            let end = rest[at + 1..].find('"').map(|e| at + e + 2).unwrap();
            filled.push('1');
            rest = &rest[end..];
        }
        filled.push_str(rest);
        let mut c: Config = serde_json::from_str(&filled).unwrap();
        for client in &mut c.clients {
            client.token_sha256 = vec![hex::encode([0u8; 32])];
        }
        assert_eq!(c.check(), Ok(()));
        // Owner ruling `key-custody-opt-in-kms`: the keys come from Vault, read by the signer
        // under AWS IAM auth; nothing is a development key.
        assert!(!c.development);
        assert!(matches!(
            c.vault.as_ref().map(|v| &v.auth),
            Some(VaultAuth::AwsIam { .. })
        ));
        assert!(
            c.keys
                .iter()
                .all(|k| matches!(k.source, KeySource::Vault { .. }))
        );
        let ids: Vec<&str> = c.keys.iter().map(|k| k.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "keyring",
                "clerk",
                "agent-authority",
                "agent-package",
                "ingress-feed"
            ]
        );
        // F-038 §5: ingress signs its feeds with its own key, the only one its client may use.
        let ingress = c.clients.iter().find(|c| c.name == "ingress").unwrap();
        assert_eq!(ingress.keys, ["ingress-feed"]);
    }

    #[test]
    fn req_au_8_a_configuration_that_does_not_check_does_not_start() {
        assert_eq!(config().check(), Ok(()));
        let mut c = config();
        c.development = false;
        assert!(c.check().unwrap_err().contains("development only"));
        let mut c = config();
        c.clients[0].uids.clear();
        assert!(c.check().unwrap_err().contains("peer credentials"));
        let mut c = config();
        c.clients[0].token_sha256 = vec!["abc".into()];
        assert!(c.check().is_err());
        let mut c = config();
        c.clients[0].keys.push("missing".into());
        assert!(c.check().unwrap_err().contains("no key missing"));
        // KMS is an opt-in for secp256k1 only.
        let mut c = config();
        c.keys[0].source = KeySource::Kms { key_id: "x".into() };
        assert!(c.check().unwrap_err().contains("secp256k1"));
        let mut c = config();
        c.keys[0].scheme = SchemeName::Secp256k1;
        c.keys[0].source = KeySource::Kms { key_id: "x".into() };
        // REQ-AU-8: never a default. A KMS key without a region does not start.
        assert!(c.check().unwrap_err().contains("\"aws_region\""));
        c.aws_region = Some("us-east-1".into());
        assert_eq!(c.check(), Ok(()));
        // A Vault key needs Vault; a Vault token in a file is for development only.
        let mut c = config();
        c.keys[0].source = KeySource::Vault {
            path: "secret/x".into(),
            field: "k".into(),
        };
        assert!(c.check().unwrap_err().contains("needs \"vault\""));
        c.vault = Some(VaultConfig {
            address: "https://vault.example".into(),
            auth: VaultAuth::AwsIam {
                mount: "aledgent-signer".into(),
                role: "aledgent-signer-iam".into(),
                region: None,
                server_id: None,
            },
        });
        c.development = false;
        // REQ-AU-8: never a default. An IAM login without a region does not start.
        assert!(c.check().unwrap_err().contains("\"region\" is required"));
        if let Some(VaultConfig {
            auth: VaultAuth::AwsIam { region, .. },
            ..
        }) = c.vault.as_mut()
        {
            *region = Some(" ".into());
        }
        assert!(c.check().unwrap_err().contains("\"region\" is required"));
        if let Some(VaultConfig {
            auth: VaultAuth::AwsIam { region, .. },
            ..
        }) = c.vault.as_mut()
        {
            *region = Some("us-east-1".into());
        }
        assert_eq!(c.check(), Ok(()));
        c.vault.as_mut().unwrap().auth = VaultAuth::TokenFile { path: "/t".into() };
        assert!(c.check().unwrap_err().contains("development only"));
        let mut c = config();
        c.keys.push(c.keys[0].clone());
        assert!(c.check().is_err());
        assert_eq!(config().mode(), 0o660);
    }
}
