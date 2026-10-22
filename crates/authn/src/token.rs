// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What ingress issues (`F-028` plan §2, §3): challenges, session tokens and API keys.
//!
//! Challenges and sessions are **stateless** and MAC'd under the deployment's authentication key,
//! so any ingress replica checks what another issued. The key is a secret from the environment, with
//! no default (`REQ-AU-8`). None of these types has `Debug` or `Display`: a token cannot be
//! formatted into a log line.
//!
//! | What | Form |
//! |---|---|
//! | Challenge | `nonce(16) ‖ expires_at(8) ‖ purpose(1) ‖ HMAC(32)`, raw bytes (the client base64url-encodes them into `clientDataJSON`) |
//! | Bound challenge | `nonce(16) ‖ expires_at(8) ‖ purpose(1) ‖ SHA-256(identity)(32) ‖ HMAC(32)`: a `register` challenge for one identity's set (amendment 0027) |
//! | Session | `aledgent_st1.` ‖ base64url(`principal ‖ credential ‖ kind ‖ expires_at`) ‖ `.` ‖ base64url(HMAC) |
//! | API key | `aledgent_ak1.` ‖ the credential's identifier body ‖ `.` ‖ base64url(secret, 32 bytes) |

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac as _};
use ledger_core::account::Time;
use ledger_core::id::{CredentialId, DID_PREFIX, IdentityId};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

type HmacSha256 = Hmac<Sha256>;

/// The prefix of a session token.
pub const SESSION_PREFIX: &str = "aledgent_st1.";
/// The prefix of an API key.
pub const API_KEY_PREFIX: &str = "aledgent_ak1.";
/// A challenge's length.
pub const CHALLENGE_LEN: usize = 16 + 8 + 1 + 32;
/// A bound challenge's length: a challenge with the digest of the identity it is for.
pub const BOUND_CHALLENGE_LEN: usize = CHALLENGE_LEN + 32;

const CHALLENGE_DOMAIN: &str = "aledgent/webauthn-challenge/v1";

/// The deployment's authentication key: 32 bytes, from the environment. No `Debug`.
#[derive(Clone)]
pub struct AuthnKey([u8; 32]);

impl AuthnKey {
    /// From its bytes.
    pub fn new(bytes: [u8; 32]) -> AuthnKey {
        AuthnKey(bytes)
    }

    /// From 64 hex digits, or an error that names no part of the input.
    pub fn from_hex(s: &str) -> Result<AuthnKey, &'static str> {
        hex::decode(s.trim())
            .ok()
            .and_then(|v| <[u8; 32]>::try_from(v).ok())
            .map(AuthnKey)
            .ok_or("the authentication key must be 32 bytes of hex")
    }

    pub(crate) fn mac(&self, domain: &str, parts: &[&[u8]]) -> [u8; 32] {
        // HMAC accepts a key of any length.
        let mut m = <HmacSha256 as hmac::Mac>::new_from_slice(&self.0)
            .unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
        m.update(domain.as_bytes());
        for p in parts {
            m.update(&(p.len() as u64).to_be_bytes());
            m.update(p);
        }
        m.finalize().into_bytes().into()
    }
}

/// What a challenge is for: one ceremony, never the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// `navigator.credentials.get`: a sign-in.
    Authenticate,
    /// `navigator.credentials.create`: a registration.
    Register,
}

impl Purpose {
    const fn code(self) -> u8 {
        match self {
            Purpose::Authenticate => 1,
            Purpose::Register => 2,
        }
    }
}

/// Issue a challenge for `purpose`, valid until `expires_at`. `nonce` comes from the OS.
pub fn issue_challenge(
    key: &AuthnKey,
    purpose: Purpose,
    expires_at: Time,
    nonce: [u8; 16],
) -> [u8; CHALLENGE_LEN] {
    let exp = expires_at.to_be_bytes();
    let mac = key.mac(CHALLENGE_DOMAIN, &[&nonce, &exp, &[purpose.code()]]);
    let mut out = [0u8; CHALLENGE_LEN];
    out[..16].copy_from_slice(&nonce);
    out[16..24].copy_from_slice(&exp);
    out[24] = purpose.code();
    out[25..].copy_from_slice(&mac);
    out
}

/// The digest a bound challenge carries for `identity`: SHA-256 of its `did:` form.
pub fn target_digest(identity: &IdentityId) -> [u8; 32] {
    Sha256::digest(identity.to_did().as_bytes()).into()
}

