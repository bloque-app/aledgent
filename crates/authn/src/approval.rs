// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Per-command approval (amendment 0037 `[PENDING AMENDMENT 0037]`, `F-040` `T-040-02`): the
//! frozen byte formats. Version 1 of each; a later version changes the version byte and so every
//! digest, instead of reading as this one.
//!
//! | Format | What |
//! |---|---|
//! | F1 | [`command_digest`]: SHA-256 of the framed tag, version, deployment, environment, client key, initiator, acted-for flag (and identity), payload digest and [`canonical_text`] |
//! | F2 | [`issue_approval_challenge`] / [`check_approval_challenge`]: 121 bytes, purpose `0x03`, sealed with the deployment and the environment |
//! | F3 | [`assertion_digest`] and [`record_message`]: what Feature B (`F-041`) stores; frozen here so B cannot change it |
//! | F4 | [`key_message`] and [`key_signed_value`]: a `key` credential's approval |
//!
//! A *field* is four bytes of big-endian length, then its bytes ([`frame`]). The F2 seal frames
//! with eight-byte lengths, as every MAC of [`crate::token::AuthnKey`] does. Nothing here formats a
//! secret; the challenge's seal is compared in constant time.

use ledger_core::account::Time;
use ledger_core::id::IdentityId;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

use crate::token::{AuthnKey, target_digest};

/// The version byte of every format here.
pub const VERSION: u8 = 0x01;
/// F2's purpose byte: approve. (`0x01` is sign-in, `0x02` registration.)
pub const PURPOSE_APPROVE: u8 = 0x03;
/// F2's length.
pub const APPROVAL_CHALLENGE_LEN: usize = 16 + 8 + 1 + 32 + 32 + 32;
/// The largest integer the canonical text writes as a number: above it, a string (2^53 − 1).
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

const COMMAND_TAG: &[u8] = b"aledgent/approval/command";
const PAYLOAD_TAG: &[u8] = b"aledgent/approval/payload";
const ASSERTION_TAG: &[u8] = b"aledgent/approval/assertion";
const RECORD_TAG: &[u8] = b"aledgent/approval/record";
const KEY_TAG: &[u8] = b"aledgent/approval/key";
const CHALLENGE_DOMAIN: &str = "aledgent/approval-challenge/v1";

/// The framing of every format: each field as four bytes of big-endian length, then its bytes.
pub fn frame(fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(fields.iter().map(|f| 4 + f.len()).sum());
    for f in fields {
        out.extend_from_slice(&u32::try_from(f.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(f);
    }
    out
}

fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

// ------------------------------------------------------------------------------------- F1

/// The canonical text of a command body (F1): JSON with no whitespace, object members sorted by
/// their names as bytes, only the escapes the amendment names (every character outside U+0020 to
/// U+007E, U+007F included, as a lower-case `\uxxxx`, with surrogate pairs), non-negative integers
/// in plain decimal and above 2^53 − 1 as a decimal string, and `null` members left out. Built from
/// the parsed value, never from a caller's bytes, and independent of the order a Rust type
/// declares its members or a map keeps them. `None` for a value no command holds (a negative or
/// fractional number, or `null` inside a list): such a command is not approvable.
pub fn canonical_text(v: &Value) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    write_value(v, &mut out)?;
    Some(out)
}

fn write_value(v: &Value, out: &mut Vec<u8>) -> Option<()> {
    match v {
        Value::Null => return None,
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => {
            let n = n.as_u64()?;
            if n > MAX_SAFE_INTEGER {
                out.push(b'"');
                out.extend_from_slice(n.to_string().as_bytes());
                out.push(b'"');
            } else {
                out.extend_from_slice(n.to_string().as_bytes());
            }
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut members: Vec<(&String, &Value)> =
                map.iter().filter(|(_, v)| !v.is_null()).collect();
            members.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push(b'{');
            for (i, (k, v)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(k, out);
                out.push(b':');
                write_value(v, out)?;
            }
            out.push(b'}');
        }
    }
    Some(())
}

fn write_string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\r' => out.extend_from_slice(b"\\r"),
            ' '..='~' => out.push(c as u8),
            _ => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.extend_from_slice(format!("\\u{u:04x}").as_bytes());
                }
            }
        }
    }
    out.push(b'"');
}

