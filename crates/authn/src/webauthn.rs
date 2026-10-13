// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Passkeys (`Scheme::WebAuthnEs256`, `DEC-19`): assertion verification, registration, and the
//! import of a Kreivo `Pass` device's key (`F-028` plan §2, §5).
//!
//! Every check returns a [`Rejected`] reason. Callers outside this crate show none of them: ingress
//! answers every rejection with the same `ERR-CredentialInvalid`, so a failed sign-in is no oracle
//! (`REQ-AU-8`). The reasons are for tests and for counting.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use blake2::Blake2b;
use blake2::digest::consts::U32;
use coset::cbor::value::Value;
use coset::{AsCborValue as _, CborSerializable as _, CoseKey, KeyType, Label, iana};
use ledger_core::scheme::{PasskeyKey, valid_rp_id};
use p256::ecdsa::signature::Verifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// Why an assertion or a registration was refused. Never shown to a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The credential id does not hash to the verifier's handle.
    Handle,
    /// The authenticator data is too short, or does not parse.
    AuthenticatorData,
    /// `rpIdHash` is not the hash of the verifier's, or an accepted, `rpId`.
    RpIdHash,
    /// The user-present flag is not set.
    UserPresent,
    /// The user-verified flag is not set, and the deployment requires it.
    UserVerified,
    /// Backup eligibility changed, or a backup state without eligibility.
    Backup,
    /// `clientDataJSON` does not parse.
    ClientData,
    /// `clientDataJSON.type` is not the ceremony's.
    Type,
    /// The challenge is not the one expected.
    Challenge,
    /// The origin is not one configured for the `rpId`, or the call was cross-origin.
    Origin,
    /// The signature does not parse, or does not verify.
    Signature,
    /// The sign count did not increase: a possible cloned authenticator.
    Counter,
    /// The attestation object does not parse, or carries no attested credential.
    Attestation,
    /// The credential's public key is not EC2 P-256 for ES256, or not on the curve.
    PublicKey,
    /// The credential id is empty or longer than 1023 bytes.
    CredentialId,
}

/// WebAuthn's flag bits in `authenticatorData`.
mod flag {
    pub const UP: u8 = 0b0000_0001;
    pub const UV: u8 = 0b0000_0100;
    pub const BE: u8 = 0b0000_1000;
    pub const BS: u8 = 0b0001_0000;
    pub const AT: u8 = 0b0100_0000;
    pub const ED: u8 = 0b1000_0000;
}

/// The longest credential id WebAuthn allows.
pub const MAX_CREDENTIAL_ID: usize = 1023;

/// The DER prefix of a P-256 `SubjectPublicKeyInfo` with an uncompressed point: what Kreivo `Pass`
/// stores (`DEREncodedPublicKey = [u8; 91]`) and what `getPublicKey()` returns in a browser.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// A passkey's handle: `blake2_256(rawId)`, Kreivo `Pass`'s `DeviceId`.
pub fn handle_of(raw_id: &[u8]) -> [u8; 32] {
    Blake2b::<U32>::digest(raw_id).into()
}

/// The SEC1 point of a 91-byte P-256 SPKI, checked to be on the curve: Kreivo `Pass`'s stored key.
pub fn sec1_from_spki(spki: &[u8]) -> Option<[u8; 65]> {
    let point = spki.strip_prefix(&P256_SPKI_PREFIX[..])?;
    let point: [u8; 65] = point.try_into().ok()?;
    on_curve(&point).then_some(point)
}

/// Whether a SEC1 uncompressed point is a P-256 public key.
pub fn on_curve(point: &[u8; 65]) -> bool {
    point[0] == 0x04 && VerifyingKey::from_sec1_bytes(point).is_ok()
}

/// One assertion, as the browser returned it (`navigator.credentials.get`).
#[derive(Clone, Copy)]
pub struct Assertion<'a> {
    /// The credential's raw id.
    pub raw_id: &'a [u8],
    /// `authenticatorData`.
    pub authenticator_data: &'a [u8],
    /// `clientDataJSON`, as bytes.
    pub client_data_json: &'a [u8],
    /// The DER ECDSA signature.
    pub signature: &'a [u8],
}

