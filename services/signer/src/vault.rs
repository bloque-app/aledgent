// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Vault, the signer's default key source (owner ruling `key-custody-opt-in-kms`).
//!
//! The signer reads its keys itself, at start and on every reload: a login, then a KV v1 read
//! whose fields are under `.data` (KV v1). Nothing secret is in its environment
//! or its configuration: the login is Vault's AWS auth method, `iam` type. The signer signs an STS
//! `GetCallerIdentity` request with the AWS credentials of its instance or ECS task role (the
//! SDK's default chain: the task's container credentials, the instance's metadata service) and
//! hands Vault the signed request; Vault replays it to STS and learns which role the caller holds,
//! which the Vault role is bound to. A Vault token in a file (`token_file`) is for development
//! and tests only.
//!
//! The token lives in this process only, as long as the reads need it; a value read is never
//! logged, formatted or put in an error.

use std::collections::BTreeMap;

use zeroize::Zeroizing;

use crate::kms::BoxFuture;

/// A secret's fields, as read: field to value. Wiped when dropped.
pub type Fields = BTreeMap<String, Zeroizing<String>>;

/// Where `vault` key sources are read: Vault ([`Vault`], feature `vault`), or a map in memory
/// (`testing::MemSecrets`).
pub trait SecretStore: Send + Sync {
    /// The fields of the secret at `path` (mount included). Errors never carry a value.
    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Fields, String>>;
}

/// The STS request a login signs: its URL and body, as Vault replays them.
pub const STS_BODY: &str = "Action=GetCallerIdentity&Version=2011-06-15";

/// The JSON body of an `iam` login to Vault's AWS auth method (`auth/<mount>/login`): the STS
/// `GetCallerIdentity` request for `region`, signed with `credentials` at `time`, every part
/// base64 as Vault expects, with `X-Vault-AWS-IAM-Server-ID` signed in when `server_id` is set.
#[cfg(feature = "vault")]
pub fn iam_login_payload(
    credentials: &aws_credential_types::Credentials,
    role: &str,
    region: &str,
    server_id: Option<&str>,
    time: std::time::SystemTime,
) -> Result<serde_json::Value, String> {
    use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
    use aws_sigv4::sign::v4;
    use base64::Engine as _;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let host = format!("sts.{region}.amazonaws.com");
    let url = format!("https://{host}/");
    let mut headers: Vec<(String, String)> = vec![
        (
            "Content-Type".into(),
            "application/x-www-form-urlencoded; charset=utf-8".into(),
        ),
        ("Host".into(), host),
    ];
    if let Some(id) = server_id {
        headers.push(("X-Vault-AWS-IAM-Server-ID".into(), id.into()));
    }
    let identity = credentials.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("sts")
        .time(time)
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| format!("signing the STS request: {e}"))?
        .into();
    let signable = SignableRequest::new(
        "POST",
        &url,
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        SignableBody::Bytes(STS_BODY.as_bytes()),
    )
    .map_err(|e| format!("signing the STS request: {e}"))?;
    let (instructions, _) = sign(signable, &params)
        .map_err(|e| format!("signing the STS request: {e}"))?
        .into_parts();
    let (signed, _) = instructions.into_parts();
    let mut all: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in &headers {
        all.insert(k.clone(), vec![v.clone()]);
    }
    for h in &signed {
        all.insert(h.name().to_owned(), vec![h.value().to_owned()]);
    }
    let headers_json = serde_json::to_vec(&all).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "role": role,
        "iam_http_request_method": "POST",
        "iam_request_url": b64(url.as_bytes()),
        "iam_request_body": b64(STS_BODY.as_bytes()),
        "iam_request_headers": b64(&headers_json),
    }))
}

/// Vault over HTTP (module documentation).
#[cfg(feature = "vault")]
pub struct Vault {
    address: String,
    auth: crate::config::VaultAuth,
    http: reqwest::Client,
    token: tokio::sync::Mutex<Option<Zeroizing<String>>>,
    credentials: Option<aws_credential_types::Credentials>,
}

