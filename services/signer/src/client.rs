// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The signer's client (blocking), and its adapters for the processes that hold the book.
//!
//! [`SignerClient`] keeps one connection, authenticated once ([`proto::Hello`]), and re-opens it
//! when it breaks. The adapters turn it into what each caller already speaks:
//!
//! - [`RemoteCustody`]: a [`Custody`] for the core's key ring (`ledger_kernel::keys`);
//! - [`RemoteClerk`]: a [`ClerkSigner`] for disclosures;
//! - [`RemoteEd25519`]: the core host's Agent authority and package keys;
//! - [`RemoteSecp256k1`]: the adapter's lane and anchor keys.
//!
//! **Failing closed.** A [`Custody`] or a [`ClerkSigner`] cannot return an error: the book's
//! native run has nowhere to put one. Their adapters wait for the signer, retrying a broken
//! connection or an unavailable backend for at most [`Patience::wait`]; past that, or on a
//! refusal (a token revoked, a key not granted), they stop the process with an `ALERT` line
//! ([`OnFailure::Exit`]). Nothing is made up and nothing is half-committed: the input that was
//! running never reached the Agent, the command is still in ingress's received-log, and a
//! restart rebuilds the replica. Tests set [`OnFailure::Panic`].

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ledger_core::anchor::clerk::ClerkSigner;
use ledger_kernel::keys::{Custody, KeyAnswer, KeyRequest};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::proto::{self, Hello, KeyInfo, Op, Refusal, Request, Response, Scheme, Welcome};

/// Where a caller finds the signer, as its own configuration names it. The token is read from a
/// file (written by Vault at start), never from the configuration or the environment.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct SignerEndpoint {
    /// The signer's Unix socket.
    pub socket: PathBuf,
    /// This caller's name in the signer's configuration.
    pub client: String,
    /// The file holding this caller's token.
    pub token_file: PathBuf,
    /// How long one request may take, in milliseconds (default 10 000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// A caller's token. Never formatted, wiped when dropped.
pub struct Token(Zeroizing<Vec<u8>>);

/// The fewest bytes a token may have: 32 bytes of randomness, hex-encoded at least.
pub const MIN_TOKEN_BYTES: usize = 32;

impl Token {
    /// A token over `bytes` (at least [`MIN_TOKEN_BYTES`]).
    pub fn new(bytes: Vec<u8>) -> Result<Token, ClientError> {
        let bytes = Zeroizing::new(bytes);
        if bytes.len() < MIN_TOKEN_BYTES {
            return Err(ClientError::Config(format!(
                "a token has at least {MIN_TOKEN_BYTES} bytes"
            )));
        }
        Ok(Token(bytes))
    }

    /// The token in the file at `path`, surrounding whitespace removed.
    pub fn from_file(path: &Path) -> Result<Token, ClientError> {
        let text =
            Zeroizing::new(std::fs::read(path).map_err(|e| {
                ClientError::Config(format!("the token file {}: {e}", path.display()))
            })?);
        let trimmed = text.trim_ascii();
        Token::new(trimmed.to_vec())
    }

    /// Its bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Why a call failed. Never carries key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// The configuration or the token file is wrong.
    Config(String),
    /// The socket could not be reached, or broke.
    Io(String),
    /// The signer refused.
    Refused(Refusal),
    /// The signer answered something this client cannot read.
    Protocol(String),
}

