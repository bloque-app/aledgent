// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The archive's S3 backend (`T-005-15`; `DEC-22` (b), "an S3 sink first", the owner,
//! 2026-10-04).
//!
//! An [`ObjectStore`] over S3's REST interface, signed with Signature Version 4, over HTTPS:
//!
//! - **Write once.** Every PUT carries `If-None-Match: *`, so S3 refuses to replace an object
//!   (412). The store then reads what is there: the same bytes are an exact retry, different
//!   bytes a [`StoreError::Conflict`] (another writer, or another history).
//! - **Object Lock.** Every PUT carries a retention (`x-amz-object-lock-mode`,
//!   `x-amz-object-lock-retain-until-date`), so nobody, the deployment's own role included, can
//!   delete or overwrite a segment within it (compliance mode); the bucket must have Object Lock
//!   enabled, and S3 refuses the PUT otherwise. Each PUT carries its SHA-256 checksum and MD5, as
//!   S3 requires of a locked upload, and S3 checks them.
//! - **KMS, optional.** With `kms-key-id`, every PUT asks for server-side encryption under the
//!   deployment's KMS key (`aws:kms`); reads decrypt transparently for a role allowed to use the
//!   key. Since `F-044` aledgent seals every object itself ([`crate::seal`]), so the archive
//!   depends on no KMS key; KMS may be kept as a second layer.
//!
//! A writer refuses to start without Object Lock, or without either encryption
//! ([`S3Config::check_writer`]): the archive holds every decision the core committed. The lock
//! defaults to compliance mode for [`crate::archive::DEFAULT_RETAIN_DAYS`] days. Reads need
//! neither, so a restore can read a bucket with a read-only role.
//!
//! **Credentials** come, in order, from the file named by `ALEDGENT_ARCHIVE_CREDENTIALS_FILE`
//! (short-lived credentials in the instance metadata service's JSON shape, `AccessKeyId`,
//! `SecretAccessKey`, `Token`, `Expiration`, kept fresh by an agent on the host such as Vault
//! Agent rendering from Vault's AWS secrets engine; read again before they expire), from the
//! environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`), or, on an EC2
//! execution node, from the instance role through IMDSv2, refreshed before they expire. None is
//! ever an argument, a default, or in a log line (`REQ-AU-8`).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac as _};
use sha2::{Digest as _, Sha256};

use crate::store::{ObjectStore, Put, StoreError, check_name};

type HmacSha256 = Hmac<Sha256>;

/// The hash of an empty payload.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Object Lock's two modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// Nobody can shorten the retention or delete the object, the account's root included.
    Compliance,
    /// A principal with `s3:BypassGovernanceRetention` can.
    Governance,
}

impl LockMode {
    fn header(self) -> &'static str {
        match self {
            LockMode::Compliance => "COMPLIANCE",
            LockMode::Governance => "GOVERNANCE",
        }
    }
}

/// Where the archive lives in S3, and how it is protected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Config {
    /// The bucket.
    pub bucket: String,
    /// The key prefix, empty or ending in `/`.
    pub prefix: String,
    /// The bucket's region.
    pub region: String,
    /// Another endpoint than AWS's (an S3-compatible store in tests), `scheme://host[:port]`.
    pub endpoint: Option<String>,
    /// Address the bucket in the path (`endpoint/bucket/key`) rather than the host.
    pub path_style: bool,
    /// The KMS key every object is encrypted under: an id, an ARN or an alias.
    pub kms_key_id: Option<String>,
    /// Object Lock: the mode and the retention in days.
    pub lock: Option<(LockMode, u32)>,
}

impl S3Config {
    /// A writer must lock what it writes, and encrypt it: sealed by aledgent (`sealed`, `F-044`),
    /// under KMS, or both (module documentation).
    pub fn check_writer(&self, sealed: bool) -> Result<(), String> {
        if !sealed && self.kms_key_id.as_deref().is_none_or(str::is_empty) {
            return Err("the archive's S3 sink needs seal= (aledgent seals every object) or kms-key-id (S3 encrypts it under the deployment's KMS key)".into());
        }
        match self.lock {
            Some((_, days)) if days > 0 => Ok(()),
            _ => Err("the archive's S3 sink needs lock-mode and retain-days: every object is under Object Lock".into()),
        }
    }

