// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The clerk key: what signs disclosures (`F-017` §Clerk key, `DEC-3`).
//!
//! The core signs a disclosure's claims and its envelope with the deployment's clerk key. It sees
//! the key only through [`ClerkSigner`] — sign these bytes, tell me your public key — so the key
//! itself never enters the service store, a log, an export or a reply (`REQ-RT-6`, `REQ-AU-8`,
//! `NFR-8`). The plan has the key reach the core through `F-005`'s private-input path; `F-005` is
//! blocked on issue #38, so this module gives the trait and one in-process implementation,
//! [`InProcessClerk`], which the in-process core holds. Whatever #38 rules, the core's side is
//! this trait.
//!
//! **No randomness inside the core** (`REQ-RT-7`). A Schnorr signature needs a nonce;
//! [`InProcessClerk`] derives it deterministically from the secret and the message,
//! `k = blake2b-512("aledgent/clerk/nonce/v1" ‖ secret ‖ message) mod ℓ`, as Ed25519 does, so a
//! repeated signature of the same bytes is the same signature and nothing is drawn from the
//! environment. CipherClerk's own signing helpers draw from `OsRng` behind its `signing`
//! feature, which pulls in `getrandom`; the core does not enable that feature.
//!
//! The signature is CipherClerk's Schnorr over Ristretto255 (`cipher-clerk/schnorr/v1`), so
//! `cipher_clerk::crypto::verify_signature` and the verifier ([`verify::schnorr`]) accept it.

use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use zeroize::Zeroize;

use verify::schnorr::SCHNORR_DOMAIN;
use verify::{blake2b_256, blake2b_512};

const NONCE_DOMAIN: &[u8] = b"aledgent/clerk/nonce/v1";

/// What the core needs of the clerk key: its public half, and a signature over bytes. Nothing
/// else — the secret never crosses this trait.
pub trait ClerkSigner {
    /// The public key, compressed Ristretto: what a recipient checks signatures against, and what
    /// the deployment publishes on its instance record.
    fn public_key(&self) -> [u8; 32];

    /// The signature `R ‖ s` over `msg`, deterministic in `msg`.
    fn sign(&self, msg: &[u8]) -> [u8; 64];
}

/// The clerk key held in process: a secret scalar that is never cloned, never printed, and
/// zeroised when dropped.
pub struct InProcessClerk {
    secret: Scalar,
    public: [u8; 32],
}

impl core::fmt::Debug for InProcessClerk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InProcessClerk")
            .field("secret", &"<redacted>")
            .field("public", &self.public)
            .finish()
    }
}

impl Drop for InProcessClerk {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

impl InProcessClerk {
    /// The clerk whose secret is the canonical scalar `secret`. The caller's copy is zeroised.
    /// `None` if the bytes are not a canonical, non-zero scalar.
    pub fn from_secret(mut secret: [u8; 32]) -> Option<InProcessClerk> {
        let s = Option::<Scalar>::from(Scalar::from_canonical_bytes(secret));
        secret.zeroize();
        let s = s.filter(|s| *s != Scalar::ZERO)?;
        let public = RistrettoPoint::mul_base(&s).compress().to_bytes();
        Some(InProcessClerk { secret: s, public })
    }

    /// The clerk whose secret is derived from `seed` (reduced wide): for development and tests.
    /// The caller's copy is zeroised.
    pub fn from_seed(mut seed: [u8; 32]) -> InProcessClerk {
        let mut wide = blake2b_512(b"aledgent/clerk/seed/v1", &[&seed]);
        seed.zeroize();
        let s = Scalar::from_bytes_mod_order_wide(&wide);
        wide.zeroize();
        let public = RistrettoPoint::mul_base(&s).compress().to_bytes();
        InProcessClerk { secret: s, public }
    }
}

impl ClerkSigner for InProcessClerk {
    fn public_key(&self) -> [u8; 32] {
        self.public
    }

    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let mut sk = self.secret.to_bytes();
        let mut wide = blake2b_512(NONCE_DOMAIN, &[&sk, msg]);
        sk.zeroize();
        let mut k = Scalar::from_bytes_mod_order_wide(&wide);
        wide.zeroize();
        let r = RistrettoPoint::mul_base(&k).compress().to_bytes();
        let e = Scalar::from_bytes_mod_order_wide(&blake2b_512(
            SCHNORR_DOMAIN,
            &[&r, &self.public, msg],
        ));
        let s = k + e * self.secret;
        k.zeroize();
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&r);
        out[32..].copy_from_slice(&s.to_bytes());
        out
    }
}

/// An identifier of a clerk key that reveals nothing of it: `blake2b-256("aledgent/clerk-id/v1" ‖
/// public key)`, for logs and operator views.
pub fn key_id(public: &[u8; 32]) -> [u8; 32] {
    blake2b_256(b"aledgent/clerk-id/v1", &[public])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use cipher_clerk::crypto::{AuthKey, Signature, verify_signature};

    #[test]
    fn a_clerk_signature_is_cipher_clerks_schnorr_and_deterministic() {
        let clerk = InProcessClerk::from_seed([3; 32]);
        let pk = clerk.public_key();
        let a = clerk.sign(b"message");
        assert_eq!(a, clerk.sign(b"message"));
        assert_ne!(a, clerk.sign(b"other"));
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&a[..32]);
        s.copy_from_slice(&a[32..]);
        assert!(verify_signature(
            &AuthKey(pk),
            b"message",
            &Signature { r, s }
        ));
        assert!(verify::schnorr::verify(&pk, b"message", &a));
        assert!(!verify::schnorr::verify(&pk, b"other", &a));
    }

    #[test]
    fn nfr_8_the_secret_is_never_printed() {
        let secret = Scalar::from(42u64).to_bytes();
        let clerk = InProcessClerk::from_secret(secret).unwrap();
        let shown = format!("{clerk:?}");
        assert!(shown.contains("redacted"));
        let hex: alloc::string::String = secret.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!shown.contains(&hex));
        assert!(InProcessClerk::from_secret([0; 32]).is_none());
        assert!(InProcessClerk::from_secret([0xff; 32]).is_none());
    }
}
