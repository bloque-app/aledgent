// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The in-process engine (`F-042`): the key is in the memory of the process that signs, read from a
//! **file** at start (32 bytes of hex), and wiped when dropped. There is no locator that reads an
//! environment variable, and there will not be one (`NFR-8`).
//!
//! It is a supported kind for every component but the core host, where `NFR-8` bars key material in the
//! book's process: a component says which kinds each of its keys may use, and the configuration check
//! enforces it ([`crate::engine::config`]). The signer service's own keys are [`LocalKey`]s too.

use std::path::PathBuf;
use std::sync::Arc;

use ledger_core::anchor::clerk::{ClerkSigner, InProcessClerk};
use ledger_kernel::keys::{Custody as _, KeyRequest, RootKeys};
use serde::Deserialize;
use zeroize::Zeroizing;

use super::{Engine, EngineKind, KeyHandle};
use crate::client::ClientError;
use crate::proto::{Op, Refusal, Scheme};

enum Backend {
    KeyRing(RootKeys),
    Clerk(InProcessClerk),
    Ed25519(ed25519_dalek::SigningKey),
    Secp256k1(k256::ecdsa::SigningKey),
}

/// A key held in memory, of any scheme, answering synchronously.
pub struct LocalKey {
    scheme: Scheme,
    public: Vec<u8>,
    backend: Backend,
}

fn compressed(k: &k256::ecdsa::VerifyingKey) -> Vec<u8> {
    k.to_encoded_point(true).as_bytes().to_vec()
}

/// A recoverable signature as the chain takes it: `r ‖ s ‖ v`.
pub(crate) fn recoverable(sig: k256::ecdsa::Signature, rec: k256::ecdsa::RecoveryId) -> Vec<u8> {
    let mut out = sig.to_bytes().to_vec();
    out.push(rec.to_byte());
    out
}

impl LocalKey {
    /// A key over a 32-byte secret, as `scheme` reads one: the key-ring root; the clerk's canonical
    /// scalar; an Ed25519 seed; a secp256k1 secret.
    pub fn from_secret(
        id: &str,
        scheme: Scheme,
        secret: Zeroizing<Vec<u8>>,
    ) -> Result<LocalKey, String> {
        let bytes: Zeroizing<[u8; 32]> = Zeroizing::new(
            secret
                .as_slice()
                .try_into()
                .map_err(|_| format!("key {id}: a secret is 32 bytes"))?,
        );
        let (backend, public) = match scheme {
            Scheme::KeyRing => {
                let r = RootKeys::new(*bytes);
                let public = r.registrar_key().0.to_vec();
                (Backend::KeyRing(r), public)
            }
            Scheme::Clerk => {
                let c = InProcessClerk::from_secret(*bytes)
                    .ok_or_else(|| format!("key {id}: not a canonical, non-zero scalar"))?;
                let public = c.public_key().to_vec();
                (Backend::Clerk(c), public)
            }
            Scheme::Ed25519 => {
                let k = ed25519_dalek::SigningKey::from_bytes(&bytes);
                let public = k.verifying_key().to_bytes().to_vec();
                (Backend::Ed25519(k), public)
            }
            Scheme::Secp256k1 => {
                let k = k256::ecdsa::SigningKey::from_slice(bytes.as_slice())
                    .map_err(|_| format!("key {id}: not a secp256k1 secret"))?;
                let public = compressed(k.verifying_key());
                (Backend::Secp256k1(k), public)
            }
        };
        Ok(LocalKey {
            scheme,
            public,
            backend,
        })
    }

    /// Its scheme.
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Its public half.
    pub fn public(&self) -> &[u8] {
        &self.public
    }

    /// The answer to `op`, or why not (what each scheme answers: [`crate::proto::Scheme`]).
    pub fn answer(&self, op: &Op) -> Result<Vec<u8>, Refusal> {
        match (op, &self.backend) {
            (Op::Public, _) => Ok(self.public.clone()),
            (Op::KeyRing(r), Backend::KeyRing(root)) => {
                let request = KeyRequest::decode(r).ok_or(Refusal::Malformed)?;
                Ok(root.answer(&request).encode())
            }
            (Op::Sign(msg), Backend::Clerk(c)) => Ok(c.sign(msg).to_vec()),
            (Op::Sign(msg), Backend::Ed25519(k)) => {
                use ed25519_dalek::Signer as _;
                Ok(k.sign(msg).to_bytes().to_vec())
            }
            (Op::SignDigest(d), Backend::Secp256k1(k)) => {
                let (sig, rec) = k
                    .sign_prehash_recoverable(d)
                    .map_err(|_| Refusal::Malformed)?;
                Ok(recoverable(sig, rec))
            }
            _ => Err(Refusal::WrongScheme),
        }
    }
}

/// The locator of an in-process key: the file holding it, 32 bytes of hex (`0x` and surrounding
/// whitespace allowed). Nothing else: an `env` field is an unknown field, and refused.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locator {
    /// The file.
    pub file: PathBuf,
}

impl Locator {
    /// Read a locator from its JSON.
    pub fn parse(v: &serde_json::Value) -> Result<Locator, String> {
        Locator::deserialize(v)
            .map_err(|e| format!("an in_process key is {{\"file\": \"<path>\"}}: {e}"))
    }
}

/// The in-process engine (module documentation).
#[derive(Clone, Copy, Debug, Default)]
pub struct InProcessEngine;

const ALL: [Scheme; 4] = [
    Scheme::KeyRing,
    Scheme::Clerk,
    Scheme::Ed25519,
    Scheme::Secp256k1,
];

impl Engine for InProcessEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::InProcess
    }

    fn schemes(&self) -> &[Scheme] {
        &ALL
    }

    fn open(
        &self,
        scheme: Scheme,
        locator: &serde_json::Value,
    ) -> Result<Arc<dyn KeyHandle>, ClientError> {
        let l = Locator::parse(locator).map_err(ClientError::Config)?;
        let text = Zeroizing::new(
            std::fs::read_to_string(&l.file)
                .map_err(|e| ClientError::Config(format!("{}: {e}", l.file.display())))?,
        );
        let secret = Zeroizing::new(
            hex::decode(text.trim().trim_start_matches("0x"))
                .map_err(|_| ClientError::Config(format!("{}: not hex", l.file.display())))?,
        );
        let id = l.file.display().to_string();
        let key = LocalKey::from_secret(&id, scheme, secret).map_err(ClientError::Config)?;
        Ok(Arc::new(InProcessKey { id, key }))
    }
}

/// An open in-process key.
pub struct InProcessKey {
    id: String,
    key: LocalKey,
}

impl core::fmt::Debug for InProcessKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InProcessKey")
            .field("file", &self.id)
            .field("scheme", &self.key.scheme)
            .finish_non_exhaustive()
    }
}

impl KeyHandle for InProcessKey {
    fn kind(&self) -> EngineKind {
        EngineKind::InProcess
    }

    fn scheme(&self) -> Scheme {
        self.key.scheme
    }

    fn public(&self) -> Vec<u8> {
        self.key.public.clone()
    }

    fn call(&self, op: &Op) -> Result<Vec<u8>, ClientError> {
        self.key.answer(op).map_err(ClientError::Refused)
    }
}
