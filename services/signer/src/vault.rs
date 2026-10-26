// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Vault, the signer's default key source (owner ruling `key-custody-opt-in-kms`).
//!
//! The signer reads its keys itself, at start and on every reload: a login, then a KV v1 read
//! whose fields are under `.data` (KV v1). Nothing secret is in its environment
//! or its configuration: the login is Vault's AWS auth method, `iam` type, and it, the token's
//! lifecycle and the retries are `crate::vault_session`'s, which a component's own Transit
//! signing shares (`F-042`, `crate::transit`). A Vault token in a file (`token_file`) is for
//! development and tests only.
//!
//! The token lives in this process only, as long as the reads need it; a value read is never
//! logged, formatted or put in an error.

use std::collections::BTreeMap;

use zeroize::Zeroizing;

use crate::kms::BoxFuture;

#[cfg(feature = "vault")]
pub use crate::vault_session::{STS_BODY, iam_login_payload};

/// A secret's fields, as read: field to value. Wiped when dropped.
pub type Fields = BTreeMap<String, Zeroizing<String>>;

/// Where `vault` key sources are read: Vault ([`Vault`], feature `vault`), or a map in memory
/// (`testing::MemSecrets`).
pub trait SecretStore: Send + Sync {
    /// The fields of the secret at `path` (mount included). Errors never carry a value.
    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Fields, String>>;
}

/// Vault over HTTP (module documentation).
#[cfg(feature = "vault")]
pub struct Vault {
    session: crate::vault_session::VaultSession,
}

#[cfg(feature = "vault")]
impl Vault {
    /// A client of the Vault `c` describes. Nothing is read until the first [`SecretStore::read`].
    pub fn new(c: &crate::config::VaultConfig) -> Result<Vault, String> {
        Ok(Vault {
            session: crate::vault_session::VaultSession::new(c)?,
        })
    }

    /// With another [`crate::vault_session::Policy`] (attempts, backoff, deadlines).
    pub fn with_policy(mut self, policy: crate::vault_session::Policy) -> Vault {
        self.session = self.session.with_policy(policy);
        self
    }

    /// Sign the login with these AWS credentials instead of the default chain's: tests only.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_credentials(mut self, c: aws_credential_types::Credentials) -> Vault {
        self.session = self.session.with_credentials(c);
        self
    }

    /// The session's metrics (logins, answers).
    pub fn metrics(&self) -> std::sync::Arc<crate::vault_session::Metrics> {
        self.session.metrics()
    }
}

#[cfg(feature = "vault")]
impl SecretStore for Vault {
    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Fields, String>> {
        Box::pin(async move {
            let r = self
                .session
                .send(reqwest::Method::GET, path, None)
                .await
                .map_err(|e| format!("vault read {path}: {e}"))?;
            if r.status == 403 {
                return Err(format!(
                    "vault read {path}: 403 (the token's policy does not reach it)"
                ));
            }
            if !(200..300).contains(&r.status) {
                return Err(format!("vault read {path}: {}", r.status));
            }
            let v: serde_json::Value = serde_json::from_str(&r.body)
                .map_err(|_| format!("vault read {path}: an unreadable answer"))?;
            // KV v1: the fields are under `.data`.
            let data = v["data"]
                .as_object()
                .ok_or_else(|| format!("vault read {path}: no .data (KV v1)"))?;
            Ok(data
                .iter()
                .filter_map(|(k, v)| {
                    v.as_str()
                        .map(|s| (k.clone(), Zeroizing::new(s.to_owned())))
                })
                .collect())
        })
    }
}

/// A secret store in memory: tests only.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::collections::BTreeMap;

    use super::*;

    /// Secrets in memory, path to fields.
    #[derive(Default)]
    pub struct MemSecrets {
        /// The secrets.
        pub secrets: BTreeMap<String, BTreeMap<String, String>>,
    }

    impl SecretStore for MemSecrets {
        fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Fields, String>> {
            let out = self
                .secrets
                .get(path)
                .map(|f| {
                    f.iter()
                        .map(|(k, v)| (k.clone(), Zeroizing::new(v.clone())))
                        .collect()
                })
                .ok_or_else(|| format!("no secret at {path}"));
            Box::pin(async move { out })
        }
    }
}
