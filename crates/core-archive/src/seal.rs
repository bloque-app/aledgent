// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The archive's seal (`F-044`, `features/F-044-archive-sealing/plan.md`): aledgent encrypts every
//! object it archives, so the archive depends on no key of the object store's provider.
//!
//! **What is sealed.** The exact bytes a store would have held (a segment, a checkpoint, a
//! chainspec, a report), under the same name. The records, their format and their hash chain do
//! not change: a sealed archive reads back as the same records, and [`crate::fetch()`] checks them
//! as before.
//!
//! **The envelope** (format 1):
//!
//! ```text
//! aledgent-seal/v1\n
//! {"name":…,"bytes":…,"sha256":…,"clear":K}\n      the seal line: what the object holds
//! <the first K bytes of the plaintext, in clear>
//! <an age v1 file of the whole plaintext>
//! ```
//!
//! The age file ([`crate::age`]) has one stanza per way to open it: `aledgent-transit <key>
//! <version>` (the file key wrapped with RSA-OAEP-SHA-256 to the public half of a Vault Transit
//! key: the writer needs no Vault, and only Vault opens it), one `X25519` stanza per offline
//! recovery key (the stock `age` tool opens it, with no Vault and nothing of aledgent's), and
//! `aledgent-seal <sha256 of the seal line>`, so the header's MAC covers the seal line.
//!
//! **In clear** (`K`): a segment's or a checkpoint's record header (the book, the offsets and
//! SHA-256 of its payloads, the restore point, its times), and the whole of a `verify-genesis`
//! report; nothing of any other object. That is what the writer reads back after a restart, with
//! no key: the core host never holds a key that opens the archive ([`ObjectStore::head`]). For
//! a keyless reader those bytes, and the seal line, are **not authenticated**: they are what the
//! store returns, kept honest by its write-once and Object Lock. A reader with a key checks them
//! (the MAC covers the seal line; the plaintext must match it and begin with the clear bytes).
//! Payloads (the log's entries and replies), checkpoints' state and chainspecs are only in the age
//! file.
//!
//! **File keys.** A writer seals under one file key for an epoch (at most [`EPOCH_OBJECTS`]
//! objects or [`EPOCH`]), each object with its own payload nonce, so a restore asks Vault once an
//! epoch rather than once a segment; a reader keeps what it opened ([`Sealed`]).
//!
//! **Write once.** Sealing is randomised, so an exact retry seals different bytes. When the store
//! refuses a name already taken, the seal line of what is there decides: the same name, length
//! and SHA-256 is the same object (`created: false`), anything else a conflict.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rsa::pkcs8::DecodePublicKey as _;
use rsa::traits::PublicKeyParts as _;
use rsa::{Oaep, RsaPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

use crate::age::{self, FileKey, Identity, Recipient, Stanza};
use crate::record::{CHECKPOINTS, SEGMENTS, sha256_hex};
use crate::store::{Head, ObjectStore, Put, StoreError, check_name};

/// The envelope's first line.
pub const MAGIC_LINE: &[u8] = b"aledgent-seal/v1\n";

/// The stanza of the file key wrapped to a Vault Transit key.
pub const TRANSIT_KIND: &str = "aledgent-transit";

/// The stanza that binds the seal line to the header's MAC.
pub const SEAL_KIND: &str = "aledgent-seal";

/// The most bytes in clear: a record's header is well under it.
pub const MAX_CLEAR: usize = 256 * 1024;

/// The most bytes a seal line may take.
const MAX_SEAL_LINE: usize = 4096;

/// The most objects sealed under one file key.
pub const EPOCH_OBJECTS: u64 = 4096;

/// The longest a file key is used for.
pub const EPOCH: Duration = Duration::from_secs(3600);

/// The smallest Transit RSA key the writer accepts: the archive is kept for as long as the book
/// exists (`NFR-22`). Vault's `rsa-4096` is the one to create.
pub const MIN_RSA_BITS: usize = 3072;

/// The environment variable naming the recovery identity file a reader opens with (an
/// `age-keygen` file: `AGE-SECRET-KEY-1…` lines).
pub const RECOVERY_IDENTITY_ENV: &str = "ALEDGENT_ARCHIVE_RECOVERY_IDENTITY";

/// The environment variable naming the file that holds a reader's Vault token, kept fresh by an
/// agent on the host (Vault Agent's token sink), read again before every call.
pub const VAULT_TOKEN_FILE_ENV: &str = "ALEDGENT_ARCHIVE_VAULT_TOKEN_FILE";

/// The most bytes an envelope of a plaintext of at most `max` bytes may take.
pub fn sealed_bound(max: u64) -> u64 {
    let chunks = max / age::CHUNK as u64 + 1;
    max + max.min(MAX_CLEAR as u64)
        + chunks * 16
        + 16
        + (age::MAX_HEADER + MAX_SEAL_LINE + MAGIC_LINE.len()) as u64
}

/// The seal line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealLine {
    name: String,
    bytes: u64,
    sha256: String,
    clear: u64,
}

