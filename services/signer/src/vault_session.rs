// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A logged-in session with Vault, shared by everything in this crate that talks to it: the
//! signer's KV reads (`crate::vault`) and a component's Transit signing (`crate::transit`,
//! `F-042`).
//!
//! **The login** is Vault's AWS auth method, `iam` type (owner ruling `key-custody-opt-in-kms`): the
//! process signs an STS `GetCallerIdentity` request with the AWS credentials of its instance or ECS
//! task role (the SDK's default chain), and hands Vault the signed request; Vault replays it to STS
//! and learns which role the caller holds, which the Vault role is bound to ([`iam_login_payload`]).
//! A Vault token in a file ([`crate::vault_config::VaultAuth::TokenFile`]) is for development only.
//!
//! **The token's lifecycle.** The roles issue short tokens (60 s, 120 s at most).
//!
//! - It is kept with its lease. A request that finds it past two thirds of the lease logs in again
//!   before sending ([`Policy::renew_after`]); the old token is used in the meantime if the new
//!   login fails and it still has life.
//! - Vault refusing the token (403) makes the request log in again and try once more, once; a second
//!   403 is the answer. A login made for a token issued less than [`Policy::relogin_cooldown`] ago
//!   is not made: a policy that denies the request is not a reason to log in on every request.
//! - **One login at a time.** Concurrent requests wait on the login in flight and share its token
//!   (the generation they found refused tells them whether a newer one is already there), and a
//!   login that failed is not tried again by the requests queued behind it for
//!   [`Policy::relogin_cooldown`].
//! - 5xx, 429 and timeouts are retried with a doubling backoff, a bounded number of times
//!   ([`Policy`]). Then that one request fails with a [`VaultError`]; nothing else does.
//!
//! The token lives in this process only. It, a secret read and a payload are never logged,
//! formatted or put in an error.

use std::collections::BTreeMap;
use std::fmt::Write as _;
#[cfg(feature = "test-vault-auth")]
use std::future::Future;
#[cfg(feature = "test-vault-auth")]
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::vault_config::{VaultAuth, VaultConfig};

/// The STS request a login signs: its URL and body, as Vault replays them.
pub const STS_BODY: &str = "Action=GetCallerIdentity&Version=2011-06-15";

/// The JSON body of an `iam` login to Vault's AWS auth method (`auth/<mount>/login`): the STS
/// `GetCallerIdentity` request for `region`, signed with `credentials` at `time`, every part
/// base64 as Vault expects, with `X-Vault-AWS-IAM-Server-ID` signed in when `server_id` is set.
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

/// Why a Vault operation failed. A label of the failure counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// Vault could not be reached (connection refused, DNS, TLS).
    Unreachable,
    /// Vault did not answer in time.
    Timeout,
    /// Vault answered 5xx or 429, after the bounded retries.
    Unavailable,
    /// Vault refused the token (403), also after one fresh login.
    Denied,
    /// The login itself failed.
    Login,
    /// Vault answered another 4xx: the key or the mount is not there, the request is refused.
    Rejected,
    /// A signature came back for another key version than the pinned one.
    Version,
    /// A signature does not verify against the key's public half.
    BadSignature,
    /// An answer that could not be read.
    Malformed,
}

impl Reason {
    /// Every reason, in label order.
    pub const ALL: [Reason; 9] = [
        Reason::Unreachable,
        Reason::Timeout,
        Reason::Unavailable,
        Reason::Denied,
        Reason::Login,
        Reason::Rejected,
        Reason::Version,
        Reason::BadSignature,
        Reason::Malformed,
    ];

    /// Its label.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Unreachable => "unreachable",
            Reason::Timeout => "timeout",
            Reason::Unavailable => "unavailable",
            Reason::Denied => "denied",
            Reason::Login => "login",
            Reason::Rejected => "rejected",
            Reason::Version => "version_mismatch",
            Reason::BadSignature => "bad_signature",
            Reason::Malformed => "malformed",
        }
    }

    /// Whether Vault was not there to answer, so waiting may help.
    pub fn vault_is_down(self) -> bool {
        matches!(
            self,
            Reason::Unreachable | Reason::Timeout | Reason::Unavailable
        )
    }
}

/// A failed Vault operation. The text never carries a payload, a signature or a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultError {
    /// Why.
    pub reason: Reason,
    /// What, in words.
    pub detail: String,
}

