// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Transport authentication (`NFR-7`, `REQ-AU-8`, `DEP-7`).
//!
//! Every route is deny-by-default: a request without a credential ingress recognises is refused
//! with `ERR-CredentialInvalid` before anything else is read. Ingress checks only that the caller
//! is who it claims to be. Whether the credential is still in the identity's set, and what
//! authority it carries, the core checks on every command, so a revocation takes effect at once
//! (`REQ-AU-3`).
//!
//! A caller presents a bearer token. Ingress keeps only the SHA-256 of each token, never the
//! token: the configuration holds digests, and a token is hashed as soon as it is read from the
//! header and then dropped. [`Token`] has no `Debug` or `Display`, so it cannot be formatted
//! into a log line (`REQ-AU-8`). Holder and agent credentials pass only when the deployment's
//! `DEP-7` switch allows them; service principals always may.
//!
//! **Credential schemes** (`DEC-19`, `F-028`). Besides configured tokens, a bearer may be:
//!
//! - a **session** a passkey sign-in issued (`POST /v1/auth/passkey`): a short-lived token
//!   derived per sign-in, MAC'd under the deployment's authentication key, so any replica checks
//!   it (each sign-in moves the passkey's sign count in ingress's store);
//! - an **API key**: it names its credential, whose verifier (the secret's hash) ingress reads from
//!   the core ([`crate::boundary::Core::credential`]) and compares in constant time. That lookup is
//!   asynchronous, so [`Authenticator::presented`] hands it back as [`Presented::ApiKey`] and the
//!   surface finishes it. What the core answered is reused for a short TTL, bounded by `NFR-20`
//!   ([`crate::credentials::VerifierCache`], `T-031-13`): a revoked API key stops authenticating
//!   within it, and the core still checks the credential on every command.
//!
//! The kind of a session or an API key comes from its identity: `service` if a configured token
//! names that identity as a service principal, `agent` for an agent, `holder` otherwise.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use authn::token::{self, AuthnKey, Kind, PresentedApiKey};
use authn::webauthn::RelyingParty;
use axum::http::HeaderMap;
use ledger_core::Refusal;
use ledger_core::account::Time;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::boundary::{CallerBody, CredentialBody};

/// What kind of principal a credential belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A service principal: an integrating party, or the operator.
    Service,
    /// A holder of accounts, calling directly.
    Holder,
    /// An agent (`REQ-AG-1`).
    Agent,
}

/// A presented token. No `Debug`, no `Display`: it cannot reach a log.
pub struct Token(Vec<u8>);

impl Token {
    /// A token's bytes.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Token {
        Token(bytes.into())
    }

    /// Its SHA-256.
    pub fn digest(&self) -> [u8; 32] {
        Sha256::digest(&self.0).into()
    }
}

/// One configured credential: the digest of its token and who presents it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenEntry {
    /// SHA-256 of the token, hex. The token itself is never configured.
    pub token_sha256: String,
    /// The identity, `did:`-shaped.
    pub principal: String,
    /// The credential's identifier, `did:`-shaped.
    pub credential: String,
    /// What kind of principal.
    pub kind: CredentialKind,
}

/// The bearer token of a request's headers, or `ERR-CredentialInvalid`.
fn bearer(headers: &HeaderMap) -> Result<&str, Refusal> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .ok_or(Refusal::CredentialInvalid)
}

/// How a caller proved who it is (amendment 0037 `[PENDING AMENDMENT 0037]`, "How the door
/// decides", `T-040-11`). This, and nothing else (not the kind of principal, not the route, not a
/// credential's scheme), decides whether a command needs an approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proof {
    /// A session a passkey sign-in issued: it stands for a sign-in with a device-bound credential
    /// and signs nothing itself. Needs an approval.
    Session,
    /// An issued secret (an API key), presented as it is: a bearer secret. Exempt.
    IssuedSecret,
    /// A token an operator configured, presented as it is: a bearer secret. Exempt.
    ConfiguredToken,
}