/// How many leading bytes of `plaintext`, published as `name`, are kept in clear (module
/// documentation).
pub fn clear_len(name: &str, plaintext: &[u8]) -> usize {
    let base = name.rsplit('/').next().unwrap_or(name);
    let in_dir = |dir: &str| name.starts_with(dir) || name.contains(&format!("/{dir}"));
    if (in_dir(SEGMENTS) && base.ends_with(".seg"))
        || (in_dir(CHECKPOINTS) && base.ends_with(".ckpt"))
    {
        return crate::record::header_len(plaintext)
            .filter(|n| *n <= MAX_CLEAR)
            .unwrap_or(0);
    }
    if in_dir("chainspec/") && base.ends_with(".verified.json") && plaintext.len() <= MAX_CLEAR {
        return plaintext.len();
    }
    0
}

/// A Vault Transit key the writer seals to: its name, its version, and that version's public key.
pub struct TransitKey {
    name: String,
    version: u32,
    key: RsaPublicKey,
}

impl core::fmt::Debug for TransitKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "TransitKey({} v{}, {} bits)",
            self.name,
            self.version,
            self.key.size() * 8
        )
    }
}

fn check_vault_name(what: &str, s: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{what} {s:?}: letters, digits, `-`, `_` and `.` only"
        ))
    }
}

impl TransitKey {
    /// The key `name` at `version`, whose public key is `pem` (SPKI, as Vault's
    /// `GET <mount>/keys/<name>` gives it).
    pub fn new(name: &str, version: u32, pem: &str) -> Result<TransitKey, String> {
        check_vault_name("the Transit key", name)?;
        if version == 0 {
            return Err("the Transit key's version starts at 1".into());
        }
        let key = RsaPublicKey::from_public_key_pem(pem.trim())
            .map_err(|_| "the Transit key's public_key is not an RSA public key in PEM (SPKI)")?;
        if key.size() * 8 < MIN_RSA_BITS {
            return Err(format!(
                "the Transit key is {} bits: at least {MIN_RSA_BITS} (create it as rsa-4096)",
                key.size() * 8
            ));
        }
        Ok(TransitKey {
            name: name.to_owned(),
            version,
            key,
        })
    }

    /// Wrap `file_key` to it: RSA-OAEP with SHA-256 (and MGF1-SHA-256), which Vault's `decrypt`
    /// opens as `vault:v<version>:<base64>`.
    pub fn wrap(&self, file_key: &[u8; 16]) -> Result<Stanza, String> {
        let body = self
            .key
            .encrypt(
                &mut rand_core::OsRng,
                Oaep::new::<Sha256>(),
                file_key.as_slice(),
            )
            .map_err(|_| "the file key did not wrap to the Transit key")?;
        Ok(Stanza {
            kind: TRANSIT_KIND.into(),
            args: vec![self.name.clone(), self.version.to_string()],
            body,
        })
    }
}

