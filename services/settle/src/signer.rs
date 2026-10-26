// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The signer boundary (`REQ-AR-3`, `NFR-8`, `REQ-SA-8`; guardrail 13).
//!
//! The adapter never needs a secret key, only something that signs a 32-byte digest: [`Signer`].
//! The chain accepts ECDSA over secp256k1 through `MultiSignature`, and verifies a signature over
//! the blake2b-256 of the signer payload, so the digest is all a signer sees (`F-016` plan,
//! proposal "signer"). One key is one nonce lane.
//!
//! - [`PassAccount`] (`F-027`): any of these as the device key of the deployment's community PASS
//!   account on Kreivo. The device signs; the PASS account acts.
//! - [`DevSigner`]: a key read from a file or an environment variable, for development and tests.
//!   It has no `Debug` or `Display` that shows the key, it never logs it, and its errors never
//!   carry it. The key is zeroised when the signer is dropped (k256's `SigningKey`).
//! - [`ServiceSigner`] (`F-035`): the production signer. A key of the signer service
//!   (`services/signer`), a process of its own, reached on a Unix socket with peer credentials
//!   and a token; the signer reads the key from Vault (or, opted in per key, holds it in AWS KMS).
//!   The adapter holds the key's identifier and public half, nothing else.
//!
//! Trade-off, stated plainly: a key in a file or in the environment is exactly what `NFR-8` rules
//! out for production. [`DevSigner`] exists for development, the tests and the canary; the binary
//! refuses it unless the configuration says `"development_keys": true`, and warns when it does.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use k256::ecdsa::SigningKey;
use zeroize::Zeroizing;

use crate::chain::{AccountId32, blake2_256};

/// Why a signature was not produced. Never carries key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignerError {
    /// The signer cannot sign now (a signer or a KMS that does not answer).
    Unavailable(String),
    /// The key could not be loaded.
    Key(String),
}

impl std::fmt::Display for SignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerError::Unavailable(e) => write!(f, "the signer is unavailable: {e}"),
            SignerError::Key(e) => write!(f, "the signing key could not be loaded: {e}"),
        }
    }
}

/// A boxed future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Something that signs a 32-byte digest with a secp256k1 key. See the module documentation.
pub trait Signer: Send + Sync {
    /// The compressed public key, 33 bytes.
    fn public(&self) -> [u8; 33];

    /// The chain account that acts: the one whose nonce the transactions bump, that pays their
    /// fees and that the settlement contract lists as a key. For a plain key, the key's own
    /// account ([`Signer::device`]); for a PASS account ([`PassAccount`]), the PASS account.
    fn account(&self) -> AccountId32 {
        self.device()
    }

    /// The account of the key itself: blake2b-256 of the compressed public key (`MultiSigner`).
    /// A PASS account's device id.
    fn device(&self) -> AccountId32 {
        blake2_256(&self.public())
    }

    /// The hashed user id of the PASS account this key is a device of, if it is one
    /// (`F-027` T-027-03).
    fn pass_user(&self) -> Option<[u8; 32]> {
        None
    }

    /// A recoverable ECDSA signature over `digest`: `r ‖ s ‖ v`, 65 bytes.
    fn sign(&self, digest: [u8; 32]) -> BoxFuture<'_, Result<[u8; 65], SignerError>>;
}

/// The deployment's community PASS account on Kreivo (`F-027` T-027-03, the owner's ruling of
/// 2026-09-24): a device key behind any [`Signer`] (the signer service in production, never a key in this
/// process's configuration), registered as a device of the PASS account of `user`. Transactions
/// are v5 general transactions authorised by `PassAuthenticate`: the device signs a challenge
/// bound to the call, and the runtime dispatches the call as the PASS account, whose ordinary
/// nonce it bumps. So [`Signer::account`] is the PASS account; [`Signer::device`] is the key's.
pub struct PassAccount {
    device: std::sync::Arc<dyn Signer>,
    user: [u8; 32],
}

impl PassAccount {
    /// The PASS account of the hashed user id `user`, signing with `device`.
    pub fn new(device: std::sync::Arc<dyn Signer>, user: [u8; 32]) -> PassAccount {
        PassAccount { device, user }
    }
}

impl std::fmt::Debug for PassAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Accounts only: the device's key is never formatted (NFR-8).
        f.debug_struct("PassAccount")
            .field("account", &hex::encode(self.account()))
            .field("device", &hex::encode(self.device()))
            .finish_non_exhaustive()
    }
}

impl Signer for PassAccount {
    fn public(&self) -> [u8; 33] {
        self.device.public()
    }

    fn account(&self) -> AccountId32 {
        crate::chain::kreivo::pass_account(&self.user)
    }

    fn device(&self) -> AccountId32 {
        self.device.device()
    }

    fn pass_user(&self) -> Option<[u8; 32]> {
        Some(self.user)
    }

    fn sign(&self, digest: [u8; 32]) -> BoxFuture<'_, Result<[u8; 65], SignerError>> {
        self.device.sign(digest)
    }
}

/// A key read from a file or an environment variable. See the module documentation.
pub struct DevSigner {
    key: SigningKey,
}

impl std::fmt::Debug for DevSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The account only: the key is never formatted (NFR-8).
        f.debug_struct("DevSigner")
            .field("account", &hex::encode(self.account()))
            .finish_non_exhaustive()
    }
}

