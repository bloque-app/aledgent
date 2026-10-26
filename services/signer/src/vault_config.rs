// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Where Vault is and how a process logs in to it, as a configuration file names it: the signer's
//! (`vault`, where it reads its KV keys) and a Vault Transit engine's (`F-042`). Plain data and its
//! checks; the client is `crate::vault_session` (feature `vault-session`).

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

#[cfg(any(test, feature = "transit"))]
pub(crate) fn name_ok(s: &str, slash: bool) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> VaultConfig {
        VaultConfig {
            address: "https://vault.example".into(),
            auth: VaultAuth::AwsIam {
                mount: "aws".into(),
                role: "r".into(),
                region: Some("us-east-1".into()),
                server_id: None,
            },
        }
    }

    #[test]
    fn nfr_8_vault_needs_https_a_region_and_no_token_file_outside_development() {
        assert_eq!(config().check(false), Ok(()));
        let mut c = config();
        c.address = "http://vault.example".into();
        assert!(c.check(false).unwrap_err().contains("https"));
        assert_eq!(c.check(true), Ok(()));
        c.auth = VaultAuth::TokenFile { path: "/t".into() };
        assert!(c.check(false).unwrap_err().contains("development only"));
        let mut c = config();
        c.auth = VaultAuth::AwsIam {
            mount: "aws".into(),
            role: "r".into(),
            region: Some(" ".into()),
            server_id: None,
        };
        assert!(
            c.check(false)
                .unwrap_err()
                .contains("\"region\" is required")
        );
    }

    #[test]
    fn a_mount_or_key_name_is_plain() {
        for ok in ["transit", "team/transit", "a.b-c_d"] {
            assert!(name_ok(ok, true), "{ok}");
        }
        for bad in ["", "transit/../sys", "/transit", "a?b", "a b", "a//b"] {
            assert!(!name_ok(bad, true), "{bad:?}");
        }
        assert!(!name_ok("a/b", false));
    }
}