impl VaultError {
    /// An error of `reason`.
    pub fn new(reason: Reason, detail: impl Into<String>) -> VaultError {
        VaultError {
            reason,
            detail: detail.into(),
        }
    }
}

impl core::fmt::Display for VaultError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "vault ({}): {}", self.reason.as_str(), self.detail)
    }
}

/// How patient a session is. The defaults keep one request, retries and a login included, inside
/// the eight seconds that leave a caller with the signer's ten-second default.
#[derive(Clone, Debug)]
pub struct Policy {
    /// One HTTP attempt.
    pub attempt_timeout: Duration,
    /// Attempts per request (and per login), the first included.
    pub max_attempts: u32,
    /// The pause before the second attempt; doubled for each one after.
    pub backoff: Duration,
    /// The whole of a signature: login, attempts and pauses.
    pub deadline: Duration,
    /// A token issued less than this ago is not replaced because Vault refused it; a login that
    /// failed less than this ago is not tried again.
    pub relogin_cooldown: Duration,
    /// After a renewal that failed, when to try it again.
    pub renew_retry: Duration,
    /// How often a Transit key checks Vault and keeps the token fresh while idle. Zero: never.
    pub heartbeat: Duration,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            attempt_timeout: Duration::from_secs(2),
            max_attempts: 3,
            backoff: Duration::from_millis(100),
            deadline: Duration::from_secs(8),
            relogin_cooldown: Duration::from_secs(1),
            renew_retry: Duration::from_secs(5),
            heartbeat: Duration::from_secs(15),
        }
    }
}

impl Policy {
    /// When, inside a lease of `ttl`, a token is replaced: two thirds of it.
    pub fn renew_after(ttl: Duration) -> Duration {
        ttl * 2 / 3
    }
}

/// The upper bounds, in microseconds, of the latency histogram's buckets (the last bucket is
/// everything above).
pub const BUCKETS_US: [u64; 10] = [
    1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 1_000_000, 5_000_000,
];

/// Whether this process can use Vault, as of the last time it asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Readiness {
    /// Ready: Vault answered the last request (or none was made yet) and the key still checks.
    pub ready: bool,
    /// Vault did not answer the last request or more (network, timeout, 5xx).
    pub vault_unreachable: bool,
    /// How many requests in a row it did not answer.
    pub consecutive_failures: u64,
    /// Seconds since Vault last answered, if it ever did.
    pub last_answer_secs_ago: Option<u64>,
}

/// Metrics of a session and of the Transit signatures made over it. Integers only (`INV-7`):
/// microseconds and counts, on atomics, like the rest of the signer's.
pub struct Metrics {
    buckets: [AtomicU64; BUCKETS_US.len() + 1],
    sum_us: AtomicU64,
    count: AtomicU64,
    failures: [AtomicU64; Reason::ALL.len()],
    /// Signatures given.
    pub signs: AtomicU64,
    /// Logins made.
    pub logins: AtomicU64,
    /// Logins made because Vault refused the token (403).
    pub relogins_after_refusal: AtomicU64,
    unreachable: AtomicBool,
    key_drifted: AtomicBool,
    consecutive_failures: AtomicU64,
    last_answer: Mutex<Option<Instant>>,
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics {
            buckets: Default::default(),
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
            failures: Default::default(),
            signs: AtomicU64::new(0),
            logins: AtomicU64::new(0),
            relogins_after_refusal: AtomicU64::new(0),
            unreachable: AtomicBool::new(false),
            key_drifted: AtomicBool::new(false),
            consecutive_failures: AtomicU64::new(0),
            last_answer: Mutex::new(None),
        }
    }
}