impl Proof {
    /// Its name, for the report-only log and counts. Never a credential.
    pub fn name(self) -> &'static str {
        match self {
            Proof::Session => "session",
            Proof::IssuedSecret => "issued_secret",
            Proof::ConfiguredToken => "configured_token",
        }
    }
}

/// An authenticated caller: who it is, and the credential it used. Never the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authenticated {
    /// The identity, `did:`-shaped.
    pub principal: String,
    /// The credential, `did:`-shaped.
    pub credential: String,
    /// What kind of principal.
    pub kind: CredentialKind,
    /// How it proved who it is.
    pub proof: Proof,
}

impl Authenticated {
    /// The caller as the core receives it, acting for `on_behalf_of` if given.
    pub fn caller(&self, on_behalf_of: Option<String>) -> CallerBody {
        CallerBody {
            principal: self.principal.clone(),
            credential: self.credential.clone(),
            on_behalf_of,
        }
    }
}

impl CredentialKind {
    fn of(k: Kind) -> CredentialKind {
        match k {
            Kind::Service => CredentialKind::Service,
            Kind::Holder => CredentialKind::Holder,
            Kind::Agent => CredentialKind::Agent,
        }
    }

    fn token(self) -> Kind {
        match self {
            CredentialKind::Service => Kind::Service,
            CredentialKind::Holder => Kind::Holder,
            CredentialKind::Agent => Kind::Agent,
        }
    }
}

/// A deployment's passkeys (`F-028`, `DEC-19`): the key that MACs its challenges and sessions,
/// and the relying parties it accepts. Which challenges were used, and each passkey's sign count,
/// live in ingress's store, shared by every replica.
pub struct Passkeys {
    key: AuthnKey,
    parties: Vec<RelyingParty>,
    require_user_verification: bool,
    challenge_ttl: Time,
    session_ttl: Time,
    approvals: crate::approval::Approvals,
}

impl Passkeys {
    /// Passkeys under `key`, accepting `parties`.
    pub fn new(
        key: AuthnKey,
        parties: Vec<RelyingParty>,
        require_user_verification: bool,
        challenge_ttl: Time,
        session_ttl: Time,
    ) -> Passkeys {
        Passkeys {
            key,
            parties,
            require_user_verification,
            challenge_ttl,
            session_ttl,
            approvals: crate::approval::Approvals::off(),
        }
    }

    /// With per-command approvals as configured (`T-040-07`): off by default.
    pub fn with_approvals(mut self, approvals: crate::approval::Approvals) -> Passkeys {
        self.approvals = approvals;
        self
    }

    /// The per-command approval setting (amendment 0037).
    pub fn approvals(&self) -> &crate::approval::Approvals {
        &self.approvals
    }

    /// The key.
    pub fn key(&self) -> &AuthnKey {
        &self.key
    }

    /// The relying parties.
    pub fn parties(&self) -> &[RelyingParty] {
        &self.parties
    }

    /// The relying party of `rp_id`, if the deployment accepts it.
    pub fn party(&self, rp_id: &str) -> Option<&RelyingParty> {
        self.parties.iter().find(|p| p.rp_id == rp_id)
    }

    /// Whether UV is required.
    pub fn require_user_verification(&self) -> bool {
        self.require_user_verification
    }

    /// How long a challenge lives.
    pub fn challenge_ttl(&self) -> Time {
        self.challenge_ttl
    }

    /// How long a session lives.
    pub fn session_ttl(&self) -> Time {
        self.session_ttl
    }
}

/// What a request presented, as far as ingress can tell without the core.
pub enum Presented {
    /// A configured token or a session: the caller is known.
    Known(Authenticated),
    /// An API key: the core holds its verifier ([`Authenticator::finish_api_key`]).
    ApiKey(PresentedApiKey),
}

#[derive(Default)]
struct Inner {
    by_digest: HashMap<[u8; 32], Authenticated>,
    holders_allowed: bool,
    services: HashSet<String>,
    passkeys: Option<Arc<Passkeys>>,
}

