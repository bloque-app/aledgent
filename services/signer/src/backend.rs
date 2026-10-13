// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The keys the signer holds, and how each answers ([`Key::answer`]).
//!
//! A key is held one of three ways ([`crate::config::KeySource`]; owner ruling
//! `key-custody-opt-in-kms`):
//!
//! - **From Vault** (any scheme; the default): the 32-byte secret is a field of a KV v1 secret the
//!   signer reads itself at start ([`crate::vault`]), held in this process's memory only and wiped
//!   on drop. Never in the book's process, never in an environment.
//! - **In AWS KMS** (opt-in, secp256k1 lane and anchor keys only): this process holds the key's
//!   identifier and public half; every signature is a KMS `Sign` under the process's role, and
//!   the key never leaves KMS.
//! - **From a file** (any scheme): development and tests.
//!
//! What each scheme answers: [`Scheme::KeyRing`] answers [`Op::KeyRing`]; [`Scheme::Clerk`] and
//! [`Scheme::Ed25519`] answer [`Op::Sign`]; [`Scheme::Secp256k1`] answers [`Op::SignDigest`];
//! every key answers [`Op::Public`]. A holder's passkey (P-256) is never here: it is the holder's
//! key, never the operator's.

use std::sync::Arc;

use ledger_core::anchor::clerk::{ClerkSigner, InProcessClerk};
use ledger_kernel::keys::{Custody, KeyRequest, RootKeys};
use zeroize::Zeroizing;

use crate::config::{KeyConfig, KeySource};
use crate::kms::{Kms, spki_secp256k1};
use crate::proto::{Op, Refusal, Scheme};
use crate::vault::SecretStore;

/// Where keys are loaded from: KMS (opt-in) and the secret store (Vault).
#[derive(Clone, Default)]
pub struct Backends {
    /// AWS KMS, for `kms` sources.
    pub kms: Option<Arc<dyn Kms>>,
    /// Vault, for `vault` sources.
    pub secrets: Option<Arc<dyn SecretStore>>,
}

enum Backend {
    KeyRing(RootKeys),
    Clerk(InProcessClerk),
    Ed25519(ed25519_dalek::SigningKey),
    Secp256k1(k256::ecdsa::SigningKey),
    Secp256k1Kms {
        key_id: String,
        public: k256::ecdsa::VerifyingKey,
        kms: Arc<dyn Kms>,
    },
}

/// One key (module documentation). Its `Debug` shows its identifier, scheme and origin only.
pub struct Key {
    /// Its identifier.
    pub id: String,
    /// Its scheme.
    pub scheme: Scheme,
    /// Its public half ([`crate::proto::KeyInfo::public`]).
    pub public: Vec<u8>,
    /// Where it is held: `vault`, `kms` or `file`.
    pub origin: &'static str,
    backend: Backend,
}

impl core::fmt::Debug for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Key")
            .field("id", &self.id)
            .field("scheme", &self.scheme)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

fn compressed(k: &k256::ecdsa::VerifyingKey) -> Vec<u8> {
    k.to_encoded_point(true).as_bytes().to_vec()
}

impl Key {
    /// A key over a 32-byte secret, as `scheme` reads one: the key-ring root; the clerk's
    /// canonical scalar; an Ed25519 seed; a secp256k1 secret.
    pub fn from_secret(
        id: &str,
        scheme: Scheme,
        secret: Zeroizing<Vec<u8>>,
        origin: &'static str,
    ) -> Result<Key, String> {
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
        Ok(Key {
            id: id.to_owned(),
            scheme,
            public,
            origin,
            backend,
        })
    }

    /// Load the key `c` describes, from `b`.
    pub async fn load(c: &KeyConfig, b: &Backends) -> Result<Key, String> {
        let scheme = Scheme::from(c.scheme);
        match &c.source {
            KeySource::File { path } => {
                let text = Zeroizing::new(
                    std::fs::read_to_string(path)
                        .map_err(|e| format!("key {}: {}: {e}", c.id, path.display()))?,
                );
                Key::from_hex(&c.id, scheme, &text, "file")
            }
            KeySource::Vault { path, field } => {
                let store = b
                    .secrets
                    .as_ref()
                    .ok_or_else(|| format!("key {}: needs Vault (feature `vault`)", c.id))?;
                let fields = store
                    .read(path)
                    .await
                    .map_err(|e| format!("key {}: {e}", c.id))?;
                let value = fields
                    .get(field)
                    .ok_or_else(|| format!("key {}: {path} has no field {field}", c.id))?;
                Key::from_hex(&c.id, scheme, value, "vault")
            }
            KeySource::Kms { key_id } => {
                if scheme != Scheme::Secp256k1 {
                    return Err(format!("key {}: KMS is for secp256k1 keys only", c.id));
                }
                let kms = b
                    .kms
                    .clone()
                    .ok_or_else(|| format!("key {}: needs AWS KMS (feature `kms`)", c.id))?;
                let der = kms
                    .public_key(key_id)
                    .await
                    .map_err(|e| format!("key {}: {e}", c.id))?;
                let public = spki_secp256k1(&der)
                    .ok_or_else(|| format!("key {}: not an ECC_SECG_P256K1 key", c.id))?;
                Ok(Key {
                    id: c.id.clone(),
                    scheme,
                    public: compressed(&public),
                    origin: "kms",
                    backend: Backend::Secp256k1Kms {
                        key_id: key_id.clone(),
                        public,
                        kms,
                    },
                })
            }
        }
    }