impl DevSigner {
    /// A signer over 32 secret bytes.
    pub fn from_secret(secret: &[u8]) -> Result<DevSigner, SignerError> {
        let key = SigningKey::from_slice(secret)
            .map_err(|_| SignerError::Key("not a secp256k1 secret key".into()))?;
        Ok(DevSigner { key })
    }

    /// A signer over the hex secret in the file at `path` (whitespace ignored).
    pub fn from_file(path: &Path) -> Result<DevSigner, SignerError> {
        let text = Zeroizing::new(
            std::fs::read_to_string(path)
                .map_err(|e| SignerError::Key(format!("the key file could not be read: {e}")))?,
        );
        Self::from_hex(text.trim())
    }

    /// A signer over the hex secret in the environment variable `var`.
    pub fn from_env(var: &str) -> Result<DevSigner, SignerError> {
        let text = Zeroizing::new(
            std::env::var(var).map_err(|_| SignerError::Key(format!("{var} is not set")))?,
        );
        Self::from_hex(text.trim())
    }

    fn from_hex(s: &str) -> Result<DevSigner, SignerError> {
        let bytes = Zeroizing::new(
            hex::decode(s.trim_start_matches("0x"))
                .map_err(|_| SignerError::Key("the key is not hex".into()))?,
        );
        Self::from_secret(&bytes)
    }
}

impl Signer for DevSigner {
    fn public(&self) -> [u8; 33] {
        let point = self.key.verifying_key().to_encoded_point(true);
        let mut out = [0u8; 33];
        out.copy_from_slice(point.as_bytes());
        out
    }

    fn sign(&self, digest: [u8; 32]) -> BoxFuture<'_, Result<[u8; 65], SignerError>> {
        let signed = self
            .key
            .sign_prehash_recoverable(&digest)
            .map_err(|_| SignerError::Unavailable("the digest could not be signed".into()))
            .map(|(sig, rec)| {
                let mut out = [0u8; 65];
                out[..64].copy_from_slice(&sig.to_bytes());
                out[64] = rec.to_byte();
                out
            });
        Box::pin(async move { signed })
    }
}

/// Recover the compressed public key that signed `digest` (for the fake chain's verification
/// and the tests).
pub fn recover(digest: &[u8; 32], signature: &[u8; 65]) -> Option<[u8; 33]> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    let sig = Signature::from_slice(&signature[..64]).ok()?;
    let rec = RecoveryId::from_byte(signature[64])?;
    let key = VerifyingKey::recover_from_prehash(digest, &sig, rec).ok()?;
    let point = key.to_encoded_point(true);
    point.as_bytes().try_into().ok()
}

/// A key of the signer service (`F-035`; module documentation). Each signature is checked to
/// recover to the key before it is used.
pub struct ServiceSigner {
    key: std::sync::Arc<signer::client::RemoteSecp256k1>,
}

impl std::fmt::Debug for ServiceSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceSigner")
            .field("account", &hex::encode(self.account()))
            .finish_non_exhaustive()
    }
}

impl ServiceSigner {
    /// The secp256k1 key `key` of the signer `client` is connected to.
    pub fn new(
        client: std::sync::Arc<signer::client::SignerClient>,
        key: &str,
    ) -> Result<ServiceSigner, SignerError> {
        let key = signer::client::RemoteSecp256k1::new(client, key)
            .map_err(|e| SignerError::Key(e.to_string()))?;
        Ok(ServiceSigner {
            key: std::sync::Arc::new(key),
        })
    }
}

impl ServiceSigner {
    /// The secp256k1 key `handle`, of any engine that offers one (`F-042`).
    pub fn from_key(
        handle: std::sync::Arc<dyn signer::engine::KeyHandle>,
    ) -> Result<ServiceSigner, SignerError> {
        let key = signer::client::RemoteSecp256k1::from_key(handle)
            .map_err(|e| SignerError::Key(e.to_string()))?;
        Ok(ServiceSigner {
            key: std::sync::Arc::new(key),
        })
    }
}

impl Signer for ServiceSigner {
    fn public(&self) -> [u8; 33] {
        self.key.public()
    }

    fn sign(&self, digest: [u8; 32]) -> BoxFuture<'_, Result<[u8; 65], SignerError>> {
        let key = std::sync::Arc::clone(&self.key);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || key.sign_digest(digest))
                .await
                .map_err(|e| SignerError::Unavailable(e.to_string()))?
                .map_err(|e| SignerError::Unavailable(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [0x11; 32];

    #[tokio::test]
    async fn a_signature_recovers_to_the_signer() {
        let s = DevSigner::from_secret(&SECRET).unwrap();
        let digest = blake2_256(b"payload");
        let sig = s.sign(digest).await.unwrap();
        assert_eq!(recover(&digest, &sig), Some(s.public()));
        // Deterministic (RFC 6979): the same digest, the same bytes.
        assert_eq!(s.sign(digest).await.unwrap(), sig);
    }

    #[test]
    fn nfr_8_the_key_is_never_formatted() {
        let s = DevSigner::from_secret(&SECRET).unwrap();
        let shown = format!("{s:?}");
        assert!(!shown.contains(&hex::encode(SECRET)), "{shown}");
        let err = DevSigner::from_secret(&[0u8; 32]).unwrap_err().to_string();
        assert!(!err.contains(&hex::encode([0u8; 32])));
    }
}