/// Issue a challenge for `purpose`, valid until `expires_at`, bound to `target` (amendment 0027): a
/// registration over it is accepted only into `target`'s set. The digest is MAC'd with the rest, so
/// it cannot be removed or replaced.
pub fn issue_bound_challenge(
    key: &AuthnKey,
    purpose: Purpose,
    target: &IdentityId,
    expires_at: Time,
    nonce: [u8; 16],
) -> [u8; BOUND_CHALLENGE_LEN] {
    let exp = expires_at.to_be_bytes();
    let t = target_digest(target);
    let mac = key.mac(CHALLENGE_DOMAIN, &[&nonce, &exp, &[purpose.code()], &t]);
    let mut out = [0u8; BOUND_CHALLENGE_LEN];
    out[..16].copy_from_slice(&nonce);
    out[16..24].copy_from_slice(&exp);
    out[24] = purpose.code();
    out[25..57].copy_from_slice(&t);
    out[57..].copy_from_slice(&mac);
    out
}

/// A challenge this deployment issued, checked: its nonce (for single use), its expiry, and the
/// digest of the identity it is bound to, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Issued {
    /// Its nonce.
    pub nonce: [u8; 16],
    /// When it expires.
    pub expires_at: Time,
    /// The digest of the identity it is bound to ([`target_digest`]); `None` for an unbound one.
    pub target: Option<[u8; 32]>,
}

impl Issued {
    /// Whether a ceremony over this challenge may land in `identity`'s set: always for an unbound
    /// challenge; for a bound one, only if it is `identity`'s.
    pub fn admits(&self, identity: &IdentityId) -> bool {
        self.target.is_none_or(|t| t == target_digest(identity))
    }
}

/// Check that `bytes` is a challenge this deployment issued for `purpose` and not expired at `now`,
/// bound or not.
pub fn check_challenge(
    key: &AuthnKey,
    bytes: &[u8],
    purpose: Purpose,
    now: Time,
) -> Option<Issued> {
    let (head, target, mac) = match bytes.len() {
        CHALLENGE_LEN => (&bytes[..25], None, &bytes[25..]),
        BOUND_CHALLENGE_LEN => (
            &bytes[..25],
            Some(<[u8; 32]>::try_from(&bytes[25..57]).ok()?),
            &bytes[57..],
        ),
        _ => return None,
    };
    let nonce: [u8; 16] = head[..16].try_into().ok()?;
    let exp: [u8; 8] = head[16..24].try_into().ok()?;
    let code = [purpose.code()];
    let expected = match &target {
        None => key.mac(CHALLENGE_DOMAIN, &[&nonce, &exp, &code]),
        Some(t) => key.mac(CHALLENGE_DOMAIN, &[&nonce, &exp, &code, t]),
    };
    let expires_at = Time::from_be_bytes(exp);
    let ok = head[24] == purpose.code() && bool::from(mac.ct_eq(&expected)) && now < expires_at;
    ok.then_some(Issued {
        nonce,
        expires_at,
        target,
    })
}

/// What kind of principal a session or an API key authenticates (`DEP-7`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A service principal.
    Service,
    /// A holder.
    Holder,
    /// An agent.
    Agent,
}

impl Kind {
    const fn code(self) -> u8 {
        match self {
            Kind::Service => 1,
            Kind::Holder => 2,
            Kind::Agent => 3,
        }
    }

    fn of(code: u8) -> Option<Kind> {
        [Kind::Service, Kind::Holder, Kind::Agent]
            .into_iter()
            .find(|k| k.code() == code)
    }
}

/// A session's claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
    /// The identity.
    pub principal: IdentityId,
    /// The passkey it signed in with.
    pub credential: CredentialId,
    /// What kind of principal.
    pub kind: Kind,
    /// When it expires.
    pub expires_at: Time,
}

