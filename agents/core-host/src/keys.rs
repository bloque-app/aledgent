// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Agent's Ed25519 keys behind one trait (`F-035`, guardrail 13): the authority key that
//! signs every Authority receipt, and the key runtime packages are signed with. `serve` takes
//! them from the signer service ([`signer::client::RemoteEd25519`]), so neither is ever in this
//! process; the measurement commands use fixed test keys ([`ed25519_dalek::SigningKey`]).

use std::sync::Arc;

/// Signs with an Ed25519 key whose secret this process may not hold.
pub trait AgentSigner: Send + Sync {
    /// The public key.
    fn public(&self) -> [u8; 32];
    /// The signature over `msg`. A key behind the signer fails closed rather than return an
    /// error (`signer::client::Patience`).
    fn sign(&self, msg: &[u8]) -> [u8; 64];
}

/// A shared [`AgentSigner`].
pub type SharedSigner = Arc<dyn AgentSigner>;

impl AgentSigner for ed25519_dalek::SigningKey {
    fn public(&self) -> [u8; 32] {
        self.verifying_key().to_bytes()
    }

    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        ed25519_dalek::Signer::sign(self, msg).to_bytes()
    }
}

impl AgentSigner for signer::client::RemoteEd25519 {
    fn public(&self) -> [u8; 32] {
        signer::client::RemoteEd25519::public(self)
    }

    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        signer::client::RemoteEd25519::sign(self, msg)
    }
}

/// A fixed test key: the measurement commands' (never `serve`'s).
pub fn test_key(seed: u8) -> SharedSigner {
    Arc::new(ed25519_dalek::SigningKey::from_bytes(&[seed; 32]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_key_signs_and_verifies() {
        let k = test_key(0x72);
        let sig = k.sign(b"m");
        let v = ed25519_dalek::VerifyingKey::from_bytes(&k.public()).unwrap();
        v.verify_strict(b"m", &ed25519_dalek::Signature::from_bytes(&sig))
            .unwrap();
    }
}