/// What the deployment accepts for a passkey's ceremonies (`F-028` plan §6).
#[derive(Clone, Copy)]
pub struct Policy<'a> {
    /// The origins accepted for the passkey's `rpId`, by exact match.
    pub origins: &'a [String],
    /// Whether the user-verified flag is required.
    pub require_user_verification: bool,
}

/// The fields of `clientDataJSON` aledgent reads. Others are ignored, as WebAuthn requires.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
    #[serde(default)]
    cross_origin: Option<bool>,
}

/// The client data's checks shared by both ceremonies: type, challenge, origin, same-origin.
/// Returns the origin.
fn client_data(json: &[u8], kind: &str, challenge: &[u8]) -> Result<String, Rejected> {
    let cd: ClientData = serde_json::from_slice(json).map_err(|_| Rejected::ClientData)?;
    if cd.kind != kind {
        return Err(Rejected::Type);
    }
    let presented = URL_SAFE_NO_PAD
        .decode(cd.challenge.as_bytes())
        .map_err(|_| Rejected::Challenge)?;
    if presented != challenge {
        return Err(Rejected::Challenge);
    }
    if cd.cross_origin == Some(true) {
        return Err(Rejected::Origin);
    }
    Ok(cd.origin)
}

/// The challenge a `clientDataJSON` carries, decoded, before anything is checked: ingress reads it
/// to find which challenge to check against.
pub fn presented_challenge(client_data_json: &[u8]) -> Option<Vec<u8>> {
    let cd: ClientData = serde_json::from_slice(client_data_json).ok()?;
    URL_SAFE_NO_PAD.decode(cd.challenge.as_bytes()).ok()
}

/// Verify an assertion against `key` (`F-028` plan §2, in its order). `challenge` is the exact
/// bytes the client data must carry; `stored_count` is the high-water mark of the authenticator's
/// counter. Returns the counter to store.
pub fn verify_assertion(
    key: &PasskeyKey,
    a: &Assertion<'_>,
    challenge: &[u8],
    policy: &Policy<'_>,
    stored_count: u32,
) -> Result<u32, Rejected> {
    // 1. The credential is the one the verifier names.
    if handle_of(a.raw_id) != key.handle {
        return Err(Rejected::Handle);
    }
    // 2. Authenticator data, bound to the verifier's relying party.
    let ad = a.authenticator_data;
    if ad.len() < 37 {
        return Err(Rejected::AuthenticatorData);
    }
    if ad[..32] != Sha256::digest(key.rp_id.as_bytes())[..] {
        return Err(Rejected::RpIdHash);
    }
    // 3. Flags.
    let flags = ad[32];
    if flags & flag::UP == 0 {
        return Err(Rejected::UserPresent);
    }
    if policy.require_user_verification && flags & flag::UV == 0 {
        return Err(Rejected::UserVerified);
    }
    let be = flags & flag::BE != 0;
    if be != key.backup_eligible || (!be && flags & flag::BS != 0) {
        return Err(Rejected::Backup);
    }
    // 4. Client data.
    let origin = client_data(a.client_data_json, "webauthn.get", challenge)?;
    if !policy.origins.contains(&origin) {
        return Err(Rejected::Origin);
    }
    // 5. The signature over authenticatorData ‖ SHA-256(clientDataJSON).
    let vk = VerifyingKey::from_sec1_bytes(&key.public_key).map_err(|_| Rejected::PublicKey)?;
    let sig = Signature::from_der(a.signature).map_err(|_| Rejected::Signature)?;
    let mut msg = Vec::with_capacity(ad.len() + 32);
    msg.extend_from_slice(ad);
    msg.extend_from_slice(&Sha256::digest(a.client_data_json));
    vk.verify(&msg, &sig).map_err(|_| Rejected::Signature)?;
    // 6. The sign count: zero and zero passes (synced passkeys); otherwise it must increase.
    let presented = u32::from_be_bytes([ad[33], ad[34], ad[35], ad[36]]);
    match (stored_count, presented) {
        (0, 0) => Ok(0),
        (s, p) if p > s => Ok(p),
        _ => Err(Rejected::Counter),
    }
}

