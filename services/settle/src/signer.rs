// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The signer boundary (`REQ-AR-3`, `NFR-8`, `REQ-SA-8`; guardrail 13).
//!
//! The adapter never needs a secret key, only something that signs a message as the chain's
//! `MultiSignature` verifies it: [`Signer`]. Two schemes (`F-045`):
//!
//! - **Ed25519** ([`Public::Ed25519`]): the key signs the message's bytes. Its account is its
//!   public key (`MultiSigner::Ed25519`). The lanes and the anchor key of a deployment on **native
//!   accounts** (`F-045`, the owner's ruling of 2026-10-25, bloque-ledgent#287), whose keys stay in
//!   Vault Transit.
//! - **ECDSA over secp256k1** ([`Public::Ecdsa`]): the key signs the blake2b-256 of the message, so
//!   a 32-byte digest is all it sees (`F-016` plan, proposal "signer"). Its account is the
//!   blake2b-256 of its compressed public key. The device keys of PASS accounts (`F-027`).
//!
//! One key is one nonce lane.
//!
//! - [`PassAccount`] (`F-027`): a secp256k1 key as the device key of the deployment's community PASS
//!   account on Kreivo. The device signs; the PASS account acts. Kept while the zombienet suite runs
//!   on PASS accounts; deployments move to native accounts (`F-045`).
//! - [`DevSigner`] (secp256k1) and [`DevEd25519`]: a key read from a file (or, secp256k1 only, an
//!   environment variable), for development and tests. Neither has a `Debug` or `Display` that
//!   shows the key, neither logs it, and their errors never carry it. The key is zeroised when the
//!   signer is dropped.
//! - [`ServiceSigner`] (`F-035`) and [`EngineEd25519`] (`F-042`, `F-045`): the production signers. A
//!   key behind a signature engine: the signer service (`services/signer`), a process of its own,
//!   reached on a Unix socket with peer credentials and a token; or Vault Transit, where an Ed25519
//!   key is made and never leaves. The adapter holds the key's public half, nothing else.
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

/// A public key, as the chain's `MultiSigner` carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Public {
    /// An Ed25519 key, 32 bytes.
    Ed25519([u8; 32]),
    /// A compressed secp256k1 key, 33 bytes.
    Ecdsa([u8; 33]),
}

impl Public {
    /// The account of the key (`MultiSigner::into_account`): an Ed25519 key's own bytes; the
    /// blake2b-256 of a secp256k1 key's compressed bytes.
    pub fn account(&self) -> AccountId32 {
        match self {
            Public::Ed25519(p) => *p,
            Public::Ecdsa(p) => blake2_256(p),
        }
    }
}

impl AsRef<[u8]> for Public {
    fn as_ref(&self) -> &[u8] {
        match self {
            Public::Ed25519(p) => p,
            Public::Ecdsa(p) => p,
        }
    }
}

/// A signature, as the chain's `MultiSignature` carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signature {
    /// Ed25519 over the message's bytes, 64 bytes (`MultiSignature::Ed25519`, index 0).
    Ed25519([u8; 64]),
    /// Recoverable ECDSA over the blake2b-256 of the message: `r ‖ s ‖ v`, 65 bytes
    /// (`MultiSignature::Ecdsa`, index 2).
    Ecdsa([u8; 65]),
}

impl Signature {
    /// `MultiSignature`'s SCALE encoding: the variant's index, then the bytes.
    pub fn encode_multi(&self) -> Vec<u8> {
        match self {
            Signature::Ed25519(s) => [&[0u8][..], s].concat(),
            Signature::Ecdsa(s) => [&[2u8][..], s].concat(),
        }
    }

    /// Whether this signature over `message` is `signer`'s, as `MultiSignature::verify` checks it.
    pub fn verify(&self, message: &[u8], signer: &AccountId32) -> bool {
        match self {
            Signature::Ed25519(s) => {
                ed25519_dalek::VerifyingKey::from_bytes(signer).is_ok_and(|k| {
                    k.verify_strict(message, &ed25519_dalek::Signature::from_bytes(s))
                        .is_ok()
                })
            }
            Signature::Ecdsa(s) => {
                recover(&blake2_256(message), s).is_some_and(|p| blake2_256(&p) == *signer)
            }
        }
    }
}

/// Something that signs a message for the chain. See the module documentation.
pub trait Signer: Send + Sync {
    /// The public key.
    fn public(&self) -> Public;

    /// The chain account that acts: the one whose nonce the transactions bump, that pays their
    /// fees and that the settlement contract lists as a key. For a native account, the key's own
    /// account ([`Signer::device`]); for a PASS account ([`PassAccount`]), the PASS account.
    fn account(&self) -> AccountId32 {
        self.device()
    }