/// F1's payload digest of a device-bound credential's registration: SHA-256 of the framed tag,
/// version, client data bytes and attestation bytes.
pub fn payload_digest(client_data_json: &[u8], attestation_object: &[u8]) -> [u8; 32] {
    sha256(&frame(&[
        PAYLOAD_TAG,
        &[VERSION],
        client_data_json,
        attestation_object,
    ]))
}

/// What F1 digests, apart from the canonical text.
#[derive(Clone, Copy, Debug)]
pub struct CommandContext<'a> {
    /// The book's identity: the genesis configuration's `deployment`.
    pub deployment: &'a [u8; 16],
    /// Ingress's `environment` setting.
    pub environment: &'a str,
    /// The client key as ingress holds it: the caller's, or the one derived (`REQ-OP-11`).
    pub client_key: &'a str,
    /// The initiating principal, `did:`-shaped.
    pub initiator: &'a str,
    /// The identity it acts for, `did:`-shaped, if any.
    pub on_behalf_of: Option<&'a str>,
    /// The payload digest, for a registration of a device-bound credential.
    pub payload_digest: Option<&'a [u8; 32]>,
}

/// The preimage of F1: nine framed fields, ten when the command acts for another identity.
pub fn command_preimage(cx: &CommandContext<'_>, text: &[u8]) -> Vec<u8> {
    let mut fields: Vec<&[u8]> = vec![
        COMMAND_TAG,
        &[VERSION],
        cx.deployment,
        cx.environment.as_bytes(),
        cx.client_key.as_bytes(),
        cx.initiator.as_bytes(),
    ];
    match cx.on_behalf_of {
        None => fields.push(&[0x00]),
        Some(obo) => {
            fields.push(&[0x01]);
            fields.push(obo.as_bytes());
        }
    }
    fields.push(cx.payload_digest.map_or(&[][..], |d| &d[..]));
    fields.push(text);
    frame(&fields)
}

/// F1: the command digest.
pub fn command_digest(cx: &CommandContext<'_>, text: &[u8]) -> [u8; 32] {
    sha256(&command_preimage(cx, text))
}

// ------------------------------------------------------------------------------------- F2

/// Issue F2 for `identity` and `command_digest`, valid until `expires_at` (ingress's clock plus the
/// challenge lifetime). `nonce` comes from the operating system.
pub fn issue_approval_challenge(
    key: &AuthnKey,
    expires_at: Time,
    nonce: [u8; 16],
    identity: &IdentityId,
    command_digest: &[u8; 32],
    deployment: &[u8; 16],
    environment: &str,
) -> [u8; APPROVAL_CHALLENGE_LEN] {
    let exp = expires_at.to_be_bytes();
    let id = target_digest(identity);
    let seal = seal(
        key,
        &nonce,
        &exp,
        &id,
        command_digest,
        deployment,
        environment,
    );
    let mut out = [0u8; APPROVAL_CHALLENGE_LEN];
    out[..16].copy_from_slice(&nonce);
    out[16..24].copy_from_slice(&exp);
    out[24] = PURPOSE_APPROVE;
    out[25..57].copy_from_slice(&id);
    out[57..89].copy_from_slice(command_digest);
    out[89..].copy_from_slice(&seal);
    out
}

fn seal(
    key: &AuthnKey,
    nonce: &[u8; 16],
    exp: &[u8; 8],
    identity_digest: &[u8; 32],
    command_digest: &[u8; 32],
    deployment: &[u8; 16],
    environment: &str,
) -> [u8; 32] {
    key.mac(
        CHALLENGE_DOMAIN,
        &[
            nonce,
            exp,
            &[PURPOSE_APPROVE],
            identity_digest,
            command_digest,
            deployment,
            environment.as_bytes(),
        ],
    )
}