/// Who a writer seals to: a Vault Transit key and at least one offline recovery key.
#[derive(Debug)]
pub struct Recipients {
    transit: TransitKey,
    recovery: Vec<Recipient>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipientsFile {
    transit: TransitFile,
    recovery: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitFile {
    key: String,
    version: u32,
    public_key: String,
}

impl Recipients {
    /// Both kinds, or refused: the archive must open with Vault and without it.
    pub fn new(transit: TransitKey, recovery: Vec<Recipient>) -> Result<Recipients, String> {
        if recovery.is_empty() {
            return Err("the seal needs at least one offline recovery key (age1…)".into());
        }
        Ok(Recipients { transit, recovery })
    }

    /// Read a seal file (`seal=<path>`): public keys only, nothing secret.
    ///
    /// ```json
    /// { "transit": { "key": "archive", "version": 1, "public_key": "-----BEGIN PUBLIC KEY-----…" },
    ///   "recovery": ["age1…"] }
    /// ```
    pub fn from_json(text: &str) -> Result<Recipients, String> {
        let f: RecipientsFile =
            serde_json::from_str(text).map_err(|e| format!("the seal file: {e}"))?;
        let transit = TransitKey::new(&f.transit.key, f.transit.version, &f.transit.public_key)?;
        let recovery = f
            .recovery
            .iter()
            .map(|r| Recipient::parse(r).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        Recipients::new(transit, recovery)
    }

    /// [`Recipients::from_json`] of the file at `path`.
    pub fn load(path: &Path) -> Result<Recipients, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("the seal file {}: {e}", path.display()))?;
        Recipients::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The Transit key it seals to.
    pub fn transit(&self) -> (&str, u32) {
        (&self.transit.name, self.transit.version)
    }

    fn wrap(&self, file_key: &[u8; 16]) -> Result<Vec<Stanza>, String> {
        let mut out = vec![self.transit.wrap(file_key)?];
        for r in &self.recovery {
            out.push(r.wrap(file_key).map_err(|e| e.to_string())?);
        }
        Ok(out)
    }
}

/// Opens the `aledgent-transit` stanza: Vault, or a test's stand-in.
pub trait TransitUnwrap: Send + Sync {
    /// The file key wrapped to `key` at `version`.
    ///
    /// [`StoreError::Unavailable`] when Vault did not answer (a reader retries),
    /// [`StoreError::Sealed`] when it answered and did not open it.
    fn unwrap(&self, key: &str, version: u32, wrapped: &[u8]) -> Result<FileKey, StoreError>;
}

/// Where a Vault token comes from. `Debug` shows no token.
pub enum TokenSource {
    /// A file an agent on the host keeps fresh, read again before every call.
    File(PathBuf),
    /// Given (tests).
    Fixed(Zeroizing<String>),
}

impl core::fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TokenSource::File(p) => write!(f, "TokenSource::File({})", p.display()),
            TokenSource::Fixed(_) => f.write_str("TokenSource::Fixed(..)"),
        }
    }
}

impl TokenSource {
    fn token(&self) -> Result<Zeroizing<String>, StoreError> {
        match self {
            TokenSource::Fixed(t) => Ok(t.clone()),
            TokenSource::File(p) => {
                let t = Zeroizing::new(std::fs::read_to_string(p).map_err(|e| {
                    StoreError::Unavailable(format!("the Vault token file {}: {e}", p.display()))
                })?);
                let t = Zeroizing::new(t.trim().to_owned());
                if t.is_empty() {
                    return Err(StoreError::Unavailable(format!(
                        "the Vault token file {} is empty",
                        p.display()
                    )));
                }
                Ok(t)
            }
        }
    }
}

/// Vault Transit's `decrypt`, for a reader (a restore, `opsctl archive verify`).
pub struct VaultTransit {
    address: String,
    mount: String,
    token: TokenSource,
    agent: ureq::Agent,
}

impl core::fmt::Debug for VaultTransit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "VaultTransit({}, mount {})", self.address, self.mount)
    }
}

/// Whether `address` may carry a Vault token: HTTPS to a host, or plain HTTP to this host (a dev
/// Vault). The authority is parsed: no user information (`http://localhost@evil/`), no path tricks.
fn check_vault_address(address: &str) -> Result<(), String> {
    let refuse = || {
        Err(format!(
            "vault={address:?}: https://<host>[:port], or http:// to 127.0.0.1, localhost or [::1] only"
        ))
    };
    let (https, rest) = match (
        address.strip_prefix("https://"),
        address.strip_prefix("http://"),
    ) {
        (Some(r), _) => (true, r),
        (None, Some(r)) => (false, r),
        _ => return refuse(),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty()
        || authority.contains(['@', '\\', '%'])
        || authority
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return refuse();
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((h, p)) => (format!("[{h}]"), p),
            None => return refuse(),
        },
        None => match authority.split_once(':') {
            Some((h, p)) => (h.to_owned(), &authority[h.len()..][..1 + p.len()]),
            None => (authority.to_owned(), ""),
        },
    };
    let port_ok = match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(p) => !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()),
    };
    let host_ok = !host.is_empty()
        && (host.starts_with('[')
            || host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'));
    let loopback = matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]"
    );
    if port_ok && host_ok && (https || loopback) {
        Ok(())
    } else {
        refuse()
    }
}

