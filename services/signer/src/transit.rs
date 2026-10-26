// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Vault Transit engine (`F-042`, `NFR-8`): an API provider whose Ed25519 keys never leave Vault.
//! The component that signs asks Vault to sign, each time, and checks the answer. It is an engine like
//! the sidecar ([`crate::engine`]): a component's configuration names it, and what calls a key cannot
//! tell the two apart.
//!
//! ```json
//! { "engines": { "vault": { "vault_transit": {
//!       "vault": { "address": "https://vault.example.com",
//!                  "auth": { "aws_iam": { "mount": "aws", "role": "feed-signer", "region": "us-east-1" } } },
//!       "mount": "transit" } } },
//!   "keys": { "feed": { "engine": "vault", "key": { "name": "lane-1", "version": 1 } } } }
//! ```
//!
//! The core host is not a user of it: its keys may use the sidecar only, and its build does not have
//! this module.
//!
//! **What is checked, and when.**
//!
//! - At open, with the version pinned (required, never zero, never "the latest"): `<mount>/keys/<name>`
//!   is an `ed25519` key that is not exportable, not derived, not allowed a plaintext backup, and
//!   supports signing; the pinned version exists and is not below `min_encryption_version` or
//!   `min_available_version`; its public key is the one this process publishes.
//! - On every signature: Vault signs the bytes as given (no `prehashed`, no `hash_algorithm`: pure
//!   Ed25519); the answer is `vault:v<N>:<base64>`; `N` is the pinned version; and the signature
//!   verifies against the cached public key *here*, before it is returned.
//! - Every [`Policy::heartbeat`]: the token is kept fresh and the key read again. A key that has become
//!   exportable (Vault allows that after creation, never back), or whose pinned version's public key
//!   or floor has changed, makes the engine not ready. Signatures are still verified, so none that is
//!   wrong gets out.
//!
//! **Failure ends a request, not the process.** The error is a [`ClientError`]: Vault not answering is
//! `Refused(Unavailable)`, which is transient; a refusal that remains after one new login is
//! `Refused(NotGranted)`; a wrong or unreadable answer is `Protocol`. What a caller does next is its own
//! patience, and a key held here is used through `try_*` ([`crate::client::RemoteEd25519::try_sign`]).
//!
//! **Threads.** The interface is blocking. Vault is spoken to by a thread of the engine's own, with a
//! small runtime, so a caller on an async runtime's blocking pool, or off any runtime, signs the same way.
//!
//! **Cost, stated plainly.** Each signature is a round trip to Vault over the network where the sidecar's
//! is a call on a local socket. Vault is then in the path of every signature, and its outage is the
//! component's outage.
//!
//! **Logging.** The payload and the signature are never logged at `info` or above, nor put in an error;
//! neither is a token.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use crate::client::ClientError;
use crate::engine::config::KindInfo;
use crate::engine::{Engine, EngineKind, Health, KeyHandle};
use crate::proto::{Op, Refusal, Scheme};
use crate::vault_config::{VaultConfig, name_ok};
pub use crate::vault_session::{Metrics, Policy, Readiness, Reason, VaultError, VaultSession};

impl From<VaultError> for ClientError {
    fn from(e: VaultError) -> ClientError {
        match e.reason {
            r if r.vault_is_down() => ClientError::Refused(Refusal::Unavailable(e.to_string())),
            Reason::Denied => ClientError::Refused(Refusal::NotGranted),
            _ => ClientError::Protocol(e.to_string()),
        }
    }
}

/// The engine's own configuration: Vault, and the Transit mount.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitConfig {
    /// Vault, and the login.
    pub vault: VaultConfig,
    /// The Transit engine's mount (`transit`).
    pub mount: String,
}

impl TransitConfig {
    fn check(&self, development: bool) -> Result<(), String> {
        if !name_ok(&self.mount, true) {
            return Err(format!(
                "mount {:?}: letters, digits, - _ . and / between names",
                self.mount
            ));
        }
        self.vault.check(development)
    }
}