impl ClientError {
    /// Whether waiting may help: a broken connection or an unavailable backend.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ClientError::Io(_) | ClientError::Refused(Refusal::Unavailable(_))
        )
    }
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ClientError::Config(e) => write!(f, "signer client configuration: {e}"),
            ClientError::Io(e) => write!(f, "the signer is unreachable: {e}"),
            ClientError::Refused(r) => write!(f, "the signer refused: {r}"),
            ClientError::Protocol(e) => write!(f, "the signer's answer is unreadable: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

fn io(e: std::io::Error) -> ClientError {
    ClientError::Io(e.to_string())
}

/// A connection to the signer (module documentation).
pub struct SignerClient {
    socket: PathBuf,
    client: String,
    token: Token,
    timeout: Duration,
    conn: Mutex<Option<UnixStream>>,
    keys: Mutex<Vec<KeyInfo>>,
}

impl core::fmt::Debug for SignerClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SignerClient")
            .field("socket", &self.socket)
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl SignerClient {
    /// Connect to the signer `endpoint` names and authenticate. Fails if the signer is not there
    /// or refuses: a process that needs its keys does not start without them.
    pub fn connect(endpoint: &SignerEndpoint) -> Result<SignerClient, ClientError> {
        let token = Token::from_file(&endpoint.token_file)?;
        SignerClient::with_token(
            &endpoint.socket,
            &endpoint.client,
            token,
            Duration::from_millis(endpoint.timeout_ms.unwrap_or(10_000)),
        )
    }

    /// [`SignerClient::connect`] with the token given.
    pub fn with_token(
        socket: &Path,
        client: &str,
        token: Token,
        timeout: Duration,
    ) -> Result<SignerClient, ClientError> {
        let c = SignerClient {
            socket: socket.to_path_buf(),
            client: client.to_owned(),
            token,
            timeout,
            conn: Mutex::new(None),
            keys: Mutex::new(Vec::new()),
        };
        let (stream, keys) = c.handshake()?;
        *c.conn.lock().unwrap_or_else(PoisonError::into_inner) = Some(stream);
        *c.keys.lock().unwrap_or_else(PoisonError::into_inner) = keys;
        Ok(c)
    }

    fn handshake(&self) -> Result<(UnixStream, Vec<KeyInfo>), ClientError> {
        let mut s = UnixStream::connect(&self.socket).map_err(io)?;
        s.set_read_timeout(Some(self.timeout)).map_err(io)?;
        s.set_write_timeout(Some(self.timeout)).map_err(io)?;
        let hello = Hello {
            version: proto::VERSION,
            client: self.client.clone(),
            token: self.token.bytes().to_vec(),
        };
        proto::write(&mut s, &hello).map_err(io)?;
        match proto::read::<Welcome, _>(&mut s).map_err(io)? {
            Welcome::Ok { keys } => Ok((s, keys)),
            Welcome::Refused(r) => Err(ClientError::Refused(r)),
        }
    }

    /// The keys this caller is granted, as the signer listed them at the last handshake.
    pub fn keys(&self) -> Vec<KeyInfo> {
        self.keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The key `id`, if this caller is granted it.
    pub fn key(&self, id: &str) -> Option<KeyInfo> {
        self.keys().into_iter().find(|k| k.id == id)
    }

    /// The key `id`, which must be of `scheme`.
    pub fn key_of(&self, id: &str, scheme: Scheme) -> Result<KeyInfo, ClientError> {
        match self.key(id) {
            Some(k) if k.scheme == scheme => Ok(k),
            Some(_) => Err(ClientError::Refused(Refusal::WrongScheme)),
            None => Err(ClientError::Refused(Refusal::NotGranted)),
        }
    }

    fn exchange(stream: &mut UnixStream, request: &Request) -> Result<Response, ClientError> {
        proto::write(stream, request).map_err(io)?;
        proto::read::<Response, _>(stream).map_err(io)
    }

    /// Ask `op` of the key `key`. A broken connection is re-opened once.
    pub fn call(&self, key: &str, op: Op) -> Result<Vec<u8>, ClientError> {
        let request = Request {
            key: key.to_owned(),
            op,
        };
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut last = None;
        for _ in 0..2 {
            if conn.is_none() {
                match self.handshake() {
                    Ok((s, keys)) => {
                        *conn = Some(s);
                        *self.keys.lock().unwrap_or_else(PoisonError::into_inner) = keys;
                    }
                    Err(e) => return Err(e),
                }
            }
            let Some(stream) = conn.as_mut() else {
                continue;
            };
            match Self::exchange(stream, &request) {
                Ok(Response::Ok(bytes)) => return Ok(bytes),
                Ok(Response::Refused(Refusal::Unauthenticated)) => {
                    *conn = None;
                    return Err(ClientError::Refused(Refusal::Unauthenticated));
                }
                Ok(Response::Refused(r)) => return Err(ClientError::Refused(r)),
                Err(e) => {
                    *conn = None;
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| ClientError::Io("no connection".into())))
    }
}

/// What an adapter that cannot return an error does when the signer cannot answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnFailure {
    /// Stop the process (status 70) after an `ALERT` line: the production setting.
    Exit,
    /// Panic: tests.
    Panic,
}

/// How long an adapter waits for the signer, and what it does after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Patience {
    /// The longest it retries a transient failure.
    pub wait: Duration,
    /// Then.
    pub on_failure: OnFailure,
}

impl Patience {
    /// One minute, then stop the process.
    pub const PRODUCTION: Patience = Patience {
        wait: Duration::from_secs(60),
        on_failure: OnFailure::Exit,
    };

    /// No retry, then panic.
    pub const TEST: Patience = Patience {
        wait: Duration::ZERO,
        on_failure: OnFailure::Panic,
    };

    fn fail(&self, what: &str, e: &ClientError) -> ! {
        let line = format!("ALERT aledgent-signer: {what}: {e}; stopping (fail closed)");
        match self.on_failure {
            OnFailure::Exit => {
                eprintln!("{line}");
                std::process::exit(70)
            }
            OnFailure::Panic => panic!("{line}"),
        }
    }

    /// Run `f` until it succeeds, retrying transient failures for [`Patience::wait`].
    fn must<T>(&self, what: &str, mut f: impl FnMut() -> Result<T, ClientError>) -> T {
        let started = Instant::now();
        let mut pause = Duration::from_millis(50);
        loop {
            match f() {
                Ok(v) => return v,
                Err(e) if e.is_transient() && started.elapsed() < self.wait => {
                    eprintln!("aledgent-signer: {what}: {e}; retrying");
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_secs(2));
                }
                Err(e) => self.fail(what, &e),
            }
        }
    }
}