impl VaultTransit {
    /// Transit at `address` (`https://…`), mounted at `mount`.
    pub fn new(address: &str, mount: &str, token: TokenSource) -> Result<VaultTransit, String> {
        let address = address.trim_end_matches('/');
        check_vault_address(address)?;
        for part in mount.split('/') {
            check_vault_name("the Transit mount", part)?;
        }
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(10)))
                .build(),
        );
        Ok(VaultTransit {
            address: address.to_owned(),
            mount: mount.to_owned(),
            token,
            agent,
        })
    }

    fn call(&self, key: &str, body: &str) -> Result<(u16, Zeroizing<String>), StoreError> {
        let token = self.token.token()?;
        let url = format!("{}/v1/{}/decrypt/{key}", self.address, self.mount);
        let mut resp = self
            .agent
            .post(&url)
            .header("x-vault-token", token.as_str())
            .header("content-type", "application/json")
            .send(body)
            .map_err(|e| StoreError::Unavailable(format!("Vault Transit decrypt: {e}")))?;
        let status = resp.status().as_u16();
        // It holds the file key, in base64, when Vault answers: wiped when dropped.
        let text = Zeroizing::new(
            resp.body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_string()
                .map_err(|e| StoreError::Unavailable(format!("Vault Transit decrypt: {e}")))?,
        );
        Ok((status, text))
    }
}

impl TransitUnwrap for VaultTransit {
    fn unwrap(&self, key: &str, version: u32, wrapped: &[u8]) -> Result<FileKey, StoreError> {
        check_vault_name("the Transit key", key).map_err(StoreError::Sealed)?;
        let body = serde_json::json!({
            "ciphertext": format!("vault:v{version}:{}", STANDARD.encode(wrapped)),
        })
        .to_string();
        let what = format!("{}/{key} v{version}", self.mount);
        let mut pause = Duration::from_millis(100);
        let mut refused_once = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = self.call(key, &body);
            let retry = match &outcome {
                Err(_) => true,
                Ok((s, _)) => *s == 429 || *s >= 500,
            };
            if retry && attempt < 3 {
                std::thread::sleep(pause);
                pause *= 2;
                continue;
            }
            let (status, text) = outcome?;
            match status {
                200 => {
                    #[derive(Deserialize)]
                    struct Data {
                        plaintext: String,
                    }
                    #[derive(Deserialize)]
                    struct Resp {
                        data: Data,
                    }
                    let r: Resp = serde_json::from_str(text.as_str()).map_err(|_| {
                        StoreError::Unavailable(format!(
                            "Vault Transit decrypt {what}: not an answer"
                        ))
                    })?;
                    let b64 = Zeroizing::new(r.data.plaintext);
                    let pt = Zeroizing::new(STANDARD.decode(b64.as_bytes()).map_err(|_| {
                        StoreError::Sealed(format!("Vault Transit decrypt {what}: not base64"))
                    })?);
                    if pt.len() != 16 {
                        return Err(StoreError::Sealed(format!(
                            "Vault Transit decrypt {what}: not a file key"
                        )));
                    }
                    let mut k = Zeroizing::new([0u8; 16]);
                    k.copy_from_slice(&pt);
                    return Ok(k);
                }
                // A token the agent had not yet replaced: read the file again, once.
                403 if !refused_once && matches!(self.token, TokenSource::File(_)) => {
                    refused_once = true;
                    std::thread::sleep(pause);
                    continue;
                }
                403 => {
                    return Err(StoreError::Sealed(format!(
                        "Vault Transit denied decrypt on {what}: the token's policy"
                    )));
                }
                s if s == 429 || s >= 500 => {
                    return Err(StoreError::Unavailable(format!(
                        "Vault Transit decrypt {what}: {s}"
                    )));
                }
                s => {
                    return Err(StoreError::Sealed(format!(
                        "Vault Transit did not open the file key with {what}: {s}"
                    )));
                }
            }
        }
    }
}

/// What a reader opens sealed objects with. Either is enough; none reads only what is in clear.
#[derive(Default)]
pub struct Keys {
    /// Vault Transit.
    pub transit: Option<Box<dyn TransitUnwrap>>,
    /// Offline recovery identities.
    pub recovery: Vec<Identity>,
}

impl core::fmt::Debug for Keys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Keys")
            .field("transit", &self.transit.is_some())
            .field("recovery", &self.recovery.len())
            .finish()
    }
}

impl Keys {
    /// No key: only what is in clear, and unsealed objects, read.
    pub fn none() -> Keys {
        Keys::default()
    }