/// A Transit key's locator: its name in the mount and the version pinned.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locator {
    /// The key's name.
    pub name: String,
    /// The version to sign with and publish. Required, never zero, never "the latest": a new version
    /// is a new public key, so a rotation is a decision, and it is made in the configuration.
    #[serde(default)]
    pub version: u32,
}

impl Locator {
    /// Read and check a locator.
    pub fn parse(v: &Value) -> Result<Locator, String> {
        let l = Locator::deserialize(v).map_err(|e| {
            format!("a Transit key is {{\"name\": \"<key>\", \"version\": <n>}}: {e}")
        })?;
        if !name_ok(&l.name, false) {
            return Err(format!("key name {:?}: letters, digits, - _ and .", l.name));
        }
        if l.version == 0 {
            return Err(
                "version is required and not zero: pin the key version to sign with \
                        (a new version is a new public key)"
                    .into(),
            );
        }
        Ok(l)
    }
}

/// The Vault Transit kind, as a configuration names it (`"vault_transit"`).
pub static VAULT_TRANSIT: KindInfo = KindInfo {
    name: "vault_transit",
    kind: EngineKind::ApiProvider,
    schemes: &[Scheme::Ed25519],
    check_engine: |c, development| {
        TransitConfig::deserialize(c)
            .map_err(|e| format!("a vault_transit engine is {{vault, mount}}: {e}"))?
            .check(development)
    },
    check_locator: |_, l| Locator::parse(l).map(|_| ()),
    connect: |c, development| {
        let config = TransitConfig::deserialize(c).map_err(|e| e.to_string())?;
        config.check(development)?;
        let session = VaultSession::new(&config.vault)?;
        TransitEngine::with_session(session, &config.mount).map(|e| Arc::new(e) as Arc<dyn Engine>)
    },
};

/// A Transit key, as Vault describes it (`GET <mount>/keys/<name>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitKeyInfo {
    /// Its type (`ed25519`, `ecdsa-p256`, `aes256-gcm96`, …).
    pub key_type: String,
    /// Whether the key may be exported out of Vault.
    pub exportable: bool,
    /// Whether it is a derived key (its public half would depend on a context).
    pub derived: bool,
    /// Whether a plaintext backup is allowed.
    pub allow_plaintext_backup: bool,
    /// Whether it signs.
    pub supports_signing: bool,
    /// The newest version.
    pub latest_version: u32,
    /// The lowest version that may encrypt or sign (0: none set).
    pub min_encryption_version: u32,
    /// The lowest version still kept (0: none set).
    pub min_available_version: u32,
    /// The public key of each version that has one, raw.
    pub public_keys: BTreeMap<u32, Vec<u8>>,
}

fn malformed(what: &str) -> VaultError {
    VaultError::new(Reason::Malformed, what)
}

/// What Vault said about a key: `data.type`, `data.exportable`, and for an asymmetric key
/// `data.keys.<version>.public_key`, base64.
pub fn parse_key_info(body: &str) -> Result<TransitKeyInfo, VaultError> {
    let v: Value =
        serde_json::from_str(body).map_err(|_| malformed("the key: an unreadable answer"))?;
    let d = &v["data"];
    let key_type = d["type"]
        .as_str()
        .ok_or_else(|| malformed("the key: no type"))?
        .to_owned();
    let exportable = d["exportable"]
        .as_bool()
        .ok_or_else(|| malformed("the key: no exportable flag"))?;
    let mut public_keys = BTreeMap::new();
    for (k, entry) in d["keys"].as_object().into_iter().flatten() {
        let (Ok(version), Some(b64)) = (k.parse::<u32>(), entry["public_key"].as_str()) else {
            continue;
        };
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| malformed("the key: a public key that is not base64"))?;
        public_keys.insert(version, raw);
    }
    let num = |name: &str| {
        d[name]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    let latest_version = match num("latest_version") {
        0 => public_keys.keys().next_back().copied().unwrap_or(0),
        n => n,
    };
    Ok(TransitKeyInfo {
        key_type,
        exportable,
        derived: d["derived"].as_bool().unwrap_or(false),
        allow_plaintext_backup: d["allow_plaintext_backup"].as_bool().unwrap_or(false),
        supports_signing: d["supports_signing"].as_bool().unwrap_or(true),
        latest_version,
        min_encryption_version: num("min_encryption_version"),
        min_available_version: num("min_available_version"),
        public_keys,
    })
}

