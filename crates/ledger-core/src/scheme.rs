// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Credential schemes (`DEC-19`, `F-028`): how possession of a credential is proven.
//!
//! A [`crate::principal::Verifier`] names its scheme. The core holds public material only
//! (`REQ-AU-8`) and checks its **shape** when a verifier joins a set. It verifies `key` proofs
//! itself (reclaim proofs, recovery approvals). Ingress verifies passkey assertions and API keys
//! (`crates/authn`), because the guest carries no P-256, JSON or clock (`DEC-19`).
//!
//! | Scheme | Tag | Material |
//! |---|---|---|
//! | [`Scheme::Key`] | 1 | CipherClerk's Schnorr public key over Ristretto255, 32 bytes. Not shape-checked: every verifier written before `F-028` is one, and a malformed key never verifies |
//! | [`Scheme::WebAuthnEs256`] | 2 | A [`PasskeyKey`] in its canonical bytes |
//! | [`Scheme::ApiKey`] | 3 | The SHA-256 of the key's secret, 32 bytes |
//!
//! Tags are never reused. A scheme added later is one more tag.

use alloc::string::String;
use alloc::vec::Vec;

/// How possession of a credential is proven (`DEC-19`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum Scheme {
    /// A Schnorr signature over Ristretto255, CipherClerk's: the scheme of every verifier written
    /// before `F-028`, of reclaim proofs and of recovery approvals.
    Key,
    /// A passkey: a WebAuthn assertion, ECDSA over P-256 with SHA-256 (COSE algorithm `-7`).
    #[cfg_attr(feature = "serde", serde(rename = "webauthn-es256"))]
    WebAuthnEs256,
    /// An API key: a 32-byte secret drawn by ingress and shown once. The verifier is its SHA-256.
    ApiKey,
}

impl Scheme {
    /// Every scheme, in tag order.
    pub const ALL: [Scheme; 3] = [Scheme::Key, Scheme::WebAuthnEs256, Scheme::ApiKey];

    /// Its tag: in digests and in the export. Never reused.
    pub const fn tag(self) -> u8 {
        match self {
            Scheme::Key => 1,
            Scheme::WebAuthnEs256 => 2,
            Scheme::ApiKey => 3,
        }
    }

    /// The scheme of a tag.
    pub fn from_tag(tag: u8) -> Option<Scheme> {
        Scheme::ALL.into_iter().find(|s| s.tag() == tag)
    }

    /// Its name on the wire.
    pub const fn name(self) -> &'static str {
        match self {
            Scheme::Key => "key",
            Scheme::WebAuthnEs256 => "webauthn-es256",
            Scheme::ApiKey => "api-key",
        }
    }

    /// The scheme a wire name names.
    pub fn parse(name: &str) -> Option<Scheme> {
        Scheme::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Whether `material` has this scheme's shape. `key` is not checked (see the module
    /// documentation).
    pub fn well_formed(self, material: &[u8]) -> bool {
        match self {
            Scheme::Key => true,
            Scheme::WebAuthnEs256 => PasskeyKey::decode(material).is_some(),
            Scheme::ApiKey => material.len() == API_KEY_DIGEST_LEN,
        }
    }
}

/// The length of an API key's verifier: a SHA-256.
pub const API_KEY_DIGEST_LEN: usize = 32;

/// The version of [`PasskeyKey`]'s canonical bytes.
const PASSKEY_VERSION: u8 = 1;
/// COSE algorithm `-7`, ES256: the one algorithm of [`Scheme::WebAuthnEs256`].
const ALG_ES256: u8 = 1;
/// The flag bit for backup eligibility (WebAuthn's BE).
const FLAG_BACKUP_ELIGIBLE: u8 = 0b0000_0001;
/// The longest `rpId`: a DNS name.
pub const MAX_RP_ID: usize = 253;

/// A passkey's public material (`Scheme::WebAuthnEs256`): what an assertion is checked against.
///
/// - `handle` is `blake2_256(rawId)`. That is how Kreivo `Pass` names a WebAuthn device
///   (`DeviceId`), and the raw credential id is never on chain, so a native registration and an
///   import look the same.
/// - `public_key` is the SEC1 uncompressed P-256 point, `0x04 ‖ x ‖ y`.
/// - `rp_id` is the relying party the passkey was created for. A browser offers it only to pages
///   under that host (`RISK-10`).
/// - `backup_eligible` must not change between assertions (WebAuthn's BE).
/// - `sign_count` is the authenticator's counter when the passkey was registered or imported: the
///   floor ingress's high-water mark starts from.
#[derive(Clone, PartialEq, Eq)]
pub struct PasskeyKey {
    /// `blake2_256(rawId)`.
    pub handle: [u8; 32],
    /// SEC1 uncompressed P-256 point.
    pub public_key: [u8; 65],
    /// The relying party id: lower-case DNS name.
    pub rp_id: String,
    /// WebAuthn's BE flag at registration.
    pub backup_eligible: bool,
    /// The counter at registration or import.
    pub sign_count: u32,
}