#[cfg(feature = "vault")]
impl Vault {
    /// A client of the Vault `c` describes. Nothing is read until the first [`SecretStore::read`].
    pub fn new(c: &crate::config::VaultConfig) -> Result<Vault, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("vault: {e}"))?;
        Ok(Vault {
            address: c.address.trim_end_matches('/').to_owned(),
            auth: c.auth.clone(),
            http,
            token: tokio::sync::Mutex::new(None),
            credentials: None,
        })
    }

    /// Sign the login with these AWS credentials instead of the default chain's: tests only.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_credentials(mut self, c: aws_credential_types::Credentials) -> Vault {
        self.credentials = Some(c);
        self
    }

    async fn credentials(&self, region: &str) -> Result<aws_credential_types::Credentials, String> {
        use aws_credential_types::provider::ProvideCredentials as _;
        if let Some(c) = &self.credentials {
            return Ok(c.clone());
        }
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_owned()))
            .load()
            .await;
        let provider = config
            .credentials_provider()
            .ok_or("vault: no AWS credentials (the instance or task role)")?;
        provider
            .provide_credentials()
            .await
            .map_err(|e| format!("vault: no AWS credentials: {e}"))
    }

    async fn login(&self) -> Result<Zeroizing<String>, String> {
        use crate::config::VaultAuth;
        match &self.auth {
            VaultAuth::TokenFile { path } => {
                let t = Zeroizing::new(
                    std::fs::read_to_string(path)
                        .map_err(|e| format!("vault: the token file {}: {e}", path.display()))?,
                );
                Ok(Zeroizing::new(t.trim().to_owned()))
            }
            VaultAuth::AwsIam {
                mount,
                role,
                region,
                server_id,
            } => {
                // `Config::check` refuses a configuration without it: there is no default.
                let region = region
                    .as_deref()
                    .ok_or("vault.auth.aws_iam: \"region\" is required; there is no default")?;
                let creds = self.credentials(region).await?;
                let payload = iam_login_payload(
                    &creds,
                    role,
                    region,
                    server_id.as_deref(),
                    std::time::SystemTime::now(),
                )?;
                let url = format!("{}/v1/auth/{mount}/login", self.address);
                let r = self
                    .http
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| format!("vault login: {}", e.without_url()))?;
                let status = r.status();
                if !status.is_success() {
                    return Err(format!(
                        "vault login: {status} (auth/{mount}, role {role}): is the role bound to \
                         this instance's or task's IAM role?"
                    ));
                }
                let v: serde_json::Value = r
                    .json()
                    .await
                    .map_err(|_| "vault login: an unreadable answer".to_owned())?;
                v["auth"]["client_token"]
                    .as_str()
                    .map(|t| Zeroizing::new(t.to_owned()))
                    .ok_or_else(|| "vault login: no token in the answer".to_owned())
            }
        }
    }

    async fn get(&self, path: &str, token: &str) -> Result<Option<Fields>, String> {
        let url = format!("{}/v1/{}", self.address, path.trim_start_matches('/'));
        let r = self
            .http
            .get(&url)
            .header("X-Vault-Token", token)
            .send()
            .await
            .map_err(|e| format!("vault read {path}: {}", e.without_url()))?;
        let status = r.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!("vault read {path}: {status}"));
        }
        let v: Zeroizing<String> = Zeroizing::new(
            r.text()
                .await
                .map_err(|_| format!("vault read {path}: an unreadable answer"))?,
        );
        let v: serde_json::Value = serde_json::from_str(&v)
            .map_err(|_| format!("vault read {path}: an unreadable answer"))?;
        // KV v1: the fields are under `.data`.
        let data = v["data"]
            .as_object()
            .ok_or_else(|| format!("vault read {path}: no .data (KV v1)"))?;
        Ok(Some(
            data.iter()
                .filter_map(|(k, v)| {
                    v.as_str()
                        .map(|s| (k.clone(), Zeroizing::new(s.to_owned())))
                })
                .collect(),
        ))
    }
}

#[cfg(feature = "vault")]
impl SecretStore for Vault {
    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Fields, String>> {
        Box::pin(async move {
            let mut token = self.token.lock().await;
            for _ in 0..2 {
                let t = match token.as_ref() {
                    Some(t) => t.clone(),
                    None => {
                        let t = self.login().await?;
                        *token = Some(t.clone());
                        t
                    }
                };
                match self.get(path, &t).await? {
                    Some(f) => return Ok(f),
                    // An expired token: log in again, once.
                    None => *token = None,
                }
            }
            Err(format!(
                "vault read {path}: 403 (the token's policy does not reach it)"
            ))
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

#[cfg(all(test, feature = "vault"))]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// The login is the STS request Vault replays: the URL, the body and the signed headers,
    /// base64, with the server id signed in.
    #[test]
    fn nfr_8_an_iam_login_is_a_signed_sts_request() {
        let creds = aws_credential_types::Credentials::new(
            "AKIDEXAMPLE",
            "secret-not-a-real-key",
            Some("session".into()),
            None,
            "test",
        );
        let p = iam_login_payload(
            &creds,
            "aledgent-signer-iam",
            "us-east-2",
            Some("vault.example.com"),
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
        )
        .unwrap();
        let d = |k: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(p[k].as_str().unwrap())
                .unwrap()
        };
        assert_eq!(p["role"], "aledgent-signer-iam");
        assert_eq!(
            d("iam_request_url"),
            b"https://sts.us-east-2.amazonaws.com/"
        );
        assert_eq!(d("iam_request_body"), STS_BODY.as_bytes());
        let h: BTreeMap<String, Vec<String>> =
            serde_json::from_slice(&d("iam_request_headers")).unwrap();
        let auth = &h["authorization"][0];
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
            "{auth}"
        );
        assert!(auth.contains("/us-east-2/sts/aws4_request"));
        assert!(auth.contains("x-vault-aws-iam-server-id"));
        assert_eq!(h["X-Vault-AWS-IAM-Server-ID"][0], "vault.example.com");
        assert_eq!(h["x-amz-security-token"][0], "session");
        // The secret key itself never travels.
        assert!(
            !serde_json::to_string(&p)
                .unwrap()
                .contains("secret-not-a-real-key")
        );
    }
}