/// The authenticator: token digests to principals. Clones share it; entries may be added while
/// running.
#[derive(Clone, Default)]
pub struct Authenticator {
    inner: Arc<RwLock<Inner>>,
    verifiers: Arc<crate::credentials::VerifierCache>,
}

impl Authenticator {
    /// An authenticator that knows no one: every request is refused.
    pub fn new() -> Authenticator {
        Authenticator::default()
    }

    /// Allow or forbid holder and agent credentials (the `DEP-7` switch).
    pub fn allow_holders(&self, allowed: bool) {
        self.write().holders_allowed = allowed;
    }

    /// Accept passkeys and sessions under `passkeys` (`F-028`).
    pub fn set_passkeys(&self, passkeys: Passkeys) {
        self.write().passkeys = Some(Arc::new(passkeys));
    }

    /// What the core last answered for API keys' credentials (`T-031-13`).
    pub fn verifiers(&self) -> &crate::credentials::VerifierCache {
        &self.verifiers
    }

    /// The deployment's passkeys, if it accepts any.
    pub fn passkeys(&self) -> Option<Arc<Passkeys>> {
        self.read().passkeys.clone()
    }

    /// The kind a session or an API key of `identity`, whose identity kind is `kind`, carries.
    pub fn kind_of(&self, identity: &str, kind: &str) -> CredentialKind {
        if self.read().services.contains(identity) {
            CredentialKind::Service
        } else if kind == "agent" {
            CredentialKind::Agent
        } else {
            CredentialKind::Holder
        }
    }

    /// Refuse a holder or an agent while the `DEP-7` switch is off.
    pub fn gate(&self, who: Authenticated) -> Result<Authenticated, Refusal> {
        if who.kind != CredentialKind::Service && !self.read().holders_allowed {
            return Err(Refusal::CredentialInvalid);
        }
        Ok(who)
    }

    /// Finish an API key with the credential the core holds for it: the scheme must be `api-key`
    /// and the hashes must match, in constant time. Then the `DEP-7` switch.
    pub fn finish_api_key(
        &self,
        key: &PresentedApiKey,
        found: Option<&CredentialBody>,
    ) -> Result<Authenticated, Refusal> {
        let c = found.ok_or(Refusal::CredentialInvalid)?;
        let verifier = hex::decode(&c.verifier).map_err(|_| Refusal::CredentialInvalid)?;
        if c.scheme != ledger_core::principal::Scheme::ApiKey.name() || !key.matches(&verifier) {
            return Err(Refusal::CredentialInvalid);
        }
        self.gate(Authenticated {
            principal: c.identity.clone(),
            credential: key.credential.to_did(),
            kind: self.kind_of(&c.identity, &c.kind),
            proof: Proof::IssuedSecret,
        })
    }

    /// Issue a session for a passkey that just signed in (`F-028`).
    pub fn session(
        &self,
        passkeys: &Passkeys,
        who: &Authenticated,
        now: Time,
    ) -> Result<(token::SessionToken, Time), Refusal> {
        let expires_at = now.saturating_add(passkeys.session_ttl());
        let s = token::Session {
            principal: crate::command::identity_id(&who.principal)
                .map_err(|_| Refusal::CredentialInvalid)?,
            credential: crate::command::credential(&who.credential)?,
            kind: who.kind.token(),
            expires_at,
        };
        Ok((token::issue_session(passkeys.key(), &s), expires_at))
    }