    fn host_and_base(&self) -> (String, String, String) {
        // (scheme://host[:port], host header, path prefix before the key)
        match &self.endpoint {
            Some(e) => {
                let e = e.trim_end_matches('/');
                let host = e.split_once("://").map_or(e, |(_, h)| h).to_owned();
                if self.path_style {
                    (e.to_owned(), host, format!("/{}", self.bucket))
                } else {
                    let (scheme, h) = e.split_once("://").unwrap_or(("https", e));
                    let host = format!("{}.{h}", self.bucket);
                    (format!("{scheme}://{host}"), host, String::new())
                }
            }
            None if self.path_style => {
                let host = format!("s3.{}.amazonaws.com", self.region);
                (format!("https://{host}"), host, format!("/{}", self.bucket))
            }
            None => {
                let host = format!("{}.s3.{}.amazonaws.com", self.bucket, self.region);
                (format!("https://{host}"), host, String::new())
            }
        }
    }
}

/// AWS credentials. `Debug` shows only the access key id.
#[derive(Clone)]
pub struct Credentials {
    /// The access key id.
    pub access_key: String,
    /// The secret access key.
    pub secret: String,
    /// The session token of temporary credentials.
    pub token: Option<String>,
    /// When they expire, in seconds since the Unix epoch.
    pub expires: Option<u64>,
}

impl core::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key", &self.access_key)
            .finish_non_exhaustive()
    }
}

/// Where credentials come from.
pub enum CredentialSource {
    /// Given (tests).
    Fixed(Credentials),
    /// A file of short-lived credentials an agent on the host keeps fresh (module
    /// documentation), read again whenever the ones read last are about to expire.
    File(PathBuf),
    /// The file named by [`CREDENTIALS_FILE_ENV`] if set, else the environment, else the
    /// instance role through IMDSv2 (module documentation).
    Default,
}

/// The environment variable naming a credentials file ([`CredentialSource::File`]).
pub const CREDENTIALS_FILE_ENV: &str = "ALEDGENT_ARCHIVE_CREDENTIALS_FILE";

/// The S3 backend (module documentation).
pub struct S3Store {
    config: S3Config,
    source: CredentialSource,
    cached: Mutex<Option<Credentials>>,
    agent: ureq::Agent,
}

/// The largest listing page S3 returns.
const LIST_PAGE: usize = 1000;

/// Idle connections the agent keeps, to the one endpoint it talks to.
const IDLE_CONNECTIONS: usize = 64;