    /// The keys a deployment gives a reader: Vault Transit at `vault` (address, mount) with the
    /// token in the file [`VAULT_TOKEN_FILE_ENV`] names, and the recovery identities in the file
    /// [`RECOVERY_IDENTITY_ENV`] names. Neither key is ever in the environment itself.
    pub fn from_env(vault: Option<(&str, &str)>) -> Result<Keys, String> {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        let transit = match vault {
            Some((address, mount)) => {
                let file = var(VAULT_TOKEN_FILE_ENV).ok_or_else(|| {
                    format!("vault= needs {VAULT_TOKEN_FILE_ENV}: the file that holds the token")
                })?;
                Some(Box::new(VaultTransit::new(
                    address,
                    mount,
                    TokenSource::File(PathBuf::from(file)),
                )?) as Box<dyn TransitUnwrap>)
            }
            None => None,
        };
        let recovery = match var(RECOVERY_IDENTITY_ENV) {
            Some(p) => {
                let p = PathBuf::from(p);
                let text = Zeroizing::new(
                    std::fs::read_to_string(&p)
                        .map_err(|e| format!("{RECOVERY_IDENTITY_ENV} {}: {e}", p.display()))?,
                );
                Identity::parse_file(&text)
                    .map_err(|e| format!("{RECOVERY_IDENTITY_ENV} {}: {e}", p.display()))?
            }
            None => Vec::new(),
        };
        Ok(Keys { transit, recovery })
    }

    fn is_empty(&self) -> bool {
        self.transit.is_none() && self.recovery.is_empty()
    }
}

/// Whether a reader takes objects that are not sealed (an archive written before `F-044`, or by a
/// writer with KMS only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsealed {
    /// Read them as they are.
    Accept,
    /// Refuse them: every object of this archive must be sealed.
    Refuse,
}

struct Epoch {
    file_key: FileKey,
    stanzas: Vec<Stanza>,
    started: Instant,
    sealed: u64,
}

/// An envelope, parsed.
struct Envelope<'a> {
    line: SealLine,
    line_bytes: &'a [u8],
    clear: &'a [u8],
    age: &'a [u8],
}

fn corrupt(name: &str, why: &str) -> StoreError {
    StoreError::Corrupt(format!("{name}: {why}"))
}

fn is_sealed(raw: &[u8]) -> bool {
    raw.starts_with(MAGIC_LINE)
}

fn parse_envelope<'a>(name: &str, raw: &'a [u8]) -> Result<Envelope<'a>, StoreError> {
    let rest = raw
        .strip_prefix(MAGIC_LINE)
        .ok_or_else(|| corrupt(name, "not a sealed object"))?;
    let end = rest
        .iter()
        .take(MAX_SEAL_LINE)
        .position(|b| *b == b'\n')
        .ok_or_else(|| corrupt(name, "the seal line is truncated"))?;
    let line_bytes = &rest[..end];
    let line: SealLine = serde_json::from_slice(line_bytes)
        .map_err(|_| corrupt(name, "the seal line does not parse"))?;
    let rest = &rest[end + 1..];
    let clear = usize::try_from(line.clear)
        .ok()
        .filter(|c| *c <= MAX_CLEAR && *c as u64 <= line.bytes)
        .ok_or_else(|| corrupt(name, "the seal line's clear length is out of bounds"))?;
    let (clear, age) = rest
        .split_at_checked(clear)
        .ok_or_else(|| corrupt(name, "truncated"))?;
    Ok(Envelope {
        line,
        line_bytes,
        clear,
        age,
    })
}

/// The digest by which a reader remembers a file key: the wrap stanzas, as written.
fn wraps_digest(stanzas: &[Stanza]) -> [u8; 32] {
    let mut h = Sha256::new();
    for s in stanzas.iter().filter(|s| s.kind != SEAL_KIND) {
        let mut b = Vec::new();
        s.encode(&mut b);
        h.update(&b);
    }
    h.finalize().into()
}

/// The sealing layer over a store (module documentation). With [`Recipients`] it seals what it
/// publishes; with [`Keys`] it opens what it reads; it always passes names, listings and
/// unsealed objects through ([`Unsealed`]).
pub struct Sealed {
    inner: Box<dyn ObjectStore>,
    recipients: Option<Recipients>,
    keys: Keys,
    unsealed: Unsealed,
    epoch: Mutex<Option<Epoch>>,
    opened: Mutex<HashMap<[u8; 32], FileKey>>,
}

