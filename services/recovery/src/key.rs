// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The recovery key: the one secret this service holds (`DEC-13`, `REQ-AU-15`, `NFR-8`).
//!
//! It is kept apart from everything else on purpose:
//!
//! - It lives only in this crate, which only the recovery authority's team runs. The process that
//!   holds the book is given its **public** half ([`SigningKey::public_key`]) in its deployment
//!   configuration, and nothing more.
//! - [`SigningKey`] has no `Clone`, no `Display`, no serialisation, and a `Debug` that prints
//!   nothing of it. Its bytes are zeroed when it is dropped. No error it returns quotes what it
//!   was given (`REQ-AU-8`).
//! - There is no default key and no fallback: a key is read from where the deployment puts it, or
//!   the service does not start (`REQ-AU-8`).
//! - The only thing it can do is sign an approval of one pending recovery
//!   ([`crate::RecoveryService::approve_recovery`]). Its signing function is not public.

use std::path::Path;

use cipher_clerk::crypto::{SecretKey, Signature, blake2b_256};
use ledger_core::custody::RecoveryKey;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use zeroize::Zeroize;

const NONCE_DOMAIN: &[u8] = b"aledgent/recovery-authority/nonce/v1";

/// Why a key could not be loaded. Never says what the bytes were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// No key was configured.
    Missing,
    /// The key could not be read.
    Unreadable,
    /// The key is not 32 bytes of hex encoding a canonical scalar.
    Malformed,
}

/// The recovery authority's secret key.
pub struct SigningKey(SecretKey);

impl core::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

impl SigningKey {
    /// A key from its 32 canonical bytes. The copy handed in is zeroed.
    pub fn from_bytes(mut bytes: [u8; 32]) -> Result<SigningKey, KeyError> {
        let key = SecretKey::from_bytes(bytes).ok_or(KeyError::Malformed);
        bytes.zeroize();
        key.map(SigningKey)
    }

    /// A key from 64 hexadecimal characters, surrounding whitespace ignored.
    pub fn from_hex(text: &str) -> Result<SigningKey, KeyError> {
        let text = text.trim();
        if text.len() != 64 {
            return Err(KeyError::Malformed);
        }
        let mut bytes = [0u8; 32];
        for (i, pair) in text.as_bytes().chunks(2).enumerate() {
            let hi = hex_digit(pair[0]);
            let lo = hex_digit(pair[1]);
            match (hi, lo) {
                (Some(h), Some(l)) => bytes[i] = (h << 4) | l,
                _ => {
                    bytes.zeroize();
                    return Err(KeyError::Malformed);
                }
            }
        }
        SigningKey::from_bytes(bytes)
    }

    /// A key from a file holding its hex encoding: where the deployment puts it.
    pub fn from_file(path: &Path) -> Result<SigningKey, KeyError> {
        let mut text = std::fs::read_to_string(path).map_err(|_| KeyError::Unreadable)?;
        let key = SigningKey::from_hex(&text);
        text.zeroize();
        key
    }

    /// Its public half: what the core's deployment configuration names as the recovery key.
    pub fn public_key(&self) -> RecoveryKey {
        RecoveryKey(self.0.public_key().0)
    }

    /// Sign `message`, with a nonce derived from the key and the message: deterministic, and
    /// unpredictable without the key.
    pub(crate) fn sign(&self, message: &[u8; 32]) -> Signature {
        let mut secret = self.0.to_bytes();
        let mut seed = blake2b_256(NONCE_DOMAIN, &[&secret, message]);
        secret.zeroize();
        let mut rng = ChaCha20Rng::from_seed(seed);
        seed.zeroize();
        self.0.sign(message, &mut rng)
    }
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_au_8_a_key_prints_nothing_of_itself() {
        let k = SigningKey::from_bytes([7; 32]).unwrap();
        assert_eq!(format!("{k:?}"), "SigningKey(<redacted>)");
    }

    #[test]
    fn a_malformed_key_is_refused_without_quoting_it() {
        assert_eq!(
            SigningKey::from_hex("abc").unwrap_err(),
            KeyError::Malformed
        );
        assert_eq!(
            SigningKey::from_hex(&"zz".repeat(32)).unwrap_err(),
            KeyError::Malformed
        );
        // Not a canonical scalar.
        assert_eq!(
            SigningKey::from_bytes([0xFF; 32]).unwrap_err(),
            KeyError::Malformed
        );
        assert_eq!(
            SigningKey::from_file(Path::new("/nonexistent/recovery.key")).unwrap_err(),
            KeyError::Unreadable
        );
    }

    #[test]
    fn a_key_loads_from_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery.key");
        std::fs::write(&path, format!("{}\n", "07".repeat(32))).unwrap();
        let from_file = SigningKey::from_file(&path).unwrap();
        assert_eq!(
            from_file.public_key(),
            SigningKey::from_bytes([7; 32]).unwrap().public_key()
        );
    }

    #[test]
    fn signatures_are_deterministic() {
        let k = SigningKey::from_bytes([7; 32]).unwrap();
        assert_eq!(k.sign(&[1; 32]), k.sign(&[1; 32]));
        assert_ne!(k.sign(&[1; 32]), k.sign(&[2; 32]));
    }
}