impl core::fmt::Debug for PasskeyKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Public material, but kept out of logs like every verifier (`REQ-AU-8`).
        f.debug_struct("PasskeyKey")
            .field("rp_id", &self.rp_id)
            .finish_non_exhaustive()
    }
}

impl PasskeyKey {
    /// Its canonical bytes: version, algorithm, handle, point, flags, counter (big-endian),
    /// `rpId` length and `rpId`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + 32 + 65 + 1 + 4 + 1 + self.rp_id.len());
        out.push(PASSKEY_VERSION);
        out.push(ALG_ES256);
        out.extend_from_slice(&self.handle);
        out.extend_from_slice(&self.public_key);
        out.push(if self.backup_eligible {
            FLAG_BACKUP_ELIGIBLE
        } else {
            0
        });
        out.extend_from_slice(&self.sign_count.to_be_bytes());
        // `rp_id` is at most MAX_RP_ID bytes once decoded; an encoder handed a longer one writes
        // bytes `decode` refuses.
        out.push(u8::try_from(self.rp_id.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(self.rp_id.as_bytes());
        out
    }

    /// Decode canonical bytes, or `None` if they are not exactly one well-formed key.
    pub fn decode(b: &[u8]) -> Option<PasskeyKey> {
        let (&version, b) = b.split_first()?;
        let (&alg, b) = b.split_first()?;
        if version != PASSKEY_VERSION || alg != ALG_ES256 {
            return None;
        }
        let (handle, b) = b.split_first_chunk::<32>()?;
        let (public_key, b) = b.split_first_chunk::<65>()?;
        if public_key[0] != 0x04 {
            return None;
        }
        let (&flags, b) = b.split_first()?;
        if flags & !FLAG_BACKUP_ELIGIBLE != 0 {
            return None;
        }
        let (count, b) = b.split_first_chunk::<4>()?;
        let (&len, b) = b.split_first()?;
        if b.len() != usize::from(len) || !valid_rp_id(b) {
            return None;
        }
        let rp_id = String::from(core::str::from_utf8(b).ok()?);
        Some(PasskeyKey {
            handle: *handle,
            public_key: *public_key,
            rp_id,
            backup_eligible: flags & FLAG_BACKUP_ELIGIBLE != 0,
            sign_count: u32::from_be_bytes(*count),
        })
    }
}

/// Whether `b` is a lower-case DNS name of 1 to 253 bytes: labels of `a-z`, `0-9` and `-`,
/// separated by single dots. `localhost` is one label, and allowed.
pub fn valid_rp_id(b: &[u8]) -> bool {
    if b.is_empty() || b.len() > MAX_RP_ID {
        return false;
    }
    b.split(|&c| c == b'.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.first() != Some(&b'-')
            && label.last() != Some(&b'-')
            && label
                .iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn key() -> PasskeyKey {
        let mut public_key = [7u8; 65];
        public_key[0] = 0x04;
        PasskeyKey {
            handle: [3; 32],
            public_key,
            rp_id: "api.bloque.app".into(),
            backup_eligible: true,
            sign_count: 9,
        }
    }

    #[test]
    fn dec_19_a_passkey_key_round_trips_and_nothing_else_decodes() {
        let k = key();
        let b = k.encode();
        assert!(PasskeyKey::decode(&b) == Some(k.clone()));
        assert!(Scheme::WebAuthnEs256.well_formed(&b));
        // Every truncation, and one byte too many, is refused.
        for n in 0..b.len() {
            assert!(PasskeyKey::decode(&b[..n]).is_none(), "truncated at {n}");
        }
        let mut longer = b.clone();
        longer.push(b'x');
        assert!(PasskeyKey::decode(&longer).is_none());
        // A compressed point, an unknown flag, version or algorithm.
        for (at, v) in [(0, 2u8), (1, 2), (34, 0x02), (99, 0b10)] {
            let mut bad = b.clone();
            bad[at] = v;
            assert!(PasskeyKey::decode(&bad).is_none(), "byte {at}");
        }
    }

    #[test]
    fn dec_19_rp_ids_are_lower_case_dns_names() {
        for ok in ["api.bloque.app", "localhost", "a-b.example", "x"] {
            assert!(valid_rp_id(ok.as_bytes()), "{ok}");
        }
        for bad in [
            "",
            "API.bloque.app",
            "api..bloque.app",
            ".app",
            "app.",
            "-a.app",
            "a_b.app",
            "https://api.bloque.app",
        ] {
            assert!(!valid_rp_id(bad.as_bytes()), "{bad}");
        }
        assert!(!valid_rp_id(&vec![b'a'; 254]));
    }

    #[test]
    fn dec_19_scheme_tags_and_names_are_one_to_one() {
        for s in Scheme::ALL {
            assert_eq!(Scheme::from_tag(s.tag()), Some(s));
            assert_eq!(Scheme::parse(s.name()), Some(s));
        }
        assert_eq!(Scheme::from_tag(0), None);
        assert!(Scheme::ApiKey.well_formed(&[0; 32]));
        assert!(!Scheme::ApiKey.well_formed(&[0; 31]));
        assert!(Scheme::Key.well_formed(&[1]));
    }
}
