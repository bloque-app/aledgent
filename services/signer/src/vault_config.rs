// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Where Vault is and how a process logs in to it, as a configuration file names it: the signer's
//! (`vault`, where it reads its KV keys) and a component's own (`transit`, where it signs through
//! Vault Transit, `F-042`). Plain data and its checks; the client is `crate::vault_session`
//! (feature `vault-session`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where Vault is and how the process logs in (module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    /// Its address, `https://…` (an `http://` address is for development only).
    pub address: String,
    /// How the process logs in.
    pub auth: VaultAuth,
}

/// How a process logs in to Vault.
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

impl VaultConfig {
    /// Check the address and the login. `development` allows a token in a file and an `http://`
    /// address.
    pub fn check(&self, development: bool) -> Result<(), String> {
        if matches!(self.auth, VaultAuth::TokenFile { .. }) && !development {
            return Err(
                "vault: a token in a file is for development only (NFR-8); use aws_iam, or \
                 set \"development\": true"
                    .into(),
            );
        }
        if !self.address.starts_with("https://") && !development {
            return Err("vault: the address is https://".into());
        }
        if let VaultAuth::AwsIam { region, .. } = &self.auth
            && !region.as_deref().is_some_and(|r| !r.trim().is_empty())
        {
            return Err(
                "vault.auth.aws_iam: \"region\" is required, the AWS region the STS login is \
                 signed for (e.g. \"us-east-1\"); there is no default"
                    .into(),
            );
        }
        Ok(())
    }
}

/// An Ed25519 key in Vault Transit, used in place by the component that signs (`F-042`): the
/// alternative to a key behind the signer service. The key never leaves Vault (it is not
/// exportable) and each signature is a call to Vault.
///
/// ```json
/// {
///   "vault": { "address": "https://vault.example.com",
///              "auth": { "aws_iam": { "mount": "aws", "role": "feed-signer",
///                                     "region": "us-east-1", "server_id": "vault.example.com" } } },
///   "mount": "transit",
///   "key": "lane-1",
///   "version": 1
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitConfig {
    /// Vault, and the login.
    pub vault: VaultConfig,
    /// The Transit engine's mount (`transit`).
    pub mount: String,
    /// The key's name in that mount. It must be an `ed25519` key and not exportable.
    pub key: String,
    /// The key version to sign with and publish. Required, never zero, never "the latest": a new
    /// version is a new public key, so a rotation is a decision, and it is made in this file.
    #[serde(default)]
    pub version: u32,
}

fn name_ok(s: &str, slash: bool) -> bool {
    !s.is_empty()
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        })
        && (slash || !s.contains('/'))
}

impl TransitConfig {
    /// Check the key's address, the pinned version and Vault's login.
    pub fn check(&self, development: bool) -> Result<(), String> {
        if !name_ok(&self.mount, true) {
            return Err(format!(
                "transit.mount {:?}: letters, digits, - _ . and / between names",
                self.mount
            ));
        }
        if !name_ok(&self.key, false) {
            return Err(format!(
                "transit.key {:?}: letters, digits, - _ and .",
                self.key
            ));
        }
        if self.version == 0 {
            return Err(
                "transit.version is required and not zero: pin the key version to sign with \
                 (a new version is a new public key)"
                    .into(),
            );
        }
        self.vault.check(development)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TransitConfig {
        serde_json::from_value(serde_json::json!({
            "vault": { "address": "https://vault.example",
                       "auth": { "aws_iam": { "mount": "aws", "role": "r", "region": "us-east-1" } } },
            "mount": "transit", "key": "lane-1", "version": 1
        }))
        .unwrap()
    }

    #[test]
    fn nfr_8_a_transit_key_names_a_pinned_version_and_a_safe_address() {
        assert_eq!(config().check(false), Ok(()));
        let mut c = config();
        c.version = 0;
        assert!(c.check(false).unwrap_err().contains("pin the key version"));
        // A missing version is zero.
        let missing: TransitConfig = serde_json::from_value(serde_json::json!({
            "vault": { "address": "https://v", "auth": { "token_file": { "path": "/t" } } },
            "mount": "transit", "key": "k"
        }))
        .unwrap();
        assert_eq!(missing.version, 0);
        for (mount, key) in [
            ("", "k"),
            ("transit/../sys", "k"),
            ("transit", "a/b"),
            ("transit", "k?x"),
            ("transit", ""),
            ("/transit", "k"),
        ] {
            let mut c = config();
            c.mount = mount.into();
            c.key = key.into();
            assert!(c.check(false).is_err(), "{mount:?} {key:?}");
        }
        let mut c = config();
        c.mount = "team/transit".into();
        assert_eq!(c.check(false), Ok(()));
        // Vault's own rules apply: https, a region, a token file only in development.
        let mut c = config();
        c.vault.address = "http://vault.example".into();
        assert!(c.check(false).unwrap_err().contains("https"));
        assert_eq!(c.check(true), Ok(()));
        c.vault.auth = VaultAuth::TokenFile { path: "/t".into() };
        assert!(c.check(false).is_err());
        let mut c = config();
        c.vault.auth = VaultAuth::AwsIam {
            mount: "aws".into(),
            role: "r".into(),
            region: None,
            server_id: None,
        };
        assert!(
            c.check(false)
                .unwrap_err()
                .contains("\"region\" is required")
        );
    }
}