impl Sealed {
    /// The layer over `inner`.
    pub fn new(
        inner: Box<dyn ObjectStore>,
        recipients: Option<Recipients>,
        keys: Keys,
        unsealed: Unsealed,
    ) -> Sealed {
        Sealed {
            inner,
            recipients,
            keys,
            unsealed,
            epoch: Mutex::new(None),
            opened: Mutex::new(HashMap::new()),
        }
    }

    /// Whether it seals what it publishes.
    pub fn seals(&self) -> bool {
        self.recipients.is_some()
    }

    /// The store under it.
    pub fn inner(&self) -> &dyn ObjectStore {
        self.inner.as_ref()
    }

    /// Seal `plaintext` as `name` (module documentation).
    pub fn seal(&self, name: &str, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
        let recipients = self
            .recipients
            .as_ref()
            .ok_or_else(|| StoreError::Sealed("this archive has no seal recipients".into()))?;
        let (file_key, mut stanzas) = {
            let mut epoch = self.epoch.lock().unwrap_or_else(|p| p.into_inner());
            let stale = epoch
                .as_ref()
                .is_none_or(|e| e.sealed >= EPOCH_OBJECTS || e.started.elapsed() >= EPOCH);
            if stale {
                let file_key = age::new_file_key();
                let stanzas = recipients.wrap(&file_key).map_err(StoreError::Sealed)?;
                // The writer opens what it sealed in this epoch without a key of its own.
                self.opened
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(wraps_digest(&stanzas), file_key.clone());
                *epoch = Some(Epoch {
                    file_key,
                    stanzas,
                    started: Instant::now(),
                    sealed: 0,
                });
            }
            let e = epoch.as_mut().unwrap_or_else(|| unreachable!("set above"));
            e.sealed += 1;
            (e.file_key.clone(), e.stanzas.clone())
        };
        let clear = clear_len(name, plaintext);
        let line = serde_json::to_vec(&SealLine {
            name: name.to_owned(),
            bytes: plaintext.len() as u64,
            sha256: sha256_hex(plaintext),
            clear: clear as u64,
        })
        .unwrap_or_else(|_| unreachable!("a seal line serialises"));
        stanzas.push(Stanza {
            kind: SEAL_KIND.into(),
            args: vec![sha256_hex(&line)],
            body: Vec::new(),
        });
        let header = age::encode_header(&file_key, &stanzas)
            .map_err(|e| StoreError::Sealed(e.to_string()))?;
        let payload = age::encrypt_payload(&file_key, plaintext);
        let mut out = Vec::with_capacity(
            MAGIC_LINE.len() + line.len() + 1 + clear + header.len() + payload.len(),
        );
        out.extend_from_slice(MAGIC_LINE);
        out.extend_from_slice(&line);
        out.push(b'\n');
        out.extend_from_slice(&plaintext[..clear]);
        out.extend_from_slice(&header);
        out.extend_from_slice(&payload);
        Ok(out)
    }