/// The core's key ring behind the signer: a [`Custody`] (`F-035`). Public keys are cached; every
/// signature and blinding is asked.
pub struct RemoteCustody {
    client: Arc<SignerClient>,
    key: String,
    patience: Patience,
    public: Mutex<HashMap<Vec<u8>, KeyAnswer>>,
}

impl RemoteCustody {
    /// The key ring `key` of `client`'s signer.
    pub fn new(
        client: Arc<SignerClient>,
        key: &str,
        patience: Patience,
    ) -> Result<RemoteCustody, ClientError> {
        client.key_of(key, Scheme::KeyRing)?;
        Ok(RemoteCustody {
            client,
            key: key.to_owned(),
            patience,
            public: Mutex::new(HashMap::new()),
        })
    }

    /// [`Custody::answer`], with the error.
    pub fn try_answer(&self, request: &KeyRequest) -> Result<KeyAnswer, ClientError> {
        let bytes = self.client.call(&self.key, Op::KeyRing(request.encode()))?;
        KeyAnswer::decode(&bytes).ok_or_else(|| ClientError::Protocol("not a key answer".into()))
    }
}

impl Custody for RemoteCustody {
    fn answer(&self, request: &KeyRequest) -> KeyAnswer {
        let cacheable = matches!(
            request,
            KeyRequest::RegistrarKey | KeyRequest::AccountKey(_)
        );
        let encoded = request.encode();
        if cacheable
            && let Some(a) = self
                .public
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&encoded)
        {
            return *a;
        }
        let a = self
            .patience
            .must(request.kind(), || self.try_answer(request));
        if cacheable {
            self.public
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(encoded, a);
        }
        a
    }
}

fn fixed<const N: usize>(bytes: &[u8]) -> Result<[u8; N], ClientError> {
    bytes
        .try_into()
        .map_err(|_| ClientError::Protocol(format!("expected {N} bytes, got {}", bytes.len())))
}

/// The clerk key behind the signer: a [`ClerkSigner`] (`F-017`, `F-035`).
pub struct RemoteClerk {
    client: Arc<SignerClient>,
    key: String,
    public: [u8; 32],
    patience: Patience,
}

impl RemoteClerk {
    /// The clerk key `key` of `client`'s signer.
    pub fn new(
        client: Arc<SignerClient>,
        key: &str,
        patience: Patience,
    ) -> Result<RemoteClerk, ClientError> {
        let info = client.key_of(key, Scheme::Clerk)?;
        Ok(RemoteClerk {
            public: fixed(&info.public)?,
            client,
            key: key.to_owned(),
            patience,
        })
    }

