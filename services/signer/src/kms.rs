// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! AWS KMS, an opt-in backend for secp256k1 lane and anchor keys (owner ruling
//! `key-custody-opt-in-kms`): an asymmetric signature and a public key ([`Kms`]). [`AwsKms`]
//! (feature `kms`) is the AWS SDK's client, under the process's role; `testing::FakeKms`
//! (feature `testing`) answers the same calls in memory, in KMS's own encodings, for the tests.
//!
//! **Encodings, as KMS gives them.**
//!
//! - `GetPublicKey`: a DER `SubjectPublicKeyInfo` ([`spki_secp256k1`]).
//! - `Sign`, `ECC_SECG_P256K1` with `ECDSA_SHA_256` and `MessageType=DIGEST`: KMS signs the 32
//!   bytes as given, and returns a DER `(r, s)`, `s` not necessarily low. The backend makes `s`
//!   low and finds the recovery id against the public key (`crate::backend`).

use std::future::Future;
use std::pin::Pin;

/// A boxed future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the signer asks of KMS. Errors are text and never carry key material.
pub trait Kms: Send + Sync {
    /// An `ECDSA_SHA_256` signature over the 32-byte `digest` (`MessageType=DIGEST`) with the
    /// key `key_id`, DER.
    fn sign_digest<'a>(
        &'a self,
        key_id: &'a str,
        digest: [u8; 32],
    ) -> BoxFuture<'a, Result<Vec<u8>, String>>;

    /// The DER `SubjectPublicKeyInfo` of the key `key_id`.
    fn public_key<'a>(&'a self, key_id: &'a str) -> BoxFuture<'a, Result<Vec<u8>, String>>;
}

const SPKI_SECP256K1: [u8; 23] = [
    0x30, 0x56, 0x30, 0x10, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b,
    0x81, 0x04, 0x00, 0x0a, 0x03, 0x42, 0x00,
];

/// The secp256k1 public key in a DER `SubjectPublicKeyInfo` (id-ecPublicKey, secp256k1,
/// uncompressed point), as KMS returns it.
pub fn spki_secp256k1(der: &[u8]) -> Option<k256::ecdsa::VerifyingKey> {
    let point = der.strip_prefix(SPKI_SECP256K1.as_slice())?;
    k256::ecdsa::VerifyingKey::from_sec1_bytes(point).ok()
}

/// The DER `SubjectPublicKeyInfo` of a secp256k1 key.
pub fn to_spki_secp256k1(key: &k256::ecdsa::VerifyingKey) -> Vec<u8> {
    let mut out = SPKI_SECP256K1.to_vec();
    out.extend_from_slice(key.to_encoded_point(false).as_bytes());
    out
}

/// AWS KMS through the AWS SDK, under the process's credentials (the instance or task role).
#[cfg(feature = "kms")]
pub struct AwsKms {
    client: aws_sdk_kms::Client,
}

#[cfg(feature = "kms")]
impl AwsKms {
    /// A client from the environment's AWS configuration, in `region` if given.
    pub async fn from_env(region: Option<String>) -> AwsKms {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(r) = region {
            loader = loader.region(aws_config::Region::new(r));
        }
        let config = loader.load().await;
        AwsKms {
            client: aws_sdk_kms::Client::new(&config),
        }
    }
}

#[cfg(feature = "kms")]
fn aws_error<E: std::fmt::Debug>(e: aws_sdk_kms::error::SdkError<E>) -> String {
    // The service's error code and message only: KMS never echoes key material.
    match e.as_service_error() {
        Some(s) => format!("{s:?}"),
        None => e.to_string(),
    }
}

#[cfg(feature = "kms")]
impl Kms for AwsKms {
    fn sign_digest<'a>(
        &'a self,
        key_id: &'a str,
        digest: [u8; 32],
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        use aws_sdk_kms::primitives::Blob;
        use aws_sdk_kms::types::{MessageType, SigningAlgorithmSpec};
        Box::pin(async move {
            let out = self
                .client
                .sign()
                .key_id(key_id)
                .message(Blob::new(digest.to_vec()))
                .message_type(MessageType::Digest)
                .signing_algorithm(SigningAlgorithmSpec::EcdsaSha256)
                .send()
                .await
                .map_err(aws_error)?;
            out.signature
                .map(Blob::into_inner)
                .ok_or_else(|| "KMS returned no signature".to_owned())
        })
    }

    fn public_key<'a>(&'a self, key_id: &'a str) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let out = self
                .client
                .get_public_key()
                .key_id(key_id)
                .send()
                .await
                .map_err(aws_error)?;
            out.public_key
                .map(aws_sdk_kms::primitives::Blob::into_inner)
                .ok_or_else(|| "KMS returned no public key".to_owned())
        })
    }
}

/// A KMS in memory, in KMS's encodings: tests only.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use k256::ecdsa::signature::hazmat::PrehashSigner;

    use super::*;

    /// A KMS in memory. Every `Sign` is counted, and signatures come with a high `s` every other
    /// time, as KMS's may.
    #[derive(Default)]
    pub struct FakeKms {
        keys: Mutex<HashMap<String, k256::ecdsa::SigningKey>>,
        /// How many signatures it gave.
        pub signs: std::sync::atomic::AtomicUsize,
    }

    impl FakeKms {
        /// Add a secp256k1 key.
        pub fn add_secp256k1(&self, id: &str, secret: [u8; 32]) {
            let k = k256::ecdsa::SigningKey::from_slice(&secret).expect("a secp256k1 key");
            self.keys.lock().unwrap().insert(id.into(), k);
        }
    }

    impl Kms for FakeKms {
        fn sign_digest<'a>(
            &'a self,
            key_id: &'a str,
            digest: [u8; 32],
        ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            let n = self.signs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let out = match self.keys.lock().unwrap().get(key_id) {
                Some(k) => {
                    let sig: Result<k256::ecdsa::Signature, String> = k
                        .sign_prehash(&digest)
                        .map_err(|_| "not a digest".to_owned());
                    sig.map(|s| {
                        // KMS does not normalise s: give a high one every other time.
                        let s = if n % 2 == 0 {
                            let high = -*s.s();
                            k256::ecdsa::Signature::from_scalars(s.r().to_bytes(), high.to_bytes())
                                .unwrap_or(s)
                        } else {
                            s
                        };
                        s.to_der().as_bytes().to_vec()
                    })
                }
                None => Err("NotFoundException".to_owned()),
            };
            Box::pin(async move { out })
        }

        fn public_key<'a>(&'a self, key_id: &'a str) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            let out = match self.keys.lock().unwrap().get(key_id) {
                Some(k) => Ok(to_spki_secp256k1(k.verifying_key())),
                None => Err("NotFoundException".to_owned()),
            };
            Box::pin(async move { out })
        }
    }
}