    fn file_key(&self, name: &str, stanzas: &[Stanza]) -> Result<FileKey, StoreError> {
        let digest = wraps_digest(stanzas);
        if let Some(k) = self
            .opened
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&digest)
        {
            return Ok(k.clone());
        }
        let mut transit_error = None;
        let mut found = None;
        'stanzas: for s in stanzas.iter().filter(|s| s.kind == age::X25519_KIND) {
            for id in &self.keys.recovery {
                if let Some(k) = id.unwrap(s) {
                    found = Some(k);
                    break 'stanzas;
                }
            }
        }
        if found.is_none()
            && let Some(t) = &self.keys.transit
        {
            for s in stanzas.iter().filter(|s| s.kind == TRANSIT_KIND) {
                let [key, version] = s.args.as_slice() else {
                    return Err(corrupt(name, "an aledgent-transit stanza is malformed"));
                };
                let version: u32 = version
                    .parse()
                    .map_err(|_| corrupt(name, "an aledgent-transit stanza's version"))?;
                match t.unwrap(key, version, &s.body) {
                    Ok(k) => {
                        found = Some(k);
                        break;
                    }
                    Err(e @ StoreError::Unavailable(_)) => return Err(e),
                    Err(e) => transit_error = Some(e),
                }
            }
        }
        match found {
            Some(k) => {
                self.opened
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(digest, k.clone());
                Ok(k)
            }
            None => {
                let kinds: Vec<String> = stanzas
                    .iter()
                    .filter(|s| s.kind != SEAL_KIND)
                    .map(|s| match s.kind.as_str() {
                        TRANSIT_KIND => format!("Transit {}", s.args.join(" v")),
                        k => k.to_owned(),
                    })
                    .collect();
                Err(StoreError::Sealed(
                    match (self.keys.is_empty(), transit_error) {
                        (true, _) => format!(
                            "{name} is sealed ({}) and no key is given: Vault (vault= and \
                         {VAULT_TOKEN_FILE_ENV}) or the recovery identity ({RECOVERY_IDENTITY_ENV})",
                            kinds.join(", ")
                        ),
                        (false, Some(e)) => {
                            format!("{name}: no key given opens it ({}): {e}", kinds.join(", "))
                        }
                        (false, None) => {
                            format!("{name}: no key given opens it ({})", kinds.join(", "))
                        }
                    },
                ))
            }
        }
    }

    /// Open the envelope `raw` read as `name`, refusing a plaintext over `max` bytes: always by
    /// decrypting it, with a file key this layer holds (one it sealed with, or opened before) or
    /// one its [`Keys`] unwrap. Nothing in clear is taken on trust here.
    pub fn open(&self, name: &str, raw: &[u8], max: u64) -> Result<Vec<u8>, StoreError> {
        let env = parse_envelope(name, raw)?;
        if env.line.name != name {
            return Err(corrupt(name, "sealed under another name"));
        }
        if env.line.bytes > max {
            return Err(StoreError::TooLarge(name.to_owned()));
        }
        let (header, at) = age::parse_header(env.age).map_err(|e| corrupt(name, &e.to_string()))?;
        let seal_line_digest = sha256_hex(env.line_bytes);
        let bound: Vec<&Stanza> = header
            .stanzas
            .iter()
            .filter(|s| s.kind == SEAL_KIND)
            .collect();
        if bound.len() != 1 || bound[0].args != [seal_line_digest] || !bound[0].body.is_empty() {
            return Err(corrupt(name, "the age header does not bind this seal line"));
        }
        let file_key = self.file_key(name, &header.stanzas)?;
        age::check_mac(&header, &file_key).map_err(|e| corrupt(name, &e.to_string()))?;
        let plaintext = age::decrypt_payload(&file_key, &env.age[at..], env.line.bytes)
            .map_err(|e| corrupt(name, &e.to_string()))?;
        if plaintext.len() as u64 != env.line.bytes
            || sha256_hex(&plaintext) != env.line.sha256
            || !plaintext.starts_with(env.clear)
        {
            return Err(corrupt(name, "what opened is not what the seal line says"));
        }
        Ok(plaintext)
    }

    /// Whether `raw`, as found under `name`, is `plaintext` published before: by its seal line,
    /// or byte for byte if it is not sealed.
    fn same(name: &str, raw: &[u8], plaintext: &[u8]) -> bool {
        if !is_sealed(raw) {
            return raw == plaintext;
        }
        parse_envelope(name, raw).is_ok_and(|e| {
            e.line.name == name
                && e.line.bytes == plaintext.len() as u64
                && e.line.sha256 == sha256_hex(plaintext)
        })
    }
}

impl ObjectStore for Sealed {
    fn put_once(&self, name: &str, bytes: &[u8]) -> Result<Put, StoreError> {
        check_name(name)?;
        if self.recipients.is_none() {
            return self.inner.put_once(name, bytes);
        }
        let sealed = self.seal(name, bytes)?;
        match self.inner.put_once(name, &sealed) {
            Err(StoreError::Conflict(n)) => {
                match self.inner.get(name, sealed_bound(bytes.len() as u64))? {
                    Some(raw) if Sealed::same(name, &raw, bytes) => Ok(Put { created: false }),
                    _ => Err(StoreError::Conflict(n)),
                }
            }
            other => other,
        }
    }