    /// Add a configured entry.
    pub fn add(&self, e: &TokenEntry) -> Result<(), String> {
        let digest: [u8; 32] = hex::decode(&e.token_sha256)
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| "token_sha256 must be 32 bytes of hex".to_owned())?;
        crate::command::identity_id(&e.principal).map_err(|_| "principal is not an identity")?;
        crate::command::credential(&e.credential).map_err(|_| "credential is not a credential")?;
        if e.kind == CredentialKind::Service {
            self.write().services.insert(e.principal.clone());
        }
        self.write().by_digest.insert(
            digest,
            Authenticated {
                principal: e.principal.clone(),
                credential: e.credential.clone(),
                kind: e.kind,
                proof: Proof::ConfiguredToken,
            },
        );
        Ok(())
    }

    /// Add a token directly (tests and tools): only its digest is kept.
    pub fn add_token(
        &self,
        token: &Token,
        principal: &str,
        credential: &str,
        kind: CredentialKind,
    ) -> Result<(), String> {
        self.add(&TokenEntry {
            token_sha256: hex::encode(token.digest()),
            principal: principal.to_owned(),
            credential: credential.to_owned(),
            kind,
        })
    }

    /// Authenticate a request's headers with a configured token only, or refuse with
    /// `ERR-CredentialInvalid`. Sessions and API keys need [`Authenticator::presented`].
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Authenticated, Refusal> {
        let raw = bearer(headers)?;
        if raw.starts_with(token::API_KEY_PREFIX) || raw.starts_with(token::SESSION_PREFIX) {
            return Err(Refusal::CredentialInvalid);
        }
        self.configured(raw)
    }

    fn configured(&self, raw: &str) -> Result<Authenticated, Refusal> {
        let token = Token::new(raw.as_bytes());
        let who = self
            .read()
            .by_digest
            .get(&token.digest())
            .cloned()
            .ok_or(Refusal::CredentialInvalid)?;
        self.gate(who)
    }

    /// What a request's headers present at `now`: a configured token, a session (`F-028`), or an
    /// API key for the surface to finish. Anything else is `ERR-CredentialInvalid`.
    pub fn presented(&self, headers: &HeaderMap, now: Time) -> Result<Presented, Refusal> {
        let raw = bearer(headers)?;
        if raw.starts_with(token::API_KEY_PREFIX) {
            return token::parse_api_key(raw)
                .map(Presented::ApiKey)
                .ok_or(Refusal::CredentialInvalid);
        }
        if raw.starts_with(token::SESSION_PREFIX) {
            let passkeys = self.passkeys().ok_or(Refusal::CredentialInvalid)?;
            let s =
                token::check_session(passkeys.key(), raw, now).ok_or(Refusal::CredentialInvalid)?;
            return self
                .gate(Authenticated {
                    principal: s.principal.to_did(),
                    credential: s.credential.to_did(),
                    kind: CredentialKind::of(s.kind),
                    proof: Proof::Session,
                })
                .map(Presented::Known);
        }
        self.configured(raw).map(Presented::Known)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::id::{CredentialId, IdentityId, OperationId};
    use ledger_kernel::posting::OperationRef;

    fn ids() -> (String, String) {
        let op = OperationId::from_ref(OperationRef([3; 16]));
        (
            IdentityId::mint(&op, 0).to_did(),
            CredentialId::mint(&op, 1).to_did(),
        )
    }

    fn bearer(t: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {t}").parse().unwrap(),
        );
        h
    }

    #[test]
    fn nfr_7_unknown_or_missing_credentials_are_refused() {
        let a = Authenticator::new();
        let (p, c) = ids();
        a.add_token(&Token::new("s3cret"), &p, &c, CredentialKind::Service)
            .unwrap();
        assert_eq!(
            a.authenticate(&HeaderMap::new()),
            Err(Refusal::CredentialInvalid)
        );
        assert_eq!(
            a.authenticate(&bearer("wrong")),
            Err(Refusal::CredentialInvalid)
        );
        assert_eq!(a.authenticate(&bearer("s3cret")).unwrap().principal, p);
    }

    #[test]
    fn dep_7_holder_credentials_pass_only_when_switched_on() {
        let a = Authenticator::new();
        let (p, c) = ids();
        a.add_token(&Token::new("holder"), &p, &c, CredentialKind::Holder)
            .unwrap();
        assert_eq!(
            a.authenticate(&bearer("holder")),
            Err(Refusal::CredentialInvalid)
        );
        a.allow_holders(true);
        assert!(a.authenticate(&bearer("holder")).is_ok());
    }
}