/// The 64 bytes in a Transit signature `vault:v<N>:<base64>`, if `N` is `version`.
pub fn parse_signature(answer: &str, version: u32) -> Result<[u8; 64], VaultError> {
    let bad = || malformed("the signature is not vault:v<N>:<base64>");
    let mut parts = answer.splitn(3, ':');
    let (Some("vault"), Some(v), Some(b64)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    let n: u32 = v
        .strip_prefix('v')
        .and_then(|n| n.parse().ok())
        .ok_or_else(bad)?;
    if n != version {
        return Err(VaultError::new(
            Reason::Version,
            format!("Vault signed with version {n}, the pinned version is {version}"),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| bad())?;
    bytes
        .try_into()
        .map_err(|_| malformed("an Ed25519 signature is 64 bytes"))
}

/// Vault's refusal in words: its first error, short. Vault never echoes the payload.
fn vault_said(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["errors"][0]
                .as_str()
                .map(|s| s.chars().take(160).collect())
        })
        .unwrap_or_default()
}

fn refused(what: &str, status: u16, body: &str) -> VaultError {
    let reason = if status == 403 {
        Reason::Denied
    } else {
        Reason::Rejected
    };
    VaultError::new(reason, format!("{what}: {status} {}", vault_said(body)))
}

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A thread with a runtime of its own, which Vault is spoken to from (module documentation, "Threads").
struct Worker {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
}

impl Worker {
    fn start() -> Result<Worker, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("vault transit: {e}"))?;
        let (jobs, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        std::thread::Builder::new()
            .name("vault-transit".into())
            .spawn(move || {
                rt.block_on(async move {
                    while let Some(job) = rx.recv().await {
                        tokio::spawn(job);
                    }
                });
            })
            .map_err(|e| format!("vault transit: {e}"))?;
        Ok(Worker { jobs })
    }

    fn spawn(&self, job: impl Future<Output = ()> + Send + 'static) {
        let _ = self.jobs.send(Box::pin(job));
    }

    fn run<T: Send + 'static>(
        &self,
        work: impl Future<Output = T> + Send + 'static,
        wait: Duration,
    ) -> Result<T, VaultError> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.spawn(async move {
            let _ = tx.send(work.await);
        });
        rx.recv_timeout(wait)
            .map_err(|_| VaultError::new(Reason::Timeout, "no answer from the transit thread"))
    }
}

/// The Vault Transit engine (module documentation).
pub struct TransitEngine {
    session: Arc<VaultSession>,
    mount: String,
    worker: Arc<Worker>,
}

impl TransitEngine {
    /// An engine over a session already made (its policy, its login), for the mount `mount`.
    pub fn with_session(session: VaultSession, mount: &str) -> Result<TransitEngine, String> {
        Ok(TransitEngine {
            session: Arc::new(session),
            mount: mount.to_owned(),
            worker: Arc::new(Worker::start()?),
        })
    }

    /// Its metrics.
    pub fn metrics(&self) -> Arc<Metrics> {
        self.session.metrics()
    }

    fn wait(&self) -> Duration {
        self.session.policy().deadline + Duration::from_secs(2)
    }
}