    fn get(&self, name: &str, max: u64) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(raw) = self.inner.get(name, sealed_bound(max))? else {
            return Ok(None);
        };
        if is_sealed(&raw) {
            // Wholly in clear (a verify-genesis report) and no key given: the clear bytes, if they
            // are what the seal line's digest says. That catches corruption, not a forgery: the
            // seal line is not authenticated for a reader with no key (the store's write-once and
            // Object Lock are what keep it). A reader with a key decrypts.
            if self.keys.is_empty() {
                let env = parse_envelope(name, &raw)?;
                if env.line.name == name && env.line.clear == env.line.bytes {
                    if env.line.bytes > max {
                        return Err(StoreError::TooLarge(name.to_owned()));
                    }
                    return if sha256_hex(env.clear) == env.line.sha256 {
                        Ok(Some(env.clear.to_vec()))
                    } else {
                        Err(corrupt(name, "its clear bytes are not the seal line's"))
                    };
                }
            }
            return self.open(name, &raw, max).map(Some);
        }
        if self.unsealed == Unsealed::Refuse {
            return Err(corrupt(
                name,
                "not sealed, and this archive takes sealed objects only",
            ));
        }
        if raw.len() as u64 > max {
            return Err(StoreError::TooLarge(name.to_owned()));
        }
        Ok(Some(raw))
    }

    fn head(&self, name: &str, max: u64) -> Result<Option<Head>, StoreError> {
        let Some(raw) = self.inner.get(name, sealed_bound(max))? else {
            return Ok(None);
        };
        if !is_sealed(&raw) {
            if self.unsealed == Unsealed::Refuse {
                return Err(corrupt(
                    name,
                    "not sealed, and this archive takes sealed objects only",
                ));
            }
            if raw.len() as u64 > max {
                return Err(StoreError::TooLarge(name.to_owned()));
            }
            return Ok(Some(Head::of(raw)));
        }
        let env = parse_envelope(name, &raw)?;
        if env.line.name != name {
            return Err(corrupt(name, "sealed under another name"));
        }
        if env.line.bytes > max {
            return Err(StoreError::TooLarge(name.to_owned()));
        }
        Ok(Some(Head {
            bytes: env.line.bytes,
            sha256: env.line.sha256,
            clear: env.clear.to_vec(),
        }))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        self.inner.list(prefix)
    }

    fn describe(&self) -> String {
        match &self.recipients {
            Some(r) => format!(
                "{} (sealed: Transit {} v{}, {} recovery key{})",
                self.inner.describe(),
                r.transit.name,
                r.transit.version,
                r.recovery.len(),
                if r.recovery.len() == 1 { "" } else { "s" }
            ),
            None => self.inner.describe(),
        }
    }
}

/// What [`copy`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Copied {
    /// Objects sealed and published by this run, each decrypted back and found the same.
    pub created: u64,
    /// Objects already there, by their seal line the same, and decrypted back with the target's
    /// reader keys and found the same.
    pub present: u64,
    /// Objects already there, by their seal line the same, that this run could not decrypt (the
    /// target has no reader key): **not verified**. The old archive retires only when a run with
    /// a reader key leaves this empty.
    pub unverified: Vec<String>,
}

/// Copy every object under `prefixes` of `from` (the old archive, as it is) to `to` (a sealing
/// writer), under the same names. Each object this run seals is decrypted back (with the file key
/// it sealed under) and compared byte for byte; each one already there is decrypted with `to`'s
/// reader keys if it has any, and otherwise listed in [`Copied::unverified`]. Any failure to read
/// back is an error, never a count. Resumable.
///
/// `from` must give plaintext: an object that is already sealed is refused (read a sealed source
/// through a [`Sealed`] reader), so nothing is sealed twice.
pub fn copy(
    from: &dyn ObjectStore,
    to: &Sealed,
    prefixes: &[&str],
    max: u64,
) -> Result<Copied, StoreError> {
    if !to.seals() {
        return Err(StoreError::Sealed(
            "the copy's target must seal: give it seal recipients".into(),
        ));
    }
    let mut out = Copied::default();
    for prefix in prefixes {
        for name in from.list(prefix)? {
            let bytes = from
                .get(&name, max)?
                .ok_or_else(|| StoreError::Unavailable(format!("{name} vanished while copied")))?;
            if is_sealed(&bytes) {
                return Err(corrupt(
                    &name,
                    "the source holds it sealed already: read the source through a Sealed reader",
                ));
            }
            let created = to.put_once(&name, &bytes)?.created;
            let raw = match to.inner.get(&name, sealed_bound(max))? {
                Some(raw) if is_sealed(&raw) && Sealed::same(&name, &raw, &bytes) => raw,
                _ => return Err(corrupt(&name, "the copy is not sealed as the original")),
            };
            if created || !to.keys.is_empty() {
                // Sealed by this run (its file key is held), or a reader key is given: decrypt.
                if to.open(&name, &raw, max)? != bytes {
                    return Err(corrupt(
                        &name,
                        "the copy does not read back as the original",
                    ));
                }
                if created {
                    out.created += 1;
                } else {
                    out.present += 1;
                }
            } else {
                out.unverified.push(name);
            }
        }
    }
    Ok(out)
}