    /// [`ClerkSigner::sign`], with the error.
    pub fn try_sign(&self, msg: &[u8]) -> Result<[u8; 64], ClientError> {
        fixed(&self.client.call(&self.key, Op::Sign(msg.to_vec()))?)
    }
}

impl ClerkSigner for RemoteClerk {
    fn public_key(&self) -> [u8; 32] {
        self.public
    }

    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.patience.must("clerk", || self.try_sign(msg))
    }
}

/// An Ed25519 key behind the signer: the core host's Agent authority and package keys.
pub struct RemoteEd25519 {
    client: Arc<SignerClient>,
    key: String,
    public: ed25519_dalek::VerifyingKey,
    patience: Patience,
}

impl RemoteEd25519 {
    /// The Ed25519 key `key` of `client`'s signer.
    pub fn new(
        client: Arc<SignerClient>,
        key: &str,
        patience: Patience,
    ) -> Result<RemoteEd25519, ClientError> {
        let info = client.key_of(key, Scheme::Ed25519)?;
        let public = ed25519_dalek::VerifyingKey::from_bytes(&fixed(&info.public)?)
            .map_err(|_| ClientError::Protocol("not an Ed25519 public key".into()))?;
        Ok(RemoteEd25519 {
            client,
            key: key.to_owned(),
            public,
            patience,
        })
    }

    /// The public key.
    pub fn public(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// A signature over `msg`, checked against the public key before it is returned.
    pub fn try_sign(&self, msg: &[u8]) -> Result<[u8; 64], ClientError> {
        let sig: [u8; 64] = fixed(&self.client.call(&self.key, Op::Sign(msg.to_vec()))?)?;
        self.public
            .verify_strict(msg, &ed25519_dalek::Signature::from_bytes(&sig))
            .map_err(|_| ClientError::Protocol("the signature does not verify".into()))?;
        Ok(sig)
    }

    /// [`RemoteEd25519::try_sign`], failing closed ([`Patience`]).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.patience.must("ed25519", || self.try_sign(msg))
    }
}

/// A secp256k1 key behind the signer: the adapter's lane and anchor keys (PASS device keys).
pub struct RemoteSecp256k1 {
    client: Arc<SignerClient>,
    key: String,
    public: [u8; 33],
}

impl core::fmt::Debug for RemoteSecp256k1 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RemoteSecp256k1")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Recover the compressed public key that signed `digest` with the recoverable `signature`.
pub fn recover_secp256k1(digest: &[u8; 32], signature: &[u8; 65]) -> Option<[u8; 33]> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    let sig = Signature::from_slice(&signature[..64]).ok()?;
    if sig.normalize_s().is_some() {
        // A high `s`: the chain would accept it, but nothing here produces one.
        return None;
    }
    let rec = RecoveryId::from_byte(signature[64])?;
    let key = VerifyingKey::recover_from_prehash(digest, &sig, rec).ok()?;
    key.to_encoded_point(true).as_bytes().try_into().ok()
}

impl RemoteSecp256k1 {
    /// The secp256k1 key `key` of `client`'s signer.
    pub fn new(client: Arc<SignerClient>, key: &str) -> Result<RemoteSecp256k1, ClientError> {
        let info = client.key_of(key, Scheme::Secp256k1)?;
        Ok(RemoteSecp256k1 {
            public: fixed(&info.public)?,
            client,
            key: key.to_owned(),
        })
    }

    /// The compressed public key.
    pub fn public(&self) -> [u8; 33] {
        self.public
    }

    /// A recoverable signature over `digest`, `r ‖ s ‖ v` with `s` low, checked to recover to
    /// this key before it is returned.
    pub fn sign_digest(&self, digest: [u8; 32]) -> Result<[u8; 65], ClientError> {
        let sig: [u8; 65] = fixed(&self.client.call(&self.key, Op::SignDigest(digest))?)?;
        if recover_secp256k1(&digest, &sig) != Some(self.public) {
            return Err(ClientError::Protocol(
                "the signature does not recover to the key".into(),
            ));
        }
        Ok(sig)
    }
}
