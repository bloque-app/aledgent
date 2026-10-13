// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The clerk's signature: CipherClerk's Schnorr over Ristretto255 (`SCHNORR_DOMAIN`
//! `cipher-clerk/schnorr/v1`), verified with Ristretto arithmetic alone.
//!
//! A signature `(R, s)` on `msg` under key `P` is valid when `s·G = R + e·P`, with
//! `e = blake2b-512("cipher-clerk/schnorr/v1" ‖ R ‖ P ‖ msg)` reduced mod ℓ. This is exactly
//! `cipher_clerk::crypto::verify_signature`; a test holds the two to the same answers.

use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;

use crate::hash::blake2b_512;

/// CipherClerk's Schnorr domain.
pub const SCHNORR_DOMAIN: &[u8] = b"cipher-clerk/schnorr/v1";

/// Whether `sig` (`R ‖ s`) is `key`'s signature on `msg`.
pub fn verify(key: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Some(p) = CompressedRistretto(*key).decompress() else {
        return false;
    };
    let mut r_bytes = [0u8; 32];
    r_bytes.copy_from_slice(&sig[..32]);
    let Some(big_r) = CompressedRistretto(r_bytes).decompress() else {
        return false;
    };
    let mut s_bytes = [0u8; 32];
    s_bytes.copy_from_slice(&sig[32..]);
    let Some(s) = Option::<Scalar>::from(Scalar::from_canonical_bytes(s_bytes)) else {
        return false;
    };
    let e = Scalar::from_bytes_mod_order_wide(&blake2b_512(SCHNORR_DOMAIN, &[&r_bytes, key, msg]));
    RistrettoPoint::mul_base(&s) == big_r + e * p
}

/// `value · G` compressed: the Pedersen commitment to a public `value` with a zero blinding,
/// which is how a disclosure commits to the bound of an aggregate claim (the bound is public).
pub fn public_commitment(value: u64) -> [u8; 32] {
    RistrettoPoint::mul_base(&Scalar::from(value))
        .compress()
        .to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher_clerk::crypto::{Amount, AuthKey, Blinding, SecretKey, Signature, verify_signature};

    fn key() -> SecretKey {
        SecretKey::from_bytes([7u8; 32]).unwrap()
    }

    #[test]
    fn a_cipher_clerk_signature_verifies_here_and_a_tampered_one_does_not() {
        let sk = key();
        let pk = sk.public_key();
        let msg = b"a message";
        let sig = sk.sign(msg, &mut rand_core::OsRng);
        let mut wire = [0u8; 64];
        wire[..32].copy_from_slice(&sig.r);
        wire[32..].copy_from_slice(&sig.s);
        assert!(verify(&pk.0, msg, &wire));
        assert!(verify_signature(&pk, msg, &sig));
        assert!(!verify(&pk.0, b"another", &wire));
        let mut bad = wire;
        bad[40] ^= 1;
        assert!(!verify(&pk.0, msg, &bad));
        let mut r = [0u8; 32];
        r.copy_from_slice(&bad[..32]);
        let mut s = [0u8; 32];
        s.copy_from_slice(&bad[32..]);
        assert!(!verify_signature(&pk, msg, &Signature { r, s }));
        assert!(!verify(&[0xff; 32], msg, &wire));
        assert!(!verify_signature(&AuthKey([0xff; 32]), msg, &sig));
    }

    #[test]
    fn a_public_commitment_is_pedersen_with_a_zero_blinding() {
        for v in [0u64, 1, 42, u64::MAX] {
            assert_eq!(public_commitment(v), Amount::commit(v, &Blinding::ZERO).0);
        }
    }
}