impl Metrics {
    /// One signature request took `elapsed` (retries and logins included) and ended in `failure`
    /// or not.
    pub fn observe(&self, elapsed: Duration, failure: Option<Reason>) {
        let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let at = BUCKETS_US
            .iter()
            .position(|b| us <= *b)
            .unwrap_or(BUCKETS_US.len());
        self.buckets[at].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        match failure {
            None => {
                self.signs.fetch_add(1, Ordering::Relaxed);
            }
            Some(r) => {
                self.failures[r as usize].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// How many requests failed for `reason`.
    pub fn failures(&self, reason: Reason) -> u64 {
        self.failures[reason as usize].load(Ordering::Relaxed)
    }

    /// How many requests were observed (the histogram's count).
    pub fn observed(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Vault answered (with anything under 500 but 429). Logs the way back to reachable.
    pub fn vault_answered(&self) {
        *self
            .last_answer
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        self.consecutive_failures.store(0, Ordering::Relaxed);
        if self.unreachable.swap(false, Ordering::Relaxed) {
            tracing::info!("vault is reachable again");
        }
    }

    /// Vault did not answer. Logs the way to unreachable, once.
    pub fn vault_silent(&self, why: &str) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        if !self.unreachable.swap(true, Ordering::Relaxed) {
            tracing::warn!(why, "vault is unreachable: not ready");
        }
    }

    /// The Transit key no longer is what was pinned (exportable now, or its public key changed).
    /// Signatures are still verified here, so none that is wrong gets out; but the key is not what
    /// was approved, and the process is not ready until an operator looks.
    pub fn key_drifted(&self, why: &str) {
        if !self.key_drifted.swap(true, Ordering::Relaxed) {
            tracing::error!(why, "the transit key is not what was pinned: not ready");
        }
    }

    /// Whether this process is ready (see [`Readiness`]).
    pub fn readiness(&self) -> Readiness {
        let down = self.unreachable.load(Ordering::Relaxed);
        Readiness {
            ready: !down && !self.key_drifted.load(Ordering::Relaxed),
            vault_unreachable: down,
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
            last_answer_secs_ago: self
                .last_answer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .map(|t| t.elapsed().as_secs()),
        }
    }

    /// The metrics as Prometheus text.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP signer_transit_sign_latency_us Transit signatures, microseconds, retries and logins included."
        );
        let _ = writeln!(out, "# TYPE signer_transit_sign_latency_us histogram");
        let mut cumulative = 0u64;
        for (i, b) in BUCKETS_US.iter().enumerate() {
            cumulative += n(&self.buckets[i]);
            let _ = writeln!(
                out,
                "signer_transit_sign_latency_us_bucket{{le=\"{b}\"}} {cumulative}"
            );
        }
        cumulative += n(&self.buckets[BUCKETS_US.len()]);
        let _ = writeln!(
            out,
            "signer_transit_sign_latency_us_bucket{{le=\"+Inf\"}} {cumulative}"
        );
        let _ = writeln!(
            out,
            "signer_transit_sign_latency_us_sum {}",
            n(&self.sum_us)
        );
        let _ = writeln!(
            out,
            "signer_transit_sign_latency_us_count {}",
            n(&self.count)
        );
        let _ = writeln!(
            out,
            "# HELP signer_transit_sign_failures_total Transit signatures that failed, by reason.\n# TYPE signer_transit_sign_failures_total counter"
        );
        for r in Reason::ALL {
            let _ = writeln!(
                out,
                "signer_transit_sign_failures_total{{reason=\"{}\"}} {}",
                r.as_str(),
                self.failures(r)
            );
        }
        let _ = writeln!(
            out,
            "# TYPE signer_transit_signs_total counter\nsigner_transit_signs_total {}",
            n(&self.signs)
        );
        let _ = writeln!(
            out,
            "# TYPE signer_vault_logins_total counter\nsigner_vault_logins_total {}",
            n(&self.logins)
        );
        let _ = writeln!(
            out,
            "# TYPE signer_vault_relogins_after_refusal_total counter\nsigner_vault_relogins_after_refusal_total {}",
            n(&self.relogins_after_refusal)
        );
        let _ = writeln!(
            out,
            "# HELP signer_vault_ready 1 when Vault answered the last request and the transit key checks.\n# TYPE signer_vault_ready gauge\nsigner_vault_ready {}",
            u8::from(self.readiness().ready)
        );
        out
    }
}

/// The answer to a request: its status, and its body (wiped when dropped: a KV read's is a
/// secret).
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: Zeroizing<String>,
}

#[derive(Clone)]
struct Session {
    token: Zeroizing<String>,
    generation: u64,
    created: Instant,
    renew_at: Option<Instant>,
    expires_at: Option<Instant>,
}

#[cfg(feature = "test-vault-auth")]
type LoginFuture = Pin<Box<dyn Future<Output = Result<(String, Option<Duration>), String>> + Send>>;