    /// The account of the key itself ([`Public::account`]). A PASS account's device id.
    fn device(&self) -> AccountId32 {
        self.public().account()
    }

    /// The hashed user id of the PASS account this key is a device of, if it is one
    /// (`F-027` T-027-03). `None`: the key's own account acts, a native account (`F-045`).
    fn pass_user(&self) -> Option<[u8; 32]> {
        None
    }

    /// A signature over `message`, as `MultiSignature::verify` checks it against
    /// [`Signer::device`]: Ed25519 over the bytes, ECDSA over their blake2b-256.
    fn sign(&self, message: Vec<u8>) -> BoxFuture<'_, Result<Signature, SignerError>>;
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
    fn public(&self) -> Public {
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

    fn sign(&self, message: Vec<u8>) -> BoxFuture<'_, Result<Signature, SignerError>> {
        self.device.sign(message)
    }
}

/// A secp256k1 key read from a file or an environment variable. See the module documentation.
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

impl DevSigner {
    /// The recoverable signature over `digest`: `r ‖ s ‖ v`.
    pub fn sign_digest(&self, digest: [u8; 32]) -> Result<[u8; 65], SignerError> {
        self.key
            .sign_prehash_recoverable(&digest)
            .map_err(|_| SignerError::Unavailable("the digest could not be signed".into()))
            .map(|(sig, rec)| {
                let mut out = [0u8; 65];
                out[..64].copy_from_slice(&sig.to_bytes());
                out[64] = rec.to_byte();
                out
            })
    }
}

impl Signer for DevSigner {
    fn public(&self) -> Public {
        let point = self.key.verifying_key().to_encoded_point(true);
        let mut out = [0u8; 33];
        out.copy_from_slice(point.as_bytes());
        Public::Ecdsa(out)
    }

    fn sign(&self, message: Vec<u8>) -> BoxFuture<'_, Result<Signature, SignerError>> {
        let signed = self.sign_digest(blake2_256(&message)).map(Signature::Ecdsa);
        Box::pin(async move { signed })
    }
}

/// An Ed25519 key read from a file, for development and tests (`F-045`). See the module
/// documentation. There is no environment source: `NFR-8` (and `T-042-09`).
pub struct DevEd25519 {
    key: ed25519_dalek::SigningKey,
}

impl std::fmt::Debug for DevEd25519 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The account only: the key is never formatted (NFR-8).
        f.debug_struct("DevEd25519")
            .field("account", &hex::encode(self.account()))
            .finish_non_exhaustive()
    }
}

impl DevEd25519 {
    /// A signer over the 32-byte secret seed `secret`.
    pub fn from_secret(secret: &[u8; 32]) -> DevEd25519 {
        DevEd25519 {
            key: ed25519_dalek::SigningKey::from_bytes(secret),
        }
    }

    /// A signer over the hex seed in the file at `path` (whitespace ignored).
    pub fn from_file(path: &Path) -> Result<DevEd25519, SignerError> {
        let text = Zeroizing::new(
            std::fs::read_to_string(path)
                .map_err(|e| SignerError::Key(format!("the key file could not be read: {e}")))?,
        );
        let bytes = Zeroizing::new(
            hex::decode(text.trim().trim_start_matches("0x"))
                .map_err(|_| SignerError::Key("the key is not hex".into()))?,
        );
        let seed: Zeroizing<[u8; 32]> = Zeroizing::new(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SignerError::Key("an Ed25519 seed is 32 bytes".into()))?,
        );
        Ok(DevEd25519::from_secret(&seed))
    }
}

impl Signer for DevEd25519 {
    fn public(&self) -> Public {
        Public::Ed25519(self.key.verifying_key().to_bytes())
    }

    fn sign(&self, message: Vec<u8>) -> BoxFuture<'_, Result<Signature, SignerError>> {
        use ed25519_dalek::Signer as _;
        let sig = Signature::Ed25519(self.key.sign(&message).to_bytes());
        Box::pin(async move { Ok(sig) })
    }
}

/// The domain of the probe [`probe`] signs (`F-045`): wrapped as `<Bytes>…</Bytes>`, the convention for
/// signing a message that is no transaction, so that no probe signature can ever be read as one (a
/// signed payload starts with a call's pallet and call indices, and binds the genesis hash and the
/// runtime's versions; ASCII text does neither).
pub const PROBE_DOMAIN: &str = "aledgent-settle/keys-probe/v1";