impl Engine for TransitEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::ApiProvider
    }

    fn schemes(&self) -> &[Scheme] {
        &[Scheme::Ed25519]
    }

    fn open(&self, scheme: Scheme, locator: &Value) -> Result<Arc<dyn KeyHandle>, ClientError> {
        if scheme != Scheme::Ed25519 {
            return Err(ClientError::Refused(Refusal::WrongScheme));
        }
        let l = Locator::parse(locator).map_err(ClientError::Config)?;
        let (session, mount) = (Arc::clone(&self.session), self.mount.clone());
        let signer = self
            .worker
            .run(
                async move { TransitSigner::load(session, &mount, &l.name, l.version).await },
                self.wait(),
            )?
            .map_err(ClientError::Config)?;
        let signer = Arc::new(signer);
        let every = self.session.policy().heartbeat;
        if !every.is_zero() {
            let weak: Weak<TransitSigner> = Arc::downgrade(&signer);
            self.worker.spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let Some(s) = weak.upgrade() else { return };
                    s.check().await;
                }
            });
        }
        Ok(Arc::new(TransitKey {
            signer,
            worker: Arc::clone(&self.worker),
        }))
    }

    fn health(&self) -> Health {
        let r = self.session.metrics().readiness();
        if r.ready {
            Health::ready()
        } else if r.vault_unreachable {
            Health::not_ready("Vault does not answer")
        } else {
            Health::not_ready("the Transit key is not what was pinned")
        }
    }

    fn metrics_text(&self) -> String {
        self.session.metrics().render()
    }
}

/// An Ed25519 key in Transit, in async: its address, pinned version and public half.
struct TransitSigner {
    session: Arc<VaultSession>,
    mount: String,
    name: String,
    version: u32,
    public: ed25519_dalek::VerifyingKey,
}

impl TransitSigner {
    async fn key_info(
        session: &VaultSession,
        mount: &str,
        name: &str,
    ) -> Result<TransitKeyInfo, VaultError> {
        let path = format!("{mount}/keys/{name}");
        let r = session.send(reqwest::Method::GET, &path, None).await?;
        if r.status != 200 {
            return Err(refused(&format!("GET {path}"), r.status, &r.body));
        }
        parse_key_info(&r.body)
    }