impl S3Store {
    /// A store over `config`, with credentials from `source`.
    pub fn new(config: S3Config, source: CredentialSource) -> S3Store {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(60)))
                // A restore reads many segments at once (`fetch::DEFAULT_CONCURRENCY`): keep a
                // connection for each reader, not ureq's default three, so none opens TLS anew.
                .max_idle_connections(IDLE_CONNECTIONS)
                .max_idle_connections_per_host(IDLE_CONNECTIONS)
                .build(),
        );
        S3Store {
            config,
            source,
            cached: Mutex::new(None),
            agent,
        }
    }

    /// Its configuration.
    pub fn config(&self) -> &S3Config {
        &self.config
    }

    fn credentials(&self) -> Result<Credentials, StoreError> {
        let mut cached = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        let now = unix_now();
        if let Some(c) = cached.as_ref()
            && c.expires.is_none_or(|e| e > now + 300)
        {
            return Ok(c.clone());
        }
        let fresh = match &self.source {
            CredentialSource::Fixed(c) => Ok(c.clone()),
            CredentialSource::File(path) => from_file(path),
            CredentialSource::Default => match std::env::var_os(CREDENTIALS_FILE_ENV) {
                Some(path) if !path.is_empty() => from_file(std::path::Path::new(&path)),
                _ => match from_env() {
                    Some(c) => Ok(c),
                    None => from_imds(&self.agent),
                },
            },
        };
        match fresh {
            Ok(fresh) => {
                *cached = Some(fresh.clone());
                Ok(fresh)
            }
            // A source that fails while the credentials held are still valid (a credentials file
            // caught mid-rewrite, a slow metadata service): keep using them until they expire.
            Err(e) => match cached.as_ref() {
                Some(c) if c.expires.is_some_and(|x| x > now) => Ok(c.clone()),
                _ => Err(e),
            },
        }
    }

    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.config.prefix)
    }

    /// Send one signed request. `query` is unencoded pairs; `headers` lowercase names.
    fn send(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        mut headers: Vec<(String, String)>,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), StoreError> {
        let creds = self.credentials()?;
        let (base, host, path_base) = self.config.host_and_base();
        let path = match key {
            Some(k) => format!("{path_base}/{}", uri_encode(k, false)),
            None if path_base.is_empty() => "/".to_owned(),
            None => path_base,
        };
        let payload_hash = if body.is_empty() {
            EMPTY_SHA256.to_owned()
        } else {
            hex::encode(Sha256::digest(body))
        };
        let (amz_date, _) = amz_now();
        headers.push(("host".into(), host));
        headers.push(("x-amz-date".into(), amz_date.clone()));
        headers.push(("x-amz-content-sha256".into(), payload_hash.clone()));
        if let Some(t) = &creds.token {
            headers.push(("x-amz-security-token".into(), t.clone()));
        }
        let mut q: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
            .collect();
        q.sort();
        let canonical_query = q
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let authorization = sign(
            &creds,
            &self.config.region,
            &amz_date,
            method,
            &path,
            &canonical_query,
            &headers,
            &payload_hash,
        );
        let url = if canonical_query.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{canonical_query}")
        };
        let mut req = ureq::http::Request::builder().method(method).uri(&url);
        for (k, v) in &headers {
            if k != "host" {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        req = req.header("authorization", authorization);
        let built = |e: ureq::http::Error| StoreError::Unavailable(format!("S3 request: {e}"));
        let sent = if body.is_empty() {
            self.agent.run(req.body(()).map_err(built)?)
        } else {
            self.agent.run(req.body(body).map_err(built)?)
        };
        let mut resp = sent.map_err(|e| StoreError::Unavailable(format!("S3 {method}: {e}")))?;
        let status = resp.status().as_u16();
        if method == "HEAD" {
            let mut h = Vec::new();
            for (k, v) in resp.headers() {
                if let Ok(v) = v.to_str() {
                    h.extend_from_slice(format!("{}: {v}\n", k.as_str()).as_bytes());
                }
            }
            return Ok((status, h));
        }
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(crate::record::MAX_RECORD_BYTES + 1024 * 1024)
            .read_to_vec()
            .map_err(|e| StoreError::Unavailable(format!("S3 {method}: {e}")))?;
        Ok((status, bytes))
    }
}

/// The error code in an S3 error body, never its message (which may echo request details).
fn s3_code(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    xml_values(&text, "Code")
        .into_iter()
        .next()
        .unwrap_or_else(|| "no code".into())
}

/// How S3 protects one object, as a HEAD of it reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Protection {
    /// `x-amz-server-side-encryption`: `aws:kms` under KMS.
    pub encryption: Option<String>,
    /// `x-amz-server-side-encryption-aws-kms-key-id`.
    pub kms_key: Option<String>,
    /// `x-amz-object-lock-mode`.
    pub lock_mode: Option<String>,
    /// `x-amz-object-lock-retain-until-date`.
    pub retain_until: Option<String>,
}

impl S3Store {
    /// How S3 protects the object `name` (a HEAD): what `opsctl archive verify` and the S3 test
    /// read back to show every segment is encrypted under KMS and locked.
    pub fn protection(&self, name: &str) -> Result<Option<Protection>, StoreError> {
        check_name(name)?;
        let key = self.key(name);
        let (status, headers) = self.send("HEAD", Some(&key), &[], Vec::new(), &[])?;
        match status {
            200 => {
                let text = String::from_utf8_lossy(&headers);
                let get = |h: &str| {
                    text.lines().find_map(|l| {
                        let (k, v) = l.split_once(": ")?;
                        k.eq_ignore_ascii_case(h).then(|| v.to_owned())
                    })
                };
                Ok(Some(Protection {
                    encryption: get("x-amz-server-side-encryption"),
                    kms_key: get("x-amz-server-side-encryption-aws-kms-key-id"),
                    lock_mode: get("x-amz-object-lock-mode"),
                    retain_until: get("x-amz-object-lock-retain-until-date"),
                }))
            }
            404 => Ok(None),
            _ => Err(StoreError::Unavailable(format!("S3 HEAD {name}: {status}"))),
        }
    }
}