/// An F2 challenge this deployment and environment issued, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalChallenge {
    /// Its one-time number.
    pub nonce: [u8; 16],
    /// When it expires.
    pub expires_at: Time,
    /// The digest of the identity it is for ([`target_digest`]).
    pub identity_digest: [u8; 32],
    /// The command digest it carries.
    pub command_digest: [u8; 32],
}

impl ApprovalChallenge {
    /// Whether it was issued for `identity` and `command_digest`, compared in constant time.
    pub fn is_for(&self, identity: &IdentityId, command_digest: &[u8; 32]) -> bool {
        let id = bool::from(self.identity_digest.ct_eq(&target_digest(identity)));
        let cmd = bool::from(self.command_digest.ct_eq(command_digest));
        id & cmd
    }
}

/// Check that `bytes` is F2 as this deployment and environment issued it, not expired at `now`
/// (ingress's clock): its length, purpose, seal (which covers the deployment and the environment)
/// and expiry. `None` for anything else, with no reason (`REQ-AU-8`).
pub fn check_approval_challenge(
    key: &AuthnKey,
    bytes: &[u8],
    now: Time,
    deployment: &[u8; 16],
    environment: &str,
) -> Option<ApprovalChallenge> {
    let b: &[u8; APPROVAL_CHALLENGE_LEN] = bytes.try_into().ok()?;
    let nonce: [u8; 16] = b[..16].try_into().ok()?;
    let exp: [u8; 8] = b[16..24].try_into().ok()?;
    let identity_digest: [u8; 32] = b[25..57].try_into().ok()?;
    let command_digest: [u8; 32] = b[57..89].try_into().ok()?;
    let expected = seal(
        key,
        &nonce,
        &exp,
        &identity_digest,
        &command_digest,
        deployment,
        environment,
    );
    let sealed = bool::from(b[89..].ct_eq(&expected));
    let expires_at = Time::from_be_bytes(exp);
    (b[24] == PURPOSE_APPROVE && sealed && now < expires_at).then_some(ApprovalChallenge {
        nonce,
        expires_at,
        identity_digest,
        command_digest,
    })
}

// ------------------------------------------------------------------------------------- F3

/// F3's assertion digest: SHA-256 of the framed tag, version, raw id, authenticator data, client
/// data JSON and signature, as the device returned them.
pub fn assertion_digest(
    raw_id: &[u8],
    authenticator_data: &[u8],
    client_data_json: &[u8],
    signature: &[u8],
) -> [u8; 32] {
    sha256(&frame(&[
        ASSERTION_TAG,
        &[VERSION],
        raw_id,
        authenticator_data,
        client_data_json,
        signature,
    ]))
}

/// F3's signed message: the framed tag, version, credential body, command digest, assertion
/// digest and key id. The bytes themselves are signed (by Feature B's scheme).
pub fn record_message(
    credential: &[u8; 16],
    command_digest: &[u8; 32],
    assertion_digest: &[u8; 32],
    key_id: u32,
) -> Vec<u8> {
    frame(&[
        RECORD_TAG,
        &[VERSION],
        credential,
        command_digest,
        assertion_digest,
        &key_id.to_be_bytes(),
    ])
}

// ------------------------------------------------------------------------------------- F4

/// F4's framed message: the tag, version, command digest and expiry (eight bytes, big-endian).
pub fn key_message(command_digest: &[u8; 32], expires_at: Time) -> Vec<u8> {
    frame(&[
        KEY_TAG,
        &[VERSION],
        command_digest,
        &expires_at.to_be_bytes(),
    ])
}

/// What a `key` credential signs for F4: the 32-byte SHA-256 of [`key_message`].
pub fn key_signed_value(command_digest: &[u8; 32], expires_at: Time) -> [u8; 32] {
    sha256(&key_message(command_digest, expires_at))
}