fn session_payload(s: &Session) -> Vec<u8> {
    let p = s.principal.to_did();
    let c = s.credential.to_did();
    let mut out = Vec::with_capacity(2 + p.len() + c.len() + 9);
    out.push(u8::try_from(p.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(p.as_bytes());
    out.push(u8::try_from(c.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(c.as_bytes());
    out.push(s.kind.code());
    out.extend_from_slice(&s.expires_at.to_be_bytes());
    out
}

/// Issue a session token. It is a secret: return it to its holder and drop it.
pub fn issue_session(key: &AuthnKey, s: &Session) -> SessionToken {
    let payload = session_payload(s);
    let mac = key.mac("aledgent/session/v1", &[&payload]);
    SessionToken(format!(
        "{SESSION_PREFIX}{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac)
    ))
}

/// A session token. No `Debug`, no `Display`.
pub struct SessionToken(String);

impl SessionToken {
    /// The token, for the one reply that carries it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Check a presented session token at `now`. `None` for anything but an unexpired session this
/// deployment issued.
pub fn check_session(key: &AuthnKey, token: &str, now: Time) -> Option<Session> {
    let rest = token.strip_prefix(SESSION_PREFIX)?;
    let (payload, mac) = rest.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let mac = URL_SAFE_NO_PAD.decode(mac).ok()?;
    let expected = key.mac("aledgent/session/v1", &[&payload]);
    if !bool::from(mac.ct_eq(&expected)) {
        return None;
    }
    let (&pl, b) = payload.split_first()?;
    let (p, b) = b.split_at_checked(usize::from(pl))?;
    let (&cl, b) = b.split_first()?;
    let (c, b) = b.split_at_checked(usize::from(cl))?;
    let (&kind, b) = b.split_first()?;
    let exp: [u8; 8] = b.try_into().ok()?;
    let s = Session {
        principal: IdentityId::parse(core::str::from_utf8(p).ok()?).ok()?,
        credential: CredentialId::parse(core::str::from_utf8(c).ok()?).ok()?,
        kind: Kind::of(kind)?,
        expires_at: Time::from_be_bytes(exp),
    };
    (now < s.expires_at).then_some(s)
}

/// An API key's secret. No `Debug`, no `Display`.
pub struct ApiKeySecret([u8; 32]);

impl ApiKeySecret {
    /// A fresh secret from the OS.
    pub fn generate() -> Result<ApiKeySecret, getrandom::Error> {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b)?;
        Ok(ApiKeySecret(b))
    }

    /// The verifier the core keeps: the secret's SHA-256 (`Scheme::ApiKey`).
    pub fn verifier(&self) -> [u8; 32] {
        Sha256::digest(self.0).into()
    }

    /// The key as the caller presents it, once, for credential `credential`.
    pub fn key_for(&self, credential: &CredentialId) -> ApiKeyToken {
        let did = credential.to_did();
        let body = did.rsplit(':').next().unwrap_or_default();
        ApiKeyToken(format!(
            "{API_KEY_PREFIX}{body}.{}",
            URL_SAFE_NO_PAD.encode(self.0)
        ))
    }
}

/// An API key as presented. No `Debug`, no `Display`.
pub struct ApiKeyToken(String);

impl ApiKeyToken {
    /// The key, for the one reply that carries it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// A presented API key, parsed: the credential it names and its secret's SHA-256.
pub struct PresentedApiKey {
    /// The credential it names.
    pub credential: CredentialId,
    digest: [u8; 32],
}

impl PresentedApiKey {
    /// Whether it matches `verifier`, in constant time.
    pub fn matches(&self, verifier: &[u8]) -> bool {
        bool::from(self.digest.as_slice().ct_eq(verifier))
    }
}

/// Parse a presented API key. `None` unless it has the API key's form.
pub fn parse_api_key(token: &str) -> Option<PresentedApiKey> {
    let rest = token.strip_prefix(API_KEY_PREFIX)?;
    let (body, secret) = rest.split_once('.')?;
    let credential = CredentialId::parse(&format!("{DID_PREFIX}cred:{body}")).ok()?;
    let secret = URL_SAFE_NO_PAD.decode(secret).ok()?;
    let secret: [u8; 32] = secret.try_into().ok()?;
    Some(PresentedApiKey {
        credential,
        digest: Sha256::digest(secret).into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::id::OperationId;
    use ledger_kernel::posting::OperationRef;

    fn key() -> AuthnKey {
        AuthnKey::new([5; 32])
    }

    fn ids() -> (IdentityId, CredentialId) {
        let op = OperationId::from_ref(OperationRef([4; 16]));
        (IdentityId::mint(&op, 0), CredentialId::mint(&op, 1))
    }

    #[test]
    fn nfr_7_a_challenge_checks_only_for_its_purpose_key_and_time() {
        let c = issue_challenge(&key(), Purpose::Authenticate, 100, [1; 16]);
        assert_eq!(
            check_challenge(&key(), &c, Purpose::Authenticate, 99),
            Some(Issued {
                nonce: [1; 16],
                expires_at: 100,
                target: None,
            })
        );
        assert_eq!(
            check_challenge(&key(), &c, Purpose::Authenticate, 100),
            None
        );
        assert_eq!(check_challenge(&key(), &c, Purpose::Register, 99), None);
        assert_eq!(
            check_challenge(&AuthnKey::new([6; 32]), &c, Purpose::Authenticate, 99),
            None
        );
        let mut forged = c;
        forged[16..24].copy_from_slice(&1000u64.to_be_bytes());
        assert_eq!(
            check_challenge(&key(), &forged, Purpose::Authenticate, 99),
            None
        );
        assert_eq!(
            check_challenge(&key(), &c[..56], Purpose::Authenticate, 99),
            None
        );
    }

    #[test]
    fn amendment_0027_a_bound_challenge_admits_only_its_identity_and_cannot_be_unbound() {
        let op = OperationId::from_ref(OperationRef([4; 16]));
        let (a, b) = (IdentityId::mint(&op, 0), IdentityId::mint(&op, 2));
        let c = issue_bound_challenge(&key(), Purpose::Register, &a, 100, [2; 16]);
        let issued = check_challenge(&key(), &c, Purpose::Register, 99).unwrap();
        assert_eq!(issued.nonce, [2; 16]);
        assert_eq!(issued.target, Some(target_digest(&a)));
        assert!(issued.admits(&a));
        assert!(!issued.admits(&b));
        // The same checks as an unbound one: purpose, key, expiry.
        assert_eq!(check_challenge(&key(), &c, Purpose::Authenticate, 99), None);
        assert_eq!(check_challenge(&key(), &c, Purpose::Register, 100), None);
        assert_eq!(
            check_challenge(&AuthnKey::new([6; 32]), &c, Purpose::Register, 99),
            None
        );
        // Another identity's digest under the same MAC does not check.
        let mut swapped = c;
        swapped[25..57].copy_from_slice(&target_digest(&b));
        assert_eq!(
            check_challenge(&key(), &swapped, Purpose::Register, 99),
            None
        );
        // Nor does the challenge with its binding cut out.
        let mut cut = [0u8; CHALLENGE_LEN];
        cut[..25].copy_from_slice(&c[..25]);
        cut[25..].copy_from_slice(&c[57..]);
        assert_eq!(check_challenge(&key(), &cut, Purpose::Register, 99), None);
        // An unbound challenge admits any identity, as before.
        let u = issue_challenge(&key(), Purpose::Register, 100, [3; 16]);
        let issued = check_challenge(&key(), &u, Purpose::Register, 99).unwrap();
        assert_eq!(issued.target, None);
        assert!(issued.admits(&a) && issued.admits(&b));
    }

    #[test]
    fn req_au_3_a_session_checks_only_until_it_expires_and_only_unaltered() {
        let (p, c) = ids();
        let s = Session {
            principal: p,
            credential: c,
            kind: Kind::Holder,
            expires_at: 50,
        };
        let t = issue_session(&key(), &s);
        assert_eq!(check_session(&key(), t.expose(), 49), Some(s));
        assert_eq!(check_session(&key(), t.expose(), 50), None);
        assert_eq!(check_session(&AuthnKey::new([6; 32]), t.expose(), 49), None);
        // Another kind with the same MAC does not check.
        let (head, mac) = t.expose().rsplit_once('.').unwrap();
        let mut payload = URL_SAFE_NO_PAD
            .decode(head.strip_prefix(SESSION_PREFIX).unwrap())
            .unwrap();
        let at = payload.len() - 9;
        payload[at] = Kind::Service.code();
        let forged = format!("{SESSION_PREFIX}{}.{mac}", URL_SAFE_NO_PAD.encode(&payload));
        assert_eq!(check_session(&key(), &forged, 49), None);
        assert_eq!(check_session(&key(), "aledgent_st1.x.y", 49), None);
    }

    #[test]
    fn req_au_8_an_api_key_names_its_credential_and_matches_only_its_hash() {
        let (_, c) = ids();
        let s = ApiKeySecret::generate().unwrap();
        let v = s.verifier();
        let k = s.key_for(&c);
        assert!(k.expose().starts_with(API_KEY_PREFIX));
        let p = parse_api_key(k.expose()).unwrap();
        assert_eq!(p.credential, c);
        assert!(p.matches(&v));
        assert!(!p.matches(&[0; 32]));
        let other = ApiKeySecret::generate().unwrap();
        assert!(
            !parse_api_key(other.key_for(&c).expose())
                .unwrap()
                .matches(&v)
        );
        assert!(parse_api_key("aledgent_ak1.nope").is_none());
        assert!(parse_api_key("Bearer x").is_none());
    }
}