impl ObjectStore for S3Store {
    fn put_once(&self, name: &str, bytes: &[u8]) -> Result<Put, StoreError> {
        check_name(name)?;
        let key = self.key(name);
        let sha = Sha256::digest(bytes);
        let mut headers = vec![
            (
                "content-type".to_owned(),
                "application/octet-stream".to_owned(),
            ),
            ("if-none-match".to_owned(), "*".to_owned()),
            (
                "x-amz-checksum-sha256".to_owned(),
                base64::engine::general_purpose::STANDARD.encode(sha),
            ),
            (
                "x-amz-sdk-checksum-algorithm".to_owned(),
                "SHA256".to_owned(),
            ),
            (
                "content-md5".to_owned(),
                base64::engine::general_purpose::STANDARD
                    .encode(<md5::Md5 as md5::Digest>::digest(bytes)),
            ),
        ];
        if let Some(k) = &self.config.kms_key_id {
            headers.push(("x-amz-server-side-encryption".into(), "aws:kms".into()));
            headers.push((
                "x-amz-server-side-encryption-aws-kms-key-id".into(),
                k.clone(),
            ));
        }
        if let Some((mode, days)) = self.config.lock {
            let until = unix_now() + u64::from(days) * 86_400;
            headers.push(("x-amz-object-lock-mode".into(), mode.header().into()));
            headers.push(("x-amz-object-lock-retain-until-date".into(), rfc3339(until)));
        }
        let (status, body) = self.send("PUT", Some(&key), &[], headers, bytes)?;
        match status {
            200 | 201 => Ok(Put { created: true }),
            412 => match self.get(name, bytes.len() as u64) {
                Ok(Some(old)) if old == bytes => Ok(Put { created: false }),
                Ok(_) | Err(StoreError::TooLarge(_)) => Err(StoreError::Conflict(name.to_owned())),
                Err(e) => Err(e),
            },
            _ => Err(StoreError::Unavailable(format!(
                "S3 PUT {name}: {status} {}",
                s3_code(&body)
            ))),
        }
    }