#[cfg(test)]
mod tests {
    //! The vector of amendment 0037, F5 (`AC-A10.9`, `AC-A10.15`). Ingress's own canonical output
    //! for the five command bodies is pinned in `services/ingress/tests/approval_vector.rs`; the
    //! second implementation is `services/ingress/tests/approval_vector.py`.
    use super::*;

    const DEPLOYMENT: [u8; 16] = [0xD9; 16];
    const ENVIRONMENT: &str = "prod-eu";
    const ME: &str = "did:aledgent:id:0H248H248H248H248H248H248H";

    fn hex32(s: &str) -> [u8; 32] {
        hex::decode(s).unwrap().try_into().unwrap()
    }

    fn me() -> IdentityId {
        IdentityId::parse(ME).unwrap()
    }

    const CASE1_TEXT: &str = r#"{"command":"transfer","legs":[{"amount":"2500","asset":"USD","from":"did:aledgent:acct:1K6CSK6CSK6CSK6CSK6CSK6CSK","to":"did:aledgent:acct:248H248H248H248H248H248H24"}]}"#;
    const CASE1_DIGEST: &str = "c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de";

    fn case1() -> CommandContext<'static> {
        CommandContext {
            deployment: &DEPLOYMENT,
            environment: ENVIRONMENT,
            client_key: "pay-2026-10-21-0001",
            initiator: ME,
            on_behalf_of: None,
            payload_digest: None,
        }
    }

    #[test]
    fn ac_a10_9_the_vector_identity_and_case_1_digest() {
        // The vector's identity: 16 bytes of 0x11 in the repository's did form.
        assert_eq!(me().to_did(), ME);
        assert_eq!(
            hex::encode(target_digest(&me())),
            "073c7ff1e2f11c45844b0a9df8966d57d7b4f4bcf5acdddb8655f35911445a57"
        );
        let pre = command_preimage(&case1(), CASE1_TEXT.as_bytes());
        assert_eq!(pre.len(), 316, "case 1: nine framed fields, 316 bytes");
        assert_eq!(
            hex::encode(command_digest(&case1(), CASE1_TEXT.as_bytes())),
            CASE1_DIGEST
        );
    }

    #[test]
    fn ac_a10_9_the_vector_payload_digest_of_case_5() {
        let attestation: Vec<u8> = (0x40..0x60).collect();
        assert_eq!(
            hex::encode(payload_digest(
                br#"{"type":"webauthn.create"}"#,
                &attestation
            )),
            "186a51ad1b6bbf474d2cbb0d92933e1d9beb63c8a25205f9b223f112fe407e50"
        );
    }

    fn authn_key() -> AuthnKey {
        AuthnKey::new(core::array::from_fn(|i| i as u8))
    }

    const CHALLENGE: &str = "55555555555555555555555555555555000001a3185e24c003073c7ff1e2f11c45844b0a9df8966d57d7b4f4bcf5acdddb8655f35911445a57c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de87dddea4e18379c680c22878caa522b8b607178f8ed04d50cd5acd6593b0e7fa";
    const EXPIRES: Time = 1_800_000_120_000;

    #[test]
    fn ac_a10_9_the_vector_challenge_and_its_checks() {
        let d1 = hex32(CASE1_DIGEST);
        let c = issue_approval_challenge(
            &authn_key(),
            EXPIRES,
            [0x55; 16],
            &me(),
            &d1,
            &DEPLOYMENT,
            ENVIRONMENT,
        );
        assert_eq!(hex::encode(c), CHALLENGE);
        let ok = check_approval_challenge(&authn_key(), &c, EXPIRES - 1, &DEPLOYMENT, ENVIRONMENT)
            .unwrap();
        assert!(ok.is_for(&me(), &d1));
        assert_eq!(ok.nonce, [0x55; 16]);
    }

    /// `AC-A10.15`, `AC-A10.4` at the format: each way a challenge can be wrong, alone.
    #[test]
    fn ac_a10_15_a_challenge_is_refused_for_each_fault_alone() {
        let d1 = hex32(CASE1_DIGEST);
        let c = hex::decode(CHALLENGE).unwrap();
        let check = |b: &[u8], now, dep: &[u8; 16], env| {
            check_approval_challenge(&authn_key(), b, now, dep, env)
        };
        assert!(check(&c, EXPIRES - 1, &DEPLOYMENT, ENVIRONMENT).is_some());
        // Expired, at and after its expiry, on ingress's clock.
        assert!(check(&c, EXPIRES, &DEPLOYMENT, ENVIRONMENT).is_none());
        // Another deployment sharing the authentication key; another environment.
        assert!(check(&c, EXPIRES - 1, &[0xD8; 16], ENVIRONMENT).is_none());
        assert!(check(&c, EXPIRES - 1, &DEPLOYMENT, "test").is_none());
        // Another key.
        assert!(
            check_approval_challenge(
                &AuthnKey::new([9; 32]),
                &c,
                EXPIRES - 1,
                &DEPLOYMENT,
                ENVIRONMENT
            )
            .is_none()
        );
        // Every length but 121.
        for len in [0, 57, 89, 120, 122] {
            let mut b = c.clone();
            b.resize(len, 0);
            assert!(check(&b, EXPIRES - 1, &DEPLOYMENT, ENVIRONMENT).is_none());
        }
        // A wrong purpose, a later expiry, another identity or command digest, a wrong seal:
        // each one byte changed under the same seal.
        for at in [24, 16, 30, 60, 100] {
            let mut b = c.clone();
            b[at] ^= 1;
            assert!(
                check(&b, EXPIRES - 1, &DEPLOYMENT, ENVIRONMENT).is_none(),
                "byte {at}"
            );
        }
        // A sign-in and a registration challenge are not approvals.
        let other = crate::token::issue_challenge(
            &authn_key(),
            crate::token::Purpose::Authenticate,
            EXPIRES,
            [1; 16],
        );
        assert!(check(&other, EXPIRES - 1, &DEPLOYMENT, ENVIRONMENT).is_none());
        // Issued for one identity and command, it is for no other.
        let ok = check(&c, EXPIRES - 1, &DEPLOYMENT, ENVIRONMENT).unwrap();
        let other_id = IdentityId::parse("did:aledgent:id:0J289144GJ289144GJ289144GJ").unwrap();
        assert!(!ok.is_for(&other_id, &d1));
        let mut d2 = d1;
        d2[0] ^= 1;
        assert!(!ok.is_for(&me(), &d2));
    }

    #[test]
    fn ac_a10_9_the_vector_assertion_digest_record_and_key_message() {
        let challenge = hex::decode(CHALLENGE).unwrap();
        let raw_id: Vec<u8> = (0x80..0x90).collect();
        let mut auth_data = sha256(b"example.com").to_vec();
        auth_data.push(0x05);
        auth_data.extend_from_slice(&7u32.to_be_bytes());
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let cdj = format!(
            r#"{{"type":"webauthn.get","challenge":"{b64}","origin":"https://auth.example.com"}}"#
        );
        let a = assertion_digest(&raw_id, &auth_data, cdj.as_bytes(), &[0xA5; 70]);
        assert_eq!(
            hex::encode(a),
            "b3839496408b93ebc48701d29ea062231557b19969add218999e24e922a1e9b9"
        );
        let d1 = hex32(CASE1_DIGEST);
        assert_eq!(
            hex::encode(record_message(&[0x66; 16], &d1, &a, 1)),
            "00000018616c656467656e742f617070726f76616c2f7265636f72640000000101000000106666666666666666666666666666666600000020c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de00000020b3839496408b93ebc48701d29ea062231557b19969add218999e24e922a1e9b90000000400000001"
        );
        assert_eq!(
            hex::encode(key_message(&d1, EXPIRES)),
            "00000015616c656467656e742f617070726f76616c2f6b6579000000010100000020c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de00000008000001a3185e24c0"
        );
        let v = key_signed_value(&d1, EXPIRES);
        assert_eq!(
            hex::encode(v),
            "246aa66842214fd544fd859f6744f1ad273c5946eca4ef47bc614525e2764773"
        );
        // The vector's key signature verifies with the library (F4), and over nothing else.
        let pk = hex::decode("908999f7ad86c97588fc3c44e74cbeecc89df62fddd29a5b763464e28d7d5467")
            .unwrap();
        let sig = ledger_core::custody::Signature {
            r: hex32("7c107ed2840904ea12ce0be6d4d774a14c00b91c21f71dc96c1de2b087a33228"),
            s: hex32("b01981c1f9ace19f02b50e4606c2740b863e2aea66372465e433d25b188eeb08"),
        };
        assert!(ledger_core::custody::verifies(&pk, &v, &sig));
        assert!(!ledger_core::custody::verifies(
            &pk,
            &key_signed_value(&d1, EXPIRES + 1),
            &sig
        ));
    }

    /// `T-040-02`: the canonical text's rules, each alone.
    #[test]
    fn ac_a10_9_the_canonical_text_rules() {
        let t = |v: Value| String::from_utf8(canonical_text(&v).unwrap()).unwrap();
        // Sorted members, whatever the order given; null members left out; no whitespace.
        assert_eq!(
            t(serde_json::json!({"b": 1, "a": [true, "x"], "c": null})),
            r#"{"a":[true,"x"],"b":1}"#
        );
        // Escapes: quote, backslash, the five named controls, other controls, DEL, non-ASCII
        // and a character above U+FFFF.
        assert_eq!(
            t(Value::String(
                "\"\\\u{8}\t\n\u{c}\r\u{1}\u{7f}\u{e9}\u{1F600}~ ".into()
            )),
            r#""\"\\\b\t\n\f\r\u0001\u007f\u00e9\ud83d\ude00~ ""#
        );
        // Integers: plain up to 2^53 − 1, a string above.
        assert_eq!(t(serde_json::json!(MAX_SAFE_INTEGER)), "9007199254740991");
        assert_eq!(
            t(serde_json::json!(MAX_SAFE_INTEGER + 1)),
            r#""9007199254740992""#
        );
        // Not approvable: a negative or fractional number, a null in a list.
        assert!(canonical_text(&serde_json::json!(-1)).is_none());
        assert!(canonical_text(&serde_json::json!([null])).is_none());
    }

    /// `T-040-02`: a change of any one field changes the digest.
    #[test]
    fn ac_a10_3_one_field_changed_changes_the_digest() {
        let base = command_digest(&case1(), CASE1_TEXT.as_bytes());
        let other_text = CASE1_TEXT.replace("2500", "2501");
        let pd = [7u8; 32];
        let variants: Vec<[u8; 32]> = vec![
            command_digest(&case1(), other_text.as_bytes()),
            command_digest(
                &CommandContext {
                    client_key: "pay-2",
                    ..case1()
                },
                CASE1_TEXT.as_bytes(),
            ),
            command_digest(
                &CommandContext {
                    initiator: "did:aledgent:id:0J289144GJ289144GJ289144GJ",
                    ..case1()
                },
                CASE1_TEXT.as_bytes(),
            ),
            command_digest(
                &CommandContext {
                    on_behalf_of: Some(ME),
                    ..case1()
                },
                CASE1_TEXT.as_bytes(),
            ),
            command_digest(
                &CommandContext {
                    deployment: &[0xD8; 16],
                    ..case1()
                },
                CASE1_TEXT.as_bytes(),
            ),
            command_digest(
                &CommandContext {
                    environment: "test",
                    ..case1()
                },
                CASE1_TEXT.as_bytes(),
            ),
            command_digest(
                &CommandContext {
                    payload_digest: Some(&pd),
                    ..case1()
                },
                CASE1_TEXT.as_bytes(),
            ),
        ];
        for (i, v) in variants.iter().enumerate() {
            assert_ne!(*v, base, "variant {i}");
        }
    }
}