/// One registration, as the browser returned it (`navigator.credentials.create`).
#[derive(Clone, Copy)]
pub struct Registration<'a> {
    /// `clientDataJSON`, as bytes.
    pub client_data_json: &'a [u8],
    /// `attestationObject`, CBOR.
    pub attestation_object: &'a [u8],
}

/// One relying party the deployment accepts: its `rpId` and the origins that may use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelyingParty {
    /// The `rpId`: a lower-case DNS name.
    pub rp_id: String,
    /// Its origins, matched exactly.
    pub origins: Vec<String>,
    /// Whether new passkeys may be registered under it. `false` keeps a relying party for the
    /// passkeys it already has: sign-in and import only.
    pub registrations: bool,
}

/// Verify a registration (`F-028` plan §2) and build the passkey's public material. The
/// attestation statement is not verified, whatever its format: aledgent trusts the holder's device,
/// not its manufacturer.
pub fn verify_registration(
    r: &Registration<'_>,
    challenge: &[u8],
    parties: &[RelyingParty],
    require_user_verification: bool,
) -> Result<PasskeyKey, Rejected> {
    let origin = client_data(r.client_data_json, "webauthn.create", challenge)?;
    let ad = auth_data_of(r.attestation_object)?;
    if ad.len() < 37 {
        return Err(Rejected::AuthenticatorData);
    }
    // The party is the one whose `rpId` the authenticator bound, among those that take
    // registrations from this origin: one origin may serve two parties (`api.bloque.app` serves
    // both `bloque.app` and the legacy `api.bloque.app`).
    let from_origin: Vec<&RelyingParty> = parties
        .iter()
        .filter(|p| p.registrations && p.origins.contains(&origin))
        .collect();
    if from_origin.is_empty() {
        return Err(Rejected::Origin);
    }
    let party = from_origin
        .into_iter()
        .find(|p| ad[..32] == Sha256::digest(p.rp_id.as_bytes())[..])
        .ok_or(Rejected::RpIdHash)?;
    let flags = ad[32];
    if flags & flag::UP == 0 {
        return Err(Rejected::UserPresent);
    }
    if require_user_verification && flags & flag::UV == 0 {
        return Err(Rejected::UserVerified);
    }
    let be = flags & flag::BE != 0;
    if !be && flags & flag::BS != 0 {
        return Err(Rejected::Backup);
    }
    if flags & flag::AT == 0 {
        return Err(Rejected::Attestation);
    }
    let sign_count = u32::from_be_bytes([ad[33], ad[34], ad[35], ad[36]]);
    // Attested credential data: AAGUID (16), id length (2), id, COSE key.
    let rest = ad.get(37 + 16..).ok_or(Rejected::AuthenticatorData)?;
    let (len, rest) = rest
        .split_first_chunk::<2>()
        .ok_or(Rejected::AuthenticatorData)?;
    let len = usize::from(u16::from_be_bytes(*len));
    if len == 0 || len > MAX_CREDENTIAL_ID {
        return Err(Rejected::CredentialId);
    }
    let raw_id = rest.get(..len).ok_or(Rejected::AuthenticatorData)?;
    let mut cose = &rest[len..];
    let value: Value =
        coset::cbor::de::from_reader(&mut cose).map_err(|_| Rejected::AuthenticatorData)?;
    // Only extensions may follow the key.
    if !cose.is_empty() && flags & flag::ED == 0 {
        return Err(Rejected::AuthenticatorData);
    }
    let key = CoseKey::from_cbor_value(value).map_err(|_| Rejected::PublicKey)?;
    let public_key = es256_point(&key).ok_or(Rejected::PublicKey)?;
    if !valid_rp_id(party.rp_id.as_bytes()) {
        return Err(Rejected::RpIdHash);
    }
    Ok(PasskeyKey {
        handle: handle_of(raw_id),
        public_key,
        rp_id: party.rp_id.clone(),
        backup_eligible: be,
        sign_count,
    })
}