    /// The checks of the module documentation on what Vault says of the key.
    fn check_key(
        info: &TransitKeyInfo,
        mount: &str,
        name: &str,
        version: u32,
    ) -> Result<ed25519_dalek::VerifyingKey, String> {
        let at = format!("{mount}/keys/{name}");
        if info.key_type != "ed25519" {
            return Err(format!(
                "{at}: the type is {:?}, not \"ed25519\"",
                info.key_type
            ));
        }
        if info.exportable {
            return Err(format!(
                "{at}: the key is exportable, so it could leave Vault (NFR-8); create it with \
                 exportable=false (exportable can be turned on later, never back, so a key once \
                 exportable is not to be used)"
            ));
        }
        if info.derived {
            return Err(format!(
                "{at}: a derived key's public half depends on a context"
            ));
        }
        if info.allow_plaintext_backup {
            return Err(format!(
                "{at}: plaintext backup is allowed, so the key could leave Vault"
            ));
        }
        if !info.supports_signing {
            return Err(format!("{at}: the key does not support signing"));
        }
        let raw = info.public_keys.get(&version).ok_or_else(|| {
            format!(
                "{at}: version {version} does not exist (the latest is {})",
                info.latest_version
            )
        })?;
        for (floor, what) in [
            (info.min_encryption_version, "min_encryption_version"),
            (info.min_available_version, "min_available_version"),
        ] {
            if floor != 0 && version < floor {
                return Err(format!("{at}: version {version} is below {what} ({floor})"));
            }
        }
        let raw: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| format!("{at}: the public key of version {version} is not 32 bytes"))?;
        ed25519_dalek::VerifyingKey::from_bytes(&raw).map_err(|_| {
            format!("{at}: the public key of version {version} is not an Ed25519 point")
        })
    }

    async fn load(
        session: Arc<VaultSession>,
        mount: &str,
        name: &str,
        version: u32,
    ) -> Result<TransitSigner, String> {
        let info = Self::key_info(&session, mount, name)
            .await
            .map_err(|e| format!("{mount}/keys/{name}: {e}"))?;
        let public = Self::check_key(&info, mount, name, version)?;
        Ok(TransitSigner {
            session,
            mount: mount.to_owned(),
            name: name.to_owned(),
            version,
            public,
        })
    }

    /// The Ed25519 signature over `msg`: Vault signs, this process verifies (module documentation).
    /// Observed in the [`Metrics`], retries and logins included.
    async fn sign(&self, msg: &[u8]) -> Result<[u8; 64], VaultError> {
        let started = Instant::now();
        let out = match tokio::time::timeout(self.session.policy().deadline, self.sign_checked(msg))
            .await
        {
            Ok(r) => r,
            Err(_) => Err(VaultError::new(
                Reason::Timeout,
                "the signature was not done in time",
            )),
        };
        self.session
            .metrics()
            .observe(started.elapsed(), out.as_ref().err().map(|e| e.reason));
        if let Err(e) = &out {
            tracing::warn!(
                mount = %self.mount, key = %self.name, version = self.version,
                reason = e.reason.as_str(), "transit sign failed"
            );
        }
        out
    }

    async fn sign_checked(&self, msg: &[u8]) -> Result<[u8; 64], VaultError> {
        let path = format!("{}/sign/{}", self.mount, self.name);
        // No `prehashed` and no `hash_algorithm`: Vault signs these bytes, Ed25519 as it is.
        let body = serde_json::json!({
            "input": base64::engine::general_purpose::STANDARD.encode(msg),
            "key_version": self.version,
        });
        let r = self
            .session
            .send(reqwest::Method::POST, &path, Some(&body))
            .await?;
        if r.status != 200 {
            return Err(refused(&format!("POST {path}"), r.status, &r.body));
        }
        let v: Value = serde_json::from_str(&r.body)
            .map_err(|_| malformed("the signature: an unreadable answer"))?;
        let answer = v["data"]["signature"]
            .as_str()
            .ok_or_else(|| malformed("the answer has no signature"))?;
        let sig = parse_signature(answer, self.version)?;
        self.public
            .verify_strict(msg, &ed25519_dalek::Signature::from_bytes(&sig))
            .map_err(|_| {
                VaultError::new(
                    Reason::BadSignature,
                    "the signature Vault gave does not verify against the key's public half",
                )
            })?;
        Ok(sig)
    }

    /// One heartbeat: the token kept fresh, and the key read again against what was pinned.
    async fn check(&self) {
        let work = async {
            self.session.keep_fresh().await?;
            let info = Self::key_info(&self.session, &self.mount, &self.name).await?;
            match Self::check_key(&info, &self.mount, &self.name, self.version) {
                Ok(p) if p == self.public => {}
                Ok(_) => self
                    .session
                    .metrics()
                    .key_drifted("the pinned version's public key changed"),
                Err(e) => self.session.metrics().key_drifted(&e),
            }
            Ok::<(), VaultError>(())
        };
        match tokio::time::timeout(self.session.policy().deadline, work).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!(reason = e.reason.as_str(), "transit heartbeat failed"),
            Err(_) => tracing::debug!("transit heartbeat timed out"),
        }
    }
}

/// An open Transit key.
pub struct TransitKey {
    signer: Arc<TransitSigner>,
    worker: Arc<Worker>,
}

impl core::fmt::Debug for TransitKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TransitKey")
            .field("mount", &self.signer.mount)
            .field("key", &self.signer.name)
            .field("version", &self.signer.version)
            .finish_non_exhaustive()
    }
}

impl KeyHandle for TransitKey {
    fn kind(&self) -> EngineKind {
        EngineKind::ApiProvider
    }

    fn scheme(&self) -> Scheme {
        Scheme::Ed25519
    }

    fn public(&self) -> Vec<u8> {
        self.signer.public.to_bytes().to_vec()
    }

