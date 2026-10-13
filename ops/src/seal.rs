// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Sealing of backup artifacts that hold secrets (`F-006` decision 16, `REQ-RT-6`).
//!
//! The core's openings — the plaintext balances and their blindings — are not in the `F-014`
//! export: they cross by a sealed key-custody step of their own. A backup keeps them as a
//! separate artifact, sealed with ChaCha20-Poly1305 under a **sealing key** that lives in the
//! deployment's KMS, never beside the backup and never in this repository. The key root of the
//! core is not backed up here at all: it is the signer's, and rests in Vault, whose own backup holds
//! it (`F-035`, `NFR-8`).
//!
//! A [`SealingKey`] has no `Debug` that shows it, and is wiped on drop. It is read from an
//! environment variable holding 64 hex digits, never from an argument (`REQ-AU-8`).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use zeroize::Zeroize;

use crate::Error;

/// The environment variable the backup and restore jobs read the sealing key from.
pub const ENV_SEALING_KEY: &str = "ALEDGENT_OPS_SEALING_KEY";

/// The key backups are sealed under. See the module documentation.
pub struct SealingKey([u8; 32]);

impl std::fmt::Debug for SealingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SealingKey(<redacted>)")
    }
}

impl Drop for SealingKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl SealingKey {
    /// A key from its bytes. The caller should wipe its own copy.
    pub fn from_bytes(bytes: [u8; 32]) -> SealingKey {
        SealingKey(bytes)
    }

    /// The key in the environment variable `var`: 64 hex digits. Refuses a missing or malformed
    /// value without echoing it.
    pub fn from_env(var: &str) -> Result<SealingKey, Error> {
        let mut text = std::env::var(var)
            .map_err(|_| Error::Config(format!("{var} is not set: refusing to run")))?;
        let decoded = hex::decode(text.trim());
        text.zeroize();
        let mut bytes = decoded.map_err(|_| Error::Config(format!("{var} is not hex")))?;
        let key: Result<[u8; 32], _> = bytes.as_slice().try_into();
        bytes.zeroize();
        key.map(SealingKey)
            .map_err(|_| Error::Config(format!("{var} is not 32 bytes")))
    }

    /// Seal `plain` bound to `context` (the artifact's name and its backup's identity): a random
    /// 12-byte nonce, then the ciphertext and its tag.
    pub fn seal(&self, plain: &[u8], context: &[u8]) -> Result<Vec<u8>, Error> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.0));
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| Error::Seal)?;
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plain,
                    aad: context,
                },
            )
            .map_err(|_| Error::Seal)?;
        let mut out = Vec::with_capacity(12 + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Open what [`SealingKey::seal`] sealed under the same `context`.
    pub fn open(&self, sealed: &[u8], context: &[u8]) -> Result<Vec<u8>, Error> {
        if sealed.len() < 12 {
            return Err(Error::Seal);
        }
        let (nonce, body) = sealed.split_at(12);
        ChaCha20Poly1305::new(Key::from_slice(&self.0))
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: context,
                },
            )
            .map_err(|_| Error::Seal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_artifact_opens_only_under_its_key_and_context() {
        let k = SealingKey::from_bytes([3; 32]);
        let sealed = k.seal(b"openings", b"ctx").unwrap();
        assert!(!sealed.windows(8).any(|w| w == b"openings"));
        assert_eq!(k.open(&sealed, b"ctx").unwrap(), b"openings");
        assert_eq!(k.open(&sealed, b"other"), Err(Error::Seal));
        assert_eq!(
            SealingKey::from_bytes([4; 32]).open(&sealed, b"ctx"),
            Err(Error::Seal)
        );
        assert_eq!(format!("{k:?}"), "SealingKey(<redacted>)");
    }
}