/// `authData` from an attestation object: a CBOR map with `fmt`, `attStmt` and `authData`.
fn auth_data_of(attestation_object: &[u8]) -> Result<Vec<u8>, Rejected> {
    let v = Value::from_slice(attestation_object).map_err(|_| Rejected::Attestation)?;
    let Value::Map(entries) = v else {
        return Err(Rejected::Attestation);
    };
    entries
        .into_iter()
        .find_map(|(k, v)| match (k, v) {
            (Value::Text(k), Value::Bytes(b)) if k == "authData" => Some(b),
            _ => None,
        })
        .ok_or(Rejected::Attestation)
}

/// The SEC1 point of an EC2 P-256 COSE key for ES256, on the curve.
fn es256_point(key: &CoseKey) -> Option<[u8; 65]> {
    if key.kty != KeyType::Assigned(iana::KeyType::EC2) {
        return None;
    }
    // WebAuthn requires `alg` in a credential public key.
    if key.alg != Some(coset::Algorithm::Assigned(iana::Algorithm::ES256)) {
        return None;
    }
    let param = |l: i64| {
        key.params
            .iter()
            .find(|(k, _)| *k == Label::Int(l))
            .map(|(_, v)| v)
    };
    let crv = param(iana::Ec2KeyParameter::Crv as i64)?;
    if *crv != Value::from(iana::EllipticCurve::P_256 as i64) {
        return None;
    }
    let coord = |l| match param(l)? {
        Value::Bytes(b) if b.len() == 32 => Some(b.clone()),
        _ => None,
    };
    let x = coord(iana::Ec2KeyParameter::X as i64)?;
    let y = coord(iana::Ec2KeyParameter::Y as i64)?;
    let mut point = [0u8; 65];
    point[0] = 0x04;
    point[1..33].copy_from_slice(&x);
    point[33..].copy_from_slice(&y);
    on_curve(&point).then_some(point)
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    //! A software authenticator: the tests' passkey (feature `testing`; never in production).
    #![allow(missing_docs)]

    use super::*;
    use coset::CoseKeyBuilder;
    use p256::ecdsa::SigningKey;
    use p256::ecdsa::signature::Signer as _;

    /// A passkey on a software authenticator.
    pub struct Soft {
        pub sk: SigningKey,
        pub raw_id: Vec<u8>,
        pub rp_id: String,
        pub counter: u32,
        pub flags: u8,
    }

    impl Soft {
        pub fn new(seed: u8, rp_id: &str) -> Soft {
            Soft {
                sk: SigningKey::from_slice(&[seed; 32]).unwrap(),
                raw_id: vec![seed; 20],
                rp_id: rp_id.into(),
                counter: 0,
                flags: flag::UP | flag::UV | flag::BE | flag::BS,
            }
        }

        pub fn point(&self) -> [u8; 65] {
            self.sk
                .verifying_key()
                .to_encoded_point(false)
                .as_bytes()
                .try_into()
                .unwrap()
        }

        pub fn key(&self) -> PasskeyKey {
            PasskeyKey {
                handle: handle_of(&self.raw_id),
                public_key: self.point(),
                rp_id: self.rp_id.clone(),
                backup_eligible: self.flags & flag::BE != 0,
                sign_count: 0,
            }
        }

        pub fn client_data(kind: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
            format!(
                r#"{{"type":"{kind}","challenge":"{}","origin":"{origin}","crossOrigin":false}}"#,
                URL_SAFE_NO_PAD.encode(challenge)
            )
            .into_bytes()
        }

        pub fn auth_data(&self, rp_id: &str, extra: &[u8]) -> Vec<u8> {
            let mut ad = Sha256::digest(rp_id.as_bytes()).to_vec();
            ad.push(self.flags);
            ad.extend_from_slice(&self.counter.to_be_bytes());
            ad.extend_from_slice(extra);
            ad
        }

        /// An assertion over `challenge` from `origin`: (authenticatorData, clientDataJSON,
        /// signature).
        pub fn assert(&self, challenge: &[u8], origin: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let ad = self.auth_data(&self.rp_id, &[]);
            let cd = Self::client_data("webauthn.get", challenge, origin);
            let sig = self.sign(&ad, &cd);
            (ad, cd, sig)
        }

        pub fn sign(&self, ad: &[u8], cd: &[u8]) -> Vec<u8> {
            let mut msg = ad.to_vec();
            msg.extend_from_slice(&Sha256::digest(cd));
            let sig: Signature = self.sk.sign(&msg);
            sig.to_der().as_bytes().to_vec()
        }

        pub fn cose_key(&self) -> Vec<u8> {
            let p = self.point();
            CoseKeyBuilder::new_ec2_pub_key(
                iana::EllipticCurve::P_256,
                p[1..33].to_vec(),
                p[33..].to_vec(),
            )
            .algorithm(iana::Algorithm::ES256)
            .build()
            .to_vec()
            .unwrap()
        }

        /// A registration over `challenge` from `origin`, with attestation format `none`.
        pub fn register(&self, challenge: &[u8], origin: &str) -> (Vec<u8>, Vec<u8>) {
            let mut extra = vec![0u8; 16];
            extra.extend_from_slice(&(self.raw_id.len() as u16).to_be_bytes());
            extra.extend_from_slice(&self.raw_id);
            extra.extend_from_slice(&self.cose_key());
            let mut s = self.clone_shallow();
            s.flags |= flag::AT;
            let ad = s.auth_data(&self.rp_id, &extra);
            let att = Value::Map(vec![
                (Value::Text("fmt".into()), Value::Text("none".into())),
                (Value::Text("attStmt".into()), Value::Map(vec![])),
                (Value::Text("authData".into()), Value::Bytes(ad)),
            ])
            .to_vec()
            .unwrap();
            (Self::client_data("webauthn.create", challenge, origin), att)
        }

        /// A copy, for a second ceremony from the same authenticator.
        pub fn clone_shallow(&self) -> Soft {
            Soft {
                sk: self.sk.clone(),
                raw_id: self.raw_id.clone(),
                rp_id: self.rp_id.clone(),
                counter: self.counter,
                flags: self.flags,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Soft;
    use super::*;

    const RP: &str = "api.bloque.app";
    const ORIGIN: &str = "https://api.bloque.app";

    fn origins() -> Vec<String> {
        vec![ORIGIN.to_owned()]
    }

    fn check(s: &Soft, parts: &(Vec<u8>, Vec<u8>, Vec<u8>), stored: u32) -> Result<u32, Rejected> {
        let o = origins();
        verify_assertion(
            &s.key(),
            &Assertion {
                raw_id: &s.raw_id,
                authenticator_data: &parts.0,
                client_data_json: &parts.1,
                signature: &parts.2,
            },
            b"challenge-bytes-0123",
            &Policy {
                origins: &o,
                require_user_verification: true,
            },
            stored,
        )
    }

    #[test]
    fn req_au_8_a_good_assertion_verifies() {
        let s = Soft::new(1, RP);
        let a = s.assert(b"challenge-bytes-0123", ORIGIN);
        assert_eq!(check(&s, &a, 0), Ok(0));
    }

    #[test]
    fn nfr_7_each_assertion_check_breaks_alone() {
        let s = Soft::new(2, RP);
        let good = s.assert(b"challenge-bytes-0123", ORIGIN);
        // The handle: another credential's raw id.
        let mut other = Soft::new(2, RP);
        other.raw_id = vec![9; 20];
        let o = origins();
        let r = verify_assertion(
            &s.key(),
            &Assertion {
                raw_id: &other.raw_id,
                authenticator_data: &good.0,
                client_data_json: &good.1,
                signature: &good.2,
            },
            b"challenge-bytes-0123",
            &Policy {
                origins: &o,
                require_user_verification: true,
            },
            0,
        );
        assert_eq!(r, Err(Rejected::Handle));
        // rpIdHash: a passkey of another relying party.
        let mut elsewhere = Soft::new(2, "evil.example");
        elsewhere.rp_id = "evil.example".into();
        let ad = elsewhere.auth_data("evil.example", &[]);
        let sig = s.sign(&ad, &good.1);
        assert_eq!(
            check(&s, &(ad, good.1.clone(), sig), 0),
            Err(Rejected::RpIdHash)
        );
        // Truncated authenticator data.
        assert_eq!(
            check(
                &s,
                &(good.0[..36].to_vec(), good.1.clone(), good.2.clone()),
                0
            ),
            Err(Rejected::AuthenticatorData)
        );
        // Flags: UP, UV, BE.
        for (clear, why) in [
            (flag::UP, Rejected::UserPresent),
            (flag::UV, Rejected::UserVerified),
            (flag::BE, Rejected::Backup),
        ] {
            let mut t = Soft::new(2, RP);
            t.flags &= !clear;
            let ad = t.auth_data(RP, &[]);
            let sig = s.sign(&ad, &good.1);
            assert_eq!(check(&s, &(ad, good.1.clone(), sig), 0), Err(why));
        }
        // The client data: type, challenge, origin, cross-origin.
        for (cd, why) in [
            (
                Soft::client_data("webauthn.create", b"challenge-bytes-0123", ORIGIN),
                Rejected::Type,
            ),
            (
                Soft::client_data("webauthn.get", b"another-challenge", ORIGIN),
                Rejected::Challenge,
            ),
            (
                Soft::client_data(
                    "webauthn.get",
                    b"challenge-bytes-0123",
                    "https://evil.example",
                ),
                Rejected::Origin,
            ),
            (b"not json".to_vec(), Rejected::ClientData),
        ] {
            let sig = s.sign(&good.0, &cd);
            assert_eq!(check(&s, &(good.0.clone(), cd, sig), 0), Err(why));
        }
        let cross = format!(
            r#"{{"type":"webauthn.get","challenge":"{}","origin":"{ORIGIN}","crossOrigin":true}}"#,
            URL_SAFE_NO_PAD.encode(b"challenge-bytes-0123")
        )
        .into_bytes();
        let sig = s.sign(&good.0, &cross);
        assert_eq!(
            check(&s, &(good.0.clone(), cross, sig), 0),
            Err(Rejected::Origin)
        );
        // The signature: another key's, and garbage.
        let other = Soft::new(3, RP);
        let sig = other.sign(&good.0, &good.1);
        assert_eq!(
            check(&s, &(good.0.clone(), good.1.clone(), sig), 0),
            Err(Rejected::Signature)
        );
        assert_eq!(
            check(&s, &(good.0.clone(), good.1.clone(), vec![0x30, 0]), 0),
            Err(Rejected::Signature)
        );
    }

    #[test]
    fn nfr_7_the_sign_count_must_increase_unless_it_stays_zero() {
        let mut s = Soft::new(4, RP);
        let zero = s.assert(b"challenge-bytes-0123", ORIGIN);
        assert_eq!(check(&s, &zero, 0), Ok(0));
        s.counter = 5;
        let five = s.assert(b"challenge-bytes-0123", ORIGIN);
        assert_eq!(check(&s, &five, 0), Ok(5));
        assert_eq!(check(&s, &five, 4), Ok(5));
        assert_eq!(check(&s, &five, 5), Err(Rejected::Counter));
        assert_eq!(check(&s, &zero, 5), Err(Rejected::Counter));
    }

    #[test]
    fn req_au_20_a_registration_builds_the_verifier_an_assertion_then_meets() {
        let s = Soft::new(5, RP);
        let (cd, att) = s.register(b"register-challenge-0", ORIGIN);
        let parties = [
            RelyingParty {
                rp_id: RP.into(),
                origins: origins(),
                registrations: true,
            },
            // The same origin serving a parent `rpId`: the attestation's hash picks the party.
            RelyingParty {
                rp_id: "bloque.app".into(),
                origins: origins(),
                registrations: true,
            },
        ];
        let key = verify_registration(
            &Registration {
                client_data_json: &cd,
                attestation_object: &att,
            },
            b"register-challenge-0",
            &parties,
            true,
        )
        .unwrap();
        assert!(key == s.key());
        // Wrong ceremony, wrong origin, wrong challenge, garbage.
        let (bad_cd, _) = s.register(b"register-challenge-0", "https://evil.example");
        for (cd, att, why) in [
            (bad_cd, att.clone(), Rejected::Origin),
            (
                Soft::client_data("webauthn.get", b"register-challenge-0", ORIGIN),
                att.clone(),
                Rejected::Type,
            ),
            (
                Soft::client_data("webauthn.create", b"other", ORIGIN),
                att.clone(),
                Rejected::Challenge,
            ),
            (cd.clone(), vec![0xa0], Rejected::Attestation),
        ] {
            assert_eq!(
                verify_registration(
                    &Registration {
                        client_data_json: &cd,
                        attestation_object: &att,
                    },
                    b"register-challenge-0",
                    &parties,
                    true,
                ),
                Err(why)
            );
        }
    }

    #[test]
    fn dec_19_a_relying_party_closed_to_registrations_takes_none() {
        let s = Soft::new(7, RP);
        let (cd, att) = s.register(b"register-challenge-0", ORIGIN);
        let legacy = [RelyingParty {
            rp_id: RP.into(),
            origins: origins(),
            registrations: false,
        }];
        assert_eq!(
            verify_registration(
                &Registration {
                    client_data_json: &cd,
                    attestation_object: &att,
                },
                b"register-challenge-0",
                &legacy,
                true,
            ),
            Err(Rejected::Origin)
        );
    }

    #[test]
    fn dec_19_a_kreivo_pass_spki_converts_to_the_same_point() {
        let s = Soft::new(6, RP);
        let mut spki = P256_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(&s.point());
        assert_eq!(spki.len(), 91);
        assert_eq!(sec1_from_spki(&spki), Some(s.point()));
        // Not a P-256 SPKI, a short one, a point off the curve.
        let mut wrong = spki.clone();
        wrong[10] ^= 1;
        assert_eq!(sec1_from_spki(&wrong), None);
        assert_eq!(sec1_from_spki(&spki[..90]), None);
        let mut off = spki.clone();
        off[90] ^= 1;
        assert_eq!(sec1_from_spki(&off), None);
    }

    #[test]
    fn dec_19_the_import_tools_vector_decodes() {
        // `tools/pass-import/src/convert.test.ts` writes these bytes for the same passkey.
        let v = hex::decode(concat!(
            "0101",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
            "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
            "01",
            "00000007",
            "0e6170692e626c6f7175652e617070",
        ))
        .unwrap();
        let k = PasskeyKey::decode(&v).unwrap();
        assert_eq!(k.handle, [0x11; 32]);
        assert!(on_curve(&k.public_key));
        assert_eq!(k.rp_id, "api.bloque.app");
        assert!(k.backup_eligible);
        assert_eq!(k.sign_count, 7);
        assert_eq!(k.encode(), v);
    }

    #[test]
    fn dec_19_the_handle_is_kreivo_pass_device_id() {
        // blake2_256(b"") as sp_core computes it: Blake2b with a 32-byte output, unkeyed.
        assert_eq!(
            hex::encode(handle_of(b"")),
            "0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8"
        );
    }
}