    fn get(&self, name: &str, max: u64) -> Result<Option<Vec<u8>>, StoreError> {
        check_name(name)?;
        let key = self.key(name);
        let (status, body) = self.send("GET", Some(&key), &[], Vec::new(), &[])?;
        match status {
            200 if body.len() as u64 > max => Err(StoreError::TooLarge(name.to_owned())),
            200 => Ok(Some(body)),
            404 => Ok(None),
            _ => Err(StoreError::Unavailable(format!(
                "S3 GET {name}: {status} {}",
                s3_code(&body)
            ))),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let rel = prefix.trim_end_matches('/');
        if !rel.is_empty() {
            check_name(rel)?;
        }
        let full = self.key(prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let max_keys = LIST_PAGE.to_string();
            let mut query = vec![
                ("list-type", "2"),
                ("prefix", full.as_str()),
                ("max-keys", max_keys.as_str()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token", t.as_str()));
            }
            let (status, body) = self.send("GET", None, &query, Vec::new(), &[])?;
            if status != 200 {
                return Err(StoreError::Unavailable(format!(
                    "S3 LIST {prefix}: {status} {}",
                    s3_code(&body)
                )));
            }
            let text = String::from_utf8_lossy(&body);
            for k in xml_values(&text, "Key") {
                if let Some(name) = k.strip_prefix(&self.config.prefix)
                    && check_name(name).is_ok()
                {
                    out.push(name.to_owned());
                }
            }
            let truncated = xml_values(&text, "IsTruncated")
                .first()
                .is_some_and(|v| v == "true");
            token = xml_values(&text, "NextContinuationToken")
                .into_iter()
                .next();
            if !truncated || token.is_none() {
                break;
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn describe(&self) -> String {
        format!(
            "s3://{}/{} ({})",
            self.config.bucket, self.config.prefix, self.config.region
        )
    }
}

/// Every text value of `<tag>…</tag>` in `xml`, entities decoded.
fn xml_values(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find(&open) {
        let after = &rest[at + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        out.push(
            after[..end]
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&amp;", "&"),
        );
        rest = &after[end + close.len()..];
    }
    out
}

fn from_env() -> Option<Credentials> {
    let access_key = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
    Some(Credentials {
        access_key,
        secret,
        token: std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        expires: None,
    })
}

/// Temporary credentials in the instance metadata service's JSON shape, which a credentials file
/// uses too.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CredentialsDoc {
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: String,
}

impl CredentialsDoc {
    fn into_credentials(self) -> Credentials {
        Credentials {
            access_key: self.access_key_id,
            secret: self.secret_access_key,
            token: Some(self.token),
            expires: parse_rfc3339(&self.expiration),
        }
    }
}

/// Credentials from a file an agent on the host keeps fresh. Its content is never in an error.
fn from_file(path: &std::path::Path) -> Result<Credentials, StoreError> {
    let fail = |what: &str| {
        StoreError::Unavailable(format!("the credentials file {} {what}", path.display()))
    };
    let body = std::fs::read_to_string(path).map_err(|e| fail(&format!("cannot be read: {e}")))?;
    let doc: CredentialsDoc = serde_json::from_str(&body).map_err(|_| {
        fail("is not credentials (AccessKeyId, SecretAccessKey, Token, Expiration)")
    })?;
    let c = doc.into_credentials();
    if c.expires.is_none() {
        return Err(fail("has an Expiration that does not parse"));
    }
    Ok(c)
}

const IMDS: &str = "http://169.254.169.254";

/// The instance role's credentials through IMDSv2.
fn from_imds(agent: &ureq::Agent) -> Result<Credentials, StoreError> {
    let fail = |what: &str, e: &dyn core::fmt::Display| {
        StoreError::Unavailable(format!(
            "no credentials in the environment, and the instance role's ({what}): {e}"
        ))
    };
    let mut r = agent
        .put(format!("{IMDS}/latest/api/token"))
        .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send_empty()
        .map_err(|e| fail("token", &e))?;
    let token = r
        .body_mut()
        .read_to_string()
        .map_err(|e| fail("token", &e))?;
    let get = |path: &str| -> Result<String, StoreError> {
        let mut r = agent
            .get(format!("{IMDS}{path}"))
            .header("x-aws-ec2-metadata-token", token.trim())
            .call()
            .map_err(|e| fail("metadata", &e))?;
        if r.status().as_u16() != 200 {
            return Err(fail("metadata", &r.status()));
        }
        r.body_mut()
            .read_to_string()
            .map_err(|e| fail("metadata", &e))
    };
    let role = get("/latest/meta-data/iam/security-credentials/")?;
    let role = role.lines().next().unwrap_or("").trim().to_owned();
    if role.is_empty() {
        return Err(fail("role", &"none attached"));
    }
    let body = get(&format!(
        "/latest/meta-data/iam/security-credentials/{role}"
    ))?;
    let c: CredentialsDoc = serde_json::from_str(&body).map_err(|e| fail("credentials", &e))?;
    Ok(c.into_credentials())
}

/// URI-encode as SigV4 wants: every byte but `A-Z a-z 0-9 - _ . ~` as `%XX`, and `/` too unless
/// it is a path.
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).unwrap_or_else(|_| unreachable!("any key"));
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// The `Authorization` header of a SigV4 request for S3. `headers` are every header signed,
/// with lowercase names; `path` is already encoded; `canonical_query` sorted and encoded.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    creds: &Credentials,
    region: &str,
    amz_date: &str,
    method: &str,
    path: &str,
    canonical_query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> String {
    let mut h: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    h.sort();
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let canonical = format!(
        "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload_hash}"
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let k = hmac(format!("AWS4{}", creds.secret).as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    let signature = hex::encode(hmac(&k, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        creds.access_key
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// (`YYYYMMDDTHHMMSSZ`, `YYYYMMDD`) now, in UTC.
fn amz_now() -> (String, String) {
    let s = unix_now();
    let (y, m, d, hh, mm, ss) = civil(s);
    (
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"),
        format!("{y:04}{m:02}{d:02}"),
    )
}

/// `secs` as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` as seconds since the epoch.
fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |r: core::ops::Range<usize>| -> Option<u64> { s.get(r)?.parse().ok() };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let days = days_from_civil(y, m, d)?;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

/// Days since the epoch of a proleptic Gregorian date (H. Hinnant's algorithm), from 1970.
fn days_from_civil(y: u64, m: u64, d: u64) -> Option<u64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe).checked_sub(719_468)
}

/// (year, month, day, hour, minute, second) of `secs` since the epoch, UTC.
fn civil(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d, rem / 3_600, rem % 3_600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Credentials {
        // AWS's documented example credentials (Signature Version 4 examples for S3).
        Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            token: None,
            expires: None,
        }
    }

    fn rfc3339(secs: u64) -> String {
        // Days since the epoch to a civil date (Howard Hinnant's algorithm).
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        )
    }

    fn credentials_file(name: &str, key: &str, expires: u64) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aledgent-archive-creds-{}-{name}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(
                r#"{{"AccessKeyId":"{key}","SecretAccessKey":"s3cr3t","Token":"tok","Expiration":"{}"}}"#,
                rfc3339(expires)
            ),
        )
        .unwrap();
        path
    }

    fn store(source: CredentialSource) -> S3Store {
        let config = S3Config {
            bucket: "b".into(),
            prefix: String::new(),
            region: "us-east-2".into(),
            endpoint: None,
            path_style: false,
            kms_key_id: None,
            lock: None,
        };
        S3Store::new(config, source)
    }

    /// A credentials file is read, with its session token and expiry.
    #[test]
    fn credentials_come_from_a_file() {
        let path = credentials_file("read", "AKIAFILE", unix_now() + 3600);
        let c = store(CredentialSource::File(path.clone()))
            .credentials()
            .unwrap();
        assert_eq!(c.access_key, "AKIAFILE");
        assert_eq!(c.token.as_deref(), Some("tok"));
        assert!(c.expires.is_some_and(|e| e > unix_now()));
        std::fs::remove_file(path).unwrap();
    }

    /// Credentials about to expire are read again from the file, so the host's agent can rotate
    /// them without a restart.
    #[test]
    fn a_credentials_file_is_read_again_before_its_credentials_expire() {
        let path = credentials_file("rotate", "AKIAOLD", unix_now() + 60);
        let s = store(CredentialSource::File(path.clone()));
        assert_eq!(s.credentials().unwrap().access_key, "AKIAOLD");
        credentials_file("rotate", "AKIANEW", unix_now() + 3600);
        assert_eq!(s.credentials().unwrap().access_key, "AKIANEW");
        // Fresh ones are cached: a later rewrite is not read until they near expiry.
        credentials_file("rotate", "AKIALATER", unix_now() + 3600);
        assert_eq!(s.credentials().unwrap().access_key, "AKIANEW");
        std::fs::remove_file(path).unwrap();
    }

    /// A credentials file caught mid-rewrite near expiry: the credentials held, still valid, are
    /// used; once they have expired, the failure is the caller's.
    #[test]
    fn a_torn_credentials_file_keeps_the_valid_credentials_held() {
        let path = credentials_file("torn", "AKIAHELD", unix_now() + 60);
        let s = store(CredentialSource::File(path.clone()));
        assert_eq!(s.credentials().unwrap().access_key, "AKIAHELD");
        std::fs::write(&path, r#"{"AccessKeyId":"AKIAN"#).unwrap();
        assert_eq!(s.credentials().unwrap().access_key, "AKIAHELD");
        let expired = store(CredentialSource::File(path.clone()));
        assert!(expired.credentials().is_err());
        std::fs::remove_file(path).unwrap();
    }

    /// A file that is not credentials, or has no parsable expiry, is refused, and its content is
    /// not in the error.
    #[test]
    fn a_bad_credentials_file_is_refused_without_its_content() {
        let path = std::env::temp_dir().join(format!(
            "aledgent-archive-creds-{}-bad.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{"AccessKeyId":"AKIABAD","SecretAccessKey":"s3cr3t"}"#,
        )
        .unwrap();
        let e = store(CredentialSource::File(path.clone()))
            .credentials()
            .unwrap_err()
            .to_string();
        assert!(!e.contains("s3cr3t") && !e.contains("AKIABAD"), "{e}");
        std::fs::write(
            &path,
            r#"{"AccessKeyId":"AKIABAD","SecretAccessKey":"s3cr3t","Token":"t","Expiration":"soon"}"#,
        )
        .unwrap();
        let e = store(CredentialSource::File(path.clone()))
            .credentials()
            .unwrap_err()
            .to_string();
        assert!(e.contains("Expiration") && !e.contains("s3cr3t"), "{e}");
        std::fs::remove_file(path).unwrap();
    }

    /// AWS's "GET Object" example of Signature Version 4 for S3.
    #[test]
    fn t_005_15_sigv4_matches_aws_get_object_example() {
        let headers = vec![
            (
                "host".to_owned(),
                "examplebucket.s3.amazonaws.com".to_owned(),
            ),
            ("range".to_owned(), "bytes=0-9".to_owned()),
            ("x-amz-content-sha256".to_owned(), EMPTY_SHA256.to_owned()),
            ("x-amz-date".to_owned(), "20130524T000000Z".to_owned()),
        ];
        let a = sign(
            &example(),
            "us-east-1",
            "20130524T000000Z",
            "GET",
            "/test.txt",
            "",
            &headers,
            EMPTY_SHA256,
        );
        assert_eq!(
            a,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn t_005_15_dates_and_encoding() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_369_353_600), "2013-05-24T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        for s in [0, 951_782_400, 1_369_353_600, 4_102_444_799] {
            assert_eq!(parse_rfc3339(&rfc3339(s)), Some(s));
        }
        assert_eq!(
            parse_rfc3339("2026-10-04T12:00:00.000Z"),
            Some(1_791_115_200)
        );
        assert_eq!(uri_encode("a b/c+=~", true), "a%20b%2Fc%2B%3D~");
        assert_eq!(uri_encode("seg/0001.seg", false), "seg/0001.seg");
        let xml = "<a><Key>x/1</Key><Key>x/&amp;2</Key><IsTruncated>false</IsTruncated></a>";
        assert_eq!(xml_values(xml, "Key"), vec!["x/1", "x/&2"]);
    }

    #[test]
    fn t_005_15_a_writer_needs_kms_and_object_lock() {
        let mut c = S3Config {
            bucket: "b".into(),
            prefix: "core/".into(),
            region: "us-east-2".into(),
            endpoint: None,
            path_style: false,
            kms_key_id: None,
            lock: None,
        };
        assert!(c.check_writer(false).is_err());
        assert!(c.check_writer(true).is_err(), "sealed, but not locked");
        c.kms_key_id = Some("alias/aledgent-archive".into());
        assert!(c.check_writer(false).is_err());
        c.lock = Some((LockMode::Compliance, 0));
        assert!(c.check_writer(false).is_err());
        c.lock = Some((LockMode::Compliance, 180));
        assert!(c.check_writer(false).is_ok());
        c.kms_key_id = None;
        assert!(c.check_writer(false).is_err());
        assert!(c.check_writer(true).is_ok());
        assert_eq!(
            c.host_and_base(),
            (
                "https://b.s3.us-east-2.amazonaws.com".into(),
                "b.s3.us-east-2.amazonaws.com".into(),
                String::new()
            )
        );
        c.endpoint = Some("http://127.0.0.1:9000".into());
        c.path_style = true;
        assert_eq!(
            c.host_and_base(),
            (
                "http://127.0.0.1:9000".into(),
                "127.0.0.1:9000".into(),
                "/b".into()
            )
        );
        assert!(format!("{:?}", example()).contains("AKIA"));
        assert!(!format!("{:?}", example()).contains("wJalr"));
    }
}