/// A login that is not one of Vault's: a token from wherever a test gets it. Only with the cargo
/// feature `test-vault-auth`, which release builds refuse (see the crate documentation).
#[cfg(feature = "test-vault-auth")]
pub type TestLogin = Arc<dyn Fn() -> LoginFuture + Send + Sync>;

/// A logged-in session with Vault (module documentation).
pub struct VaultSession {
    address: String,
    auth: VaultAuth,
    http: reqwest::Client,
    policy: Policy,
    metrics: Arc<Metrics>,
    credentials: Option<aws_credential_types::Credentials>,
    session: Mutex<Option<Arc<Session>>>,
    generations: AtomicU64,
    gate: tokio::sync::Mutex<Option<(Instant, VaultError)>>,
    #[cfg(feature = "test-vault-auth")]
    test_login: Option<TestLogin>,
}

impl VaultSession {
    /// A session with the Vault `c` describes. Nothing is sent until the first request.
    pub fn new(c: &VaultConfig) -> Result<VaultSession, String> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("vault: {e}"))?;
        Ok(VaultSession {
            address: c.address.trim_end_matches('/').to_owned(),
            auth: c.auth.clone(),
            http,
            policy: Policy::default(),
            metrics: Arc::new(Metrics::default()),
            credentials: None,
            session: Mutex::new(None),
            generations: AtomicU64::new(0),
            gate: tokio::sync::Mutex::new(None),
            #[cfg(feature = "test-vault-auth")]
            test_login: None,
        })
    }

    /// With another [`Policy`].
    pub fn with_policy(mut self, policy: Policy) -> VaultSession {
        self.policy = policy;
        self
    }

    /// Sign the login with these AWS credentials instead of the default chain's: tests only.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_credentials(mut self, c: aws_credential_types::Credentials) -> VaultSession {
        self.credentials = Some(c);
        self
    }

    /// Log in by calling `login` instead of Vault's auth methods: the end-to-end tests only,
    /// against a Vault in dev mode, where the AWS login cannot run. It returns a token and its
    /// lease. Not built into a release (crate documentation).
    #[cfg(feature = "test-vault-auth")]
    pub fn with_test_login(mut self, login: TestLogin) -> VaultSession {
        self.test_login = Some(login);
        self
    }

    /// This session's metrics.
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    /// Its policy.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/{}", self.address, path.trim_start_matches('/'))
    }

    fn classify(&self, what: &str, e: reqwest::Error) -> VaultError {
        let reason = if e.is_timeout() {
            Reason::Timeout
        } else {
            Reason::Unreachable
        };
        VaultError::new(reason, format!("{what}: {}", e.without_url()))
    }

    async fn aws_credentials(
        &self,
        region: &str,
    ) -> Result<aws_credential_types::Credentials, VaultError> {
        use aws_credential_types::provider::ProvideCredentials as _;
        let no = |e: String| VaultError::new(Reason::Login, e);
        if let Some(c) = &self.credentials {
            return Ok(c.clone());
        }
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_owned()))
            .load()
            .await;
        let provider = config
            .credentials_provider()
            .ok_or_else(|| no("no AWS credentials (the instance or task role)".into()))?;
        provider
            .provide_credentials()
            .await
            .map_err(|e| no(format!("no AWS credentials: {e}")))
    }

    /// One login attempt: a token and, if it expires, its lease.
    async fn login_once(&self) -> Result<(Zeroizing<String>, Option<Duration>), VaultError> {
        #[cfg(feature = "test-vault-auth")]
        if let Some(f) = &self.test_login {
            return match f().await {
                Ok((t, ttl)) => Ok((Zeroizing::new(t), ttl)),
                Err(e) => Err(VaultError::new(Reason::Login, e)),
            };
        }
        match &self.auth {
            VaultAuth::TokenFile { path } => {
                let t = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| {
                    VaultError::new(
                        Reason::Login,
                        format!("the token file {}: {e}", path.display()),
                    )
                })?);
                Ok((Zeroizing::new(t.trim().to_owned()), None))
            }
            VaultAuth::AwsIam {
                mount,
                role,
                region,
                server_id,
            } => {
                // `check` refuses a configuration without it: there is no default.
                let region = region.as_deref().ok_or_else(|| {
                    VaultError::new(
                        Reason::Login,
                        "vault.auth.aws_iam: \"region\" is required; there is no default",
                    )
                })?;
                let creds = self.aws_credentials(region).await?;
                let payload = iam_login_payload(
                    &creds,
                    role,
                    region,
                    server_id.as_deref(),
                    std::time::SystemTime::now(),
                )
                .map_err(|e| VaultError::new(Reason::Login, e))?;
                let r = self
                    .http
                    .post(self.url(&format!("auth/{mount}/login")))
                    .timeout(self.policy.attempt_timeout)
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| self.classify("login", e))?;
                let status = r.status();
                if status.is_server_error() || status.as_u16() == 429 {
                    return Err(VaultError::new(
                        Reason::Unavailable,
                        format!("login: {status}"),
                    ));
                }
                if !status.is_success() {
                    self.metrics.vault_answered();
                    return Err(VaultError::new(
                        Reason::Login,
                        format!(
                            "login: {status} (auth/{mount}, role {role}): is the role bound to \
                             this instance's or task's IAM role?"
                        ),
                    ));
                }
                let v: serde_json::Value = r.json().await.map_err(|_| {
                    VaultError::new(Reason::Malformed, "login: an unreadable answer")
                })?;
                let token = v["auth"]["client_token"]
                    .as_str()
                    .map(|t| Zeroizing::new(t.to_owned()))
                    .ok_or_else(|| {
                        VaultError::new(Reason::Malformed, "login: no token in the answer")
                    })?;
                // `lease_duration` is seconds; 0 is a token that does not expire.
                let ttl = v["auth"]["lease_duration"]
                    .as_u64()
                    .filter(|s| *s > 0)
                    .map(Duration::from_secs);
                Ok((token, ttl))
            }
        }
    }

    /// Log in, with the bounded retries of the policy for a Vault that does not answer.
    async fn login(&self) -> Result<Arc<Session>, VaultError> {
        let mut last = None;
        for attempt in 0..self.policy.max_attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(self.policy.backoff * 2u32.pow(attempt - 1)).await;
            }
            match self.login_once().await {
                Ok((token, ttl)) => {
                    self.metrics.vault_answered();
                    self.metrics.logins.fetch_add(1, Ordering::Relaxed);
                    let created = Instant::now();
                    let s = Arc::new(Session {
                        token,
                        generation: self.generations.fetch_add(1, Ordering::Relaxed) + 1,
                        created,
                        renew_at: ttl.map(|t| created + Policy::renew_after(t)),
                        expires_at: ttl.map(|t| created + t),
                    });
                    *self.session.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(Arc::clone(&s));
                    tracing::debug!(generation = s.generation, "logged in to vault");
                    return Ok(s);
                }
                Err(e) if e.reason.vault_is_down() => {
                    self.metrics.vault_silent(&e.detail);
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| VaultError::new(Reason::Login, "no login attempt")))
    }

    /// The session in use if it is not the refused one and not due for renewal.
    fn fresh(&self, refused: Option<u64>) -> Option<Arc<Session>> {
        let guard = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let s = guard.as_ref()?;
        if Some(s.generation) == refused {
            return None;
        }
        if s.renew_at.is_some_and(|t| Instant::now() >= t) {
            return None;
        }
        Some(Arc::clone(s))
    }

    /// A usable session (module documentation, "One login at a time"). `refused` is the
    /// generation Vault has just refused, if any.
    async fn session(&self, refused: Option<u64>) -> Result<Arc<Session>, VaultError> {
        if let Some(s) = self.fresh(refused) {
            return Ok(s);
        }
        let mut failed = self.gate.lock().await;
        // Another request may have logged in while this one waited.
        if let Some(s) = self.fresh(refused) {
            return Ok(s);
        }
        let current = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some((at, e)) = &*failed
            && at.elapsed() < self.policy.relogin_cooldown
        {
            return Err(e.clone());
        }
        if let (Some(cur), Some(r)) = (&current, refused)
            && cur.generation == r
            && cur.created.elapsed() < self.policy.relogin_cooldown
        {
            return Err(VaultError::new(
                Reason::Denied,
                "Vault refused a token issued moments ago; not logging in again yet",
            ));
        }
        if refused.is_some() {
            self.metrics
                .relogins_after_refusal
                .fetch_add(1, Ordering::Relaxed);
        }
        match self.login().await {
            Ok(s) => {
                *failed = None;
                Ok(s)
            }
            Err(e) => {
                *failed = Some((Instant::now(), e.clone()));
                // A renewal that failed, with the old token still alive: keep using it.
                if refused.is_none()
                    && let Some(cur) = current
                    && cur.expires_at.is_none_or(|t| Instant::now() < t)
                {
                    tracing::warn!(
                        reason = e.reason.as_str(),
                        "renewing the vault token failed; using the current one"
                    );
                    let kept = Arc::new(Session {
                        renew_at: Some(Instant::now() + self.policy.renew_retry),
                        ..(*cur).clone()
                    });
                    *self.session.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(Arc::clone(&kept));
                    return Ok(kept);
                }
                Err(e)
            }
        }
    }

    /// Log in again if the token is due for renewal, or if there is none.
    pub async fn keep_fresh(&self) -> Result<(), VaultError> {
        self.session(None).await.map(|_| ())
    }

    async fn attempts(
        &self,
        token: &str,
        method: &reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Reply, VaultError> {
        let mut last = None;
        for attempt in 0..self.policy.max_attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(self.policy.backoff * 2u32.pow(attempt - 1)).await;
            }
            let mut req = self
                .http
                .request(method.clone(), self.url(path))
                .header("X-Vault-Token", token)
                .timeout(self.policy.attempt_timeout);
            if let Some(b) = body {
                req = req.json(b);
            }
            match req.send().await {
                Ok(r) => {
                    let status = r.status();
                    if status.is_server_error() || status.as_u16() == 429 {
                        self.metrics.vault_silent(status.as_str());
                        last = Some(VaultError::new(
                            Reason::Unavailable,
                            format!("{} {path}: {status}", method.as_str()),
                        ));
                        continue;
                    }
                    self.metrics.vault_answered();
                    let body = Zeroizing::new(r.text().await.map_err(|e| {
                        VaultError::new(
                            Reason::Malformed,
                            format!("{path}: an unreadable answer ({})", e.without_url()),
                        )
                    })?);
                    return Ok(Reply {
                        status: status.as_u16(),
                        body,
                    });
                }
                Err(e) => {
                    let e = self.classify(&format!("{} {path}", method.as_str()), e);
                    self.metrics.vault_silent(&e.detail);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| VaultError::new(Reason::Unreachable, "no attempt")))
    }

    /// Send `method` `/v1/<path>` with `body`, under a token (module documentation). The answer is
    /// whatever Vault said, 4xx included; an error is Vault not answering, or the login failing.
    /// A 403 is answered only after one fresh login.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Reply, VaultError> {
        let mut refused = None;
        loop {
            let s = self.session(refused).await?;
            let reply = self.attempts(&s.token, &method, path, body).await?;
            if reply.status == 403 && refused.is_none() {
                refused = Some(s.generation);
                continue;
            }
            return Ok(reply);
        }
    }
}