    /// A key over 32 bytes of hex (`0x` and surrounding whitespace allowed). An error never
    /// carries the value.
    fn from_hex(id: &str, scheme: Scheme, text: &str, origin: &'static str) -> Result<Key, String> {
        let secret = Zeroizing::new(
            hex::decode(text.trim().trim_start_matches("0x"))
                .map_err(|_| format!("key {id}: not hex"))?,
        );
        Key::from_secret(id, scheme, secret, origin)
    }

    /// The answer to `op` (module documentation).
    pub async fn answer(&self, op: &Op) -> Result<Vec<u8>, Refusal> {
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
                recoverable(sig, rec)
            }
            (
                Op::SignDigest(d),
                Backend::Secp256k1Kms {
                    key_id,
                    public,
                    kms,
                },
            ) => {
                let der = kms
                    .sign_digest(key_id, *d)
                    .await
                    .map_err(Refusal::Unavailable)?;
                from_kms_der(&der, d, public)
            }
            _ => Err(Refusal::WrongScheme),
        }
    }
}

fn recoverable(
    sig: k256::ecdsa::Signature,
    rec: k256::ecdsa::RecoveryId,
) -> Result<Vec<u8>, Refusal> {
    let mut out = sig.to_bytes().to_vec();
    out.push(rec.to_byte());
    Ok(out)
}

/// KMS's DER ECDSA signature over `digest` as the chain takes it: `s` made low, the recovery id
/// found against `public` (`r ‖ s ‖ v`, 65 bytes).
pub fn from_kms_der(
    der: &[u8],
    digest: &[u8; 32],
    public: &k256::ecdsa::VerifyingKey,
) -> Result<Vec<u8>, Refusal> {
    use k256::ecdsa::{RecoveryId, Signature};
    let bad = |what: &str| Refusal::Unavailable(format!("KMS: {what}"));
    let sig = Signature::from_der(der).map_err(|_| bad("not a DER ECDSA signature"))?;
    let sig = sig.normalize_s().unwrap_or(sig);
    let rec = RecoveryId::trial_recovery_from_prehash(public, digest, &sig)
        .map_err(|_| bad("the signature does not recover to the key"))?;
    recoverable(sig, rec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::recover_secp256k1;
    use crate::kms::testing::FakeKms;

    fn kms() -> (Arc<FakeKms>, Arc<dyn Kms>) {
        let f = Arc::new(FakeKms::default());
        (Arc::clone(&f), f as Arc<dyn Kms>)
    }

    #[tokio::test]
    async fn nfr_8_a_kms_secp256k1_key_signs_recoverably_with_a_low_s() {
        let (fake, k) = kms();
        fake.add_secp256k1("lane", [0x11; 32]);
        let c = KeyConfig {
            id: "lane-1".into(),
            scheme: crate::config::SchemeName::Secp256k1,
            source: KeySource::Kms {
                key_id: "lane".into(),
            },
        };
        let b = Backends {
            kms: Some(k),
            secrets: None,
        };
        let key = Key::load(&c, &b).await.unwrap();
        assert_eq!(key.origin, "kms");
        let local = Key::from_secret(
            "x",
            Scheme::Secp256k1,
            Zeroizing::new(vec![0x11; 32]),
            "file",
        )
        .unwrap();
        assert_eq!(key.public, local.public);
        // The fake gives a high s every other time: both come out low and recover to the key.
        for i in 0..4u8 {
            let d = [i; 32];
            let sig: [u8; 65] = key
                .answer(&Op::SignDigest(d))
                .await
                .unwrap()
                .try_into()
                .unwrap();
            assert_eq!(
                recover_secp256k1(&d, &sig).map(|p| p.to_vec()),
                Some(key.public.clone())
            );
        }
        assert_eq!(
            key.answer(&Op::Sign(vec![1])).await,
            Err(Refusal::WrongScheme)
        );
    }

    /// The default: every scheme from a field of a Vault secret; a missing field or Vault, a
    /// value that is not hex, all refused without the value in the error.
    #[tokio::test]
    async fn nfr_8_keys_load_from_vault_and_never_echo_a_value() {
        use crate::vault::testing::MemSecrets;
        let mut mem = MemSecrets::default();
        mem.secrets.insert(
            "secret/aledgent-signer/core".into(),
            [
                ("keyring".to_owned(), hex::encode([0x33u8; 32])),
                ("bad".to_owned(), "not-hex-SECRETVALUE".to_owned()),
            ]
            .into_iter()
            .collect(),
        );
        let b = Backends {
            kms: None,
            secrets: Some(Arc::new(mem)),
        };
        let c = |field: &str| KeyConfig {
            id: "keyring".into(),
            scheme: crate::config::SchemeName::KeyRing,
            source: KeySource::Vault {
                path: "secret/aledgent-signer/core".into(),
                field: field.into(),
            },
        };
        let key = Key::load(&c("keyring"), &b).await.unwrap();
        assert_eq!(key.origin, "vault");
        assert_eq!(
            key.public,
            RootKeys::new([0x33; 32]).registrar_key().0.to_vec()
        );
        assert!(
            Key::load(&c("missing"), &b)
                .await
                .unwrap_err()
                .contains("no field")
        );
        let e = Key::load(&c("bad"), &b).await.unwrap_err();
        assert!(!e.contains("SECRETVALUE"), "{e}");
        assert!(
            Key::load(&c("keyring"), &Backends::default())
                .await
                .unwrap_err()
                .contains("needs Vault")
        );
    }

    #[test]
    fn nfr_8_a_key_is_never_formatted() {
        let k = Key::from_secret("c", Scheme::Clerk, Zeroizing::new(vec![1; 32]), "file");
        // [1; 32] is a canonical scalar.
        let k = k.unwrap();
        let shown = format!("{k:?}");
        assert!(!shown.contains("1, 1, 1"), "{shown}");
        assert!(shown.contains("\"c\""));
    }
}