/// The probe message for the key whose account is `account`: `<Bytes>` ‖ [`PROBE_DOMAIN`] ‖ `:` ‖ the
/// account, hex ‖ `</Bytes>`.
pub fn probe_message(account: &AccountId32) -> Vec<u8> {
    format!("<Bytes>{PROBE_DOMAIN}:{}</Bytes>", hex::encode(account)).into_bytes()
}

/// Have `signer` sign its probe and verify the signature against its own public key, the one pinned
/// when the key was opened (`F-045`). Opening a key reads its public half only: this is what proves the
/// key can *sign* (for a Vault Transit key, that the role may use `sign/<key>`, not only `keys/<key>`).
pub async fn probe(signer: &dyn Signer) -> Result<(), SignerError> {
    let account = signer.device();
    let message = probe_message(&account);
    let signature = signer.sign(message.clone()).await?;
    if signature.verify(&message, &account) {
        Ok(())
    } else {
        Err(SignerError::Key(
            "the probe's signature does not verify against the key's public half".into(),
        ))
    }
}

/// Refuse two lanes, or a lane and the anchor key, on one account or one key (`F-045`): each is a nonce
/// lane of its own, and the contract lists each once (`set_keys` refuses a duplicate).
pub fn distinct(signers: &[&dyn Signer]) -> Result<(), String> {
    let mut accounts = std::collections::BTreeSet::new();
    let mut devices = std::collections::BTreeSet::new();
    for s in signers {
        if !accounts.insert(s.account()) || !devices.insert(s.device()) {
            return Err(format!(
                "the account {} is configured twice (lanes and the anchor key are each a key of \
                 their own)",
                hex::encode(s.account())
            ));
        }
    }
    Ok(())
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
    fn public(&self) -> Public {
        Public::Ecdsa(self.key.public())
    }

    fn sign(&self, message: Vec<u8>) -> BoxFuture<'_, Result<Signature, SignerError>> {
        let key = std::sync::Arc::clone(&self.key);
        let digest = blake2_256(&message);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || key.sign_digest(digest))
                .await
                .map_err(|e| SignerError::Unavailable(e.to_string()))?
                .map(Signature::Ecdsa)
                .map_err(|e| SignerError::Unavailable(e.to_string()))
        })
    }
}

/// An Ed25519 key behind a signature engine (`F-042`, `F-045`): Vault Transit, where the key is made
/// and never leaves, or the signer service. A native account: its account is its public key. Each
/// signature is checked against the public key before it is used ([`signer::client::RemoteEd25519`]).
///
/// It signs through `try_sign` only, so an engine that does not answer (Vault down, a token refused
/// after its one new login) fails the step that asked, as [`SignerError::Unavailable`], and never the
/// process: the next step asks again.
pub struct EngineEd25519 {
    key: std::sync::Arc<signer::client::RemoteEd25519>,
}

impl std::fmt::Debug for EngineEd25519 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineEd25519")
            .field("account", &hex::encode(self.account()))
            .finish_non_exhaustive()
    }
}

/// The patience an [`EngineEd25519`] is made with. It signs through `try_sign` only, so the patience is
/// never consulted (`sign` would, and this type never calls it); and one that stops the process is
/// refused for an API provider's key.
const NEVER_WAITS: signer::client::Patience = signer::client::Patience {
    wait: std::time::Duration::ZERO,
    on_failure: signer::client::OnFailure::Panic,
};

impl EngineEd25519 {
    /// The Ed25519 key `handle`, of any engine that offers one.
    pub fn from_key(
        handle: std::sync::Arc<dyn signer::engine::KeyHandle>,
    ) -> Result<EngineEd25519, SignerError> {
        let key = signer::client::RemoteEd25519::from_key(handle, NEVER_WAITS)
            .map_err(|e| SignerError::Key(e.to_string()))?;
        Ok(EngineEd25519 {
            key: std::sync::Arc::new(key),
        })
    }

    /// The Ed25519 key `key` of the signer service `client` is connected to (the form written before
    /// `F-042`).
    pub fn from_service(
        client: std::sync::Arc<signer::client::SignerClient>,
        key: &str,
    ) -> Result<EngineEd25519, SignerError> {
        let key = signer::client::RemoteEd25519::new(client, key, NEVER_WAITS)
            .map_err(|e| SignerError::Key(e.to_string()))?;
        Ok(EngineEd25519 {
            key: std::sync::Arc::new(key),
        })
    }
}

impl Signer for EngineEd25519 {
    fn public(&self) -> Public {
        Public::Ed25519(self.key.public())
    }