#[cfg(test)]
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

    #[test]
    fn a_token_is_renewed_after_two_thirds_of_its_lease() {
        assert_eq!(
            Policy::renew_after(Duration::from_secs(60)),
            Duration::from_secs(40)
        );
    }

    #[test]
    fn the_histogram_counts_by_bucket_and_failures_by_reason() {
        let m = Metrics::default();
        m.observe(Duration::from_micros(500), None);
        m.observe(Duration::from_millis(30), None);
        m.observe(Duration::from_secs(9), Some(Reason::Timeout));
        assert_eq!(m.observed(), 3);
        assert_eq!(m.failures(Reason::Timeout), 1);
        let text = m.render();
        assert!(text.contains("le=\"1000\"} 1"), "{text}");
        assert!(text.contains("le=\"50000\"} 2"), "{text}");
        assert!(text.contains("le=\"+Inf\"} 3"), "{text}");
        assert!(text.contains("reason=\"timeout\"} 1"), "{text}");
        assert!(text.contains("signer_vault_ready 1"), "{text}");
    }

    #[test]
    fn readiness_follows_whether_vault_answers() {
        let m = Metrics::default();
        assert!(m.readiness().ready);
        m.vault_silent("connection refused");
        m.vault_silent("connection refused");
        let r = m.readiness();
        assert!(!r.ready && r.vault_unreachable && r.consecutive_failures == 2);
        m.vault_answered();
        let r = m.readiness();
        assert!(r.ready && r.consecutive_failures == 0 && r.last_answer_secs_ago == Some(0));
        m.key_drifted("exportable");
        assert!(!m.readiness().ready);
    }
}