    fn call(&self, op: &Op) -> Result<Vec<u8>, ClientError> {
        match op {
            Op::Public => Ok(self.public()),
            Op::Sign(msg) => {
                let signer = Arc::clone(&self.signer);
                let msg = msg.clone();
                let wait = self.signer.session.policy().deadline + Duration::from_secs(2);
                let sig = self
                    .worker
                    .run(async move { signer.sign(&msg).await }, wait)?
                    .map_err(ClientError::from)?;
                Ok(sig.to_vec())
            }
            _ => Err(ClientError::Refused(Refusal::WrongScheme)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfr_8_a_transit_signature_is_vault_version_base64() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        assert_eq!(
            parse_signature(&format!("vault:v3:{b64}"), 3).unwrap(),
            [7u8; 64]
        );
        let e = parse_signature(&format!("vault:v2:{b64}"), 3).unwrap_err();
        assert_eq!(e.reason, Reason::Version);
        for bad in [
            "",
            "vault:v3",
            "x:v3:AAAA",
            "vault:3:AAAA",
            "vault:v3:!!",
            "vault:v3:AAAA",
        ] {
            assert_eq!(
                parse_signature(bad, 3).unwrap_err().reason,
                Reason::Malformed,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_key_is_read_from_vaults_answer() {
        let pk = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let body = serde_json::json!({ "data": {
            "type": "ed25519", "exportable": false, "latest_version": 2,
            "min_encryption_version": 1, "derived": false,
            "keys": { "1": { "public_key": pk, "creation_time": "t" }, "2": { "public_key": pk } }
        }})
        .to_string();
        let k = parse_key_info(&body).unwrap();
        assert_eq!(k.key_type, "ed25519");
        assert!(!k.exportable && !k.derived && !k.allow_plaintext_backup && k.supports_signing);
        assert_eq!((k.latest_version, k.min_encryption_version), (2, 1));
        assert_eq!(k.public_keys.keys().copied().collect::<Vec<_>>(), [1, 2]);
        assert!(parse_key_info("{}").is_err());
        let sym = serde_json::json!({ "data": { "type": "aes256-gcm96", "exportable": false,
                                                 "keys": { "1": 1700000000 } }});
        assert!(
            parse_key_info(&sym.to_string())
                .unwrap()
                .public_keys
                .is_empty()
        );
    }

    #[test]
    fn nfr_8_only_a_key_that_cannot_leave_vault_and_has_the_pinned_version_is_taken() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]).verifying_key();
        let good = || TransitKeyInfo {
            key_type: "ed25519".into(),
            exportable: false,
            derived: false,
            allow_plaintext_backup: false,
            supports_signing: true,
            latest_version: 2,
            min_encryption_version: 0,
            min_available_version: 0,
            public_keys: [(1, key.to_bytes().to_vec())].into_iter().collect(),
        };
        let check = |i: &TransitKeyInfo, v| TransitSigner::check_key(i, "transit", "k", v);
        assert_eq!(check(&good(), 1), Ok(key));
        let refused = |edit: fn(&mut TransitKeyInfo), v: u32, says: &str| {
            let mut i = good();
            edit(&mut i);
            let e = check(&i, v).unwrap_err();
            assert!(e.contains(says), "{says}: {e}");
        };
        refused(|i| i.exportable = true, 1, "exportable");
        refused(|i| i.key_type = "ecdsa-p256".into(), 1, "not \"ed25519\"");
        refused(|i| i.derived = true, 1, "derived");
        refused(|i| i.allow_plaintext_backup = true, 1, "plaintext backup");
        refused(
            |i| i.supports_signing = false,
            1,
            "does not support signing",
        );
        refused(|_| {}, 2, "version 2 does not exist");
        refused(
            |i| i.min_encryption_version = 2,
            1,
            "below min_encryption_version",
        );
        refused(
            |i| i.min_available_version = 2,
            1,
            "below min_available_version",
        );
    }

    #[test]
    fn the_locator_pins_a_version() {
        let l = |v: Value| Locator::parse(&v);
        assert!(l(serde_json::json!({ "name": "lane-1", "version": 1 })).is_ok());
        assert!(
            l(serde_json::json!({ "name": "lane-1" }))
                .unwrap_err()
                .contains("pin the key version")
        );
        assert!(l(serde_json::json!({ "name": "lane-1", "version": 0 })).is_err());
        assert!(l(serde_json::json!({ "name": "a/b", "version": 1 })).is_err());
        assert!(l(serde_json::json!({ "name": "k", "version": 1, "latest": true })).is_err());
    }
}