    fn sign(&self, message: Vec<u8>) -> BoxFuture<'_, Result<Signature, SignerError>> {
        let key = std::sync::Arc::clone(&self.key);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || key.try_sign(&message))
                .await
                .map_err(|e| SignerError::Unavailable(e.to_string()))?
                .map(Signature::Ed25519)
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
        let Signature::Ecdsa(sig) = s.sign(b"payload".to_vec()).await.unwrap() else {
            panic!("a secp256k1 key signs ECDSA");
        };
        assert_eq!(
            recover(&blake2_256(b"payload"), &sig).map(Public::Ecdsa),
            Some(s.public())
        );
        // Deterministic (RFC 6979): the same message, the same bytes.
        assert_eq!(
            s.sign(b"payload".to_vec()).await.unwrap(),
            Signature::Ecdsa(sig)
        );
        assert!(Signature::Ecdsa(sig).verify(b"payload", &s.account()));
        assert!(!Signature::Ecdsa(sig).verify(b"other", &s.account()));
    }

    /// A signer that answers with another key's signature, or not at all.
    struct Wrong(DevEd25519, Option<DevEd25519>);

    impl Signer for Wrong {
        fn public(&self) -> Public {
            self.0.public()
        }

        fn sign(&self, message: Vec<u8>) -> BoxFuture<'_, Result<Signature, SignerError>> {
            match &self.1 {
                Some(other) => other.sign(message),
                None => Box::pin(async {
                    Err(SignerError::Unavailable(
                        "POST transit-aledgent-signer/sign/lane-1: 403 permission denied".into(),
                    ))
                }),
            }
        }
    }

    /// `F-045`: the probe proves a key signs, and signs as the public half that was pinned: a key that
    /// reads but may not sign (Vault's 403 on `sign/`), or one that answers with another key, fails it.
    #[tokio::test]
    async fn f_045_the_probe_proves_the_key_signs_as_its_pinned_public_half() {
        let good = DevEd25519::from_secret(&SECRET);
        assert_eq!(probe(&good).await, Ok(()));
        assert_eq!(
            probe(&DevSigner::from_secret(&SECRET).unwrap()).await,
            Ok(())
        );
        let denied = Wrong(DevEd25519::from_secret(&SECRET), None);
        assert!(matches!(
            probe(&denied).await,
            Err(SignerError::Unavailable(e)) if e.contains("403")
        ));
        let other = Wrong(
            DevEd25519::from_secret(&SECRET),
            Some(DevEd25519::from_secret(&[0x22; 32])),
        );
        assert!(matches!(probe(&other).await, Err(SignerError::Key(_))));
        // The probe is text in `<Bytes>`, bound to the account: never a transaction's payload.
        let m = probe_message(&good.account());
        assert!(m.starts_with(b"<Bytes>") && m.ends_with(b"</Bytes>"));
        assert_ne!(m, probe_message(&[0x11; 32]));
    }

    /// `F-045`: no account twice among the lanes and the anchor key.
    #[test]
    fn f_045_an_account_configured_twice_is_refused() {
        let a = DevEd25519::from_secret(&SECRET);
        let a2 = DevEd25519::from_secret(&SECRET);
        let b = DevEd25519::from_secret(&[0x22; 32]);
        assert_eq!(distinct(&[&a, &b]), Ok(()));
        assert!(distinct(&[&a, &b, &a2]).unwrap_err().contains("twice"));
        // A PASS account and its device are told apart; two PASS accounts on one device are not.
        let d: std::sync::Arc<dyn Signer> =
            std::sync::Arc::new(DevSigner::from_secret(&SECRET).unwrap());
        let p1 = PassAccount::new(std::sync::Arc::clone(&d), [1; 32]);
        let p2 = PassAccount::new(std::sync::Arc::clone(&d), [2; 32]);
        assert!(distinct(&[&p1, &p2]).is_err());
    }

    /// `F-045`: a native Ed25519 account is its public key, and its signature is over the message's
    /// bytes, as `MultiSignature::Ed25519` verifies it.
    #[tokio::test]
    async fn f_045_an_ed25519_account_is_its_public_key_and_signs_the_bytes() {
        let s = DevEd25519::from_secret(&SECRET);
        let Public::Ed25519(public) = s.public() else {
            panic!("an Ed25519 key");
        };
        assert_eq!(s.account(), public);
        assert_eq!(s.device(), public);
        assert_eq!(s.pass_user(), None);
        let sig = s.sign(b"payload".to_vec()).await.unwrap();
        assert!(matches!(sig, Signature::Ed25519(_)));
        assert!(sig.verify(b"payload", &s.account()));
        assert!(!sig.verify(b"payload", &[0x11; 32]));
        assert_eq!(sig.encode_multi()[0], 0);
        assert_eq!(sig.encode_multi().len(), 65);
        assert!(!format!("{s:?}").contains(&hex::encode(SECRET)));
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
