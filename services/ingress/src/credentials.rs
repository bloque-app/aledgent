// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Credential schemes at ingress (`DEC-19`, `F-028`): passkey sign-in and challenges, API keys,
//! and the work a credential command needs before it can be logged.
//!
//! - **Authentication.** [`authenticate`] finishes what [`crate::auth::Authenticator::presented`]
//!   starts: an API key's verifier comes from the core, through a [`VerifierCache`] (`T-031-13`).
//! - **Challenges** (`POST /v1/auth/challenge`) and **sign-in** (`POST /v1/auth/passkey`): see
//!   `features/F-028-credential-schemes/plan.md` §2. Every failure of either is the same
//!   `ERR-CredentialInvalid`, with no detail, so neither is an oracle.
//! - **Before a command is logged** ([`prepare`]): a passkey's registration is verified and becomes
//!   its verifier; an API key's secret is drawn and only its hash is logged; a passkey's reclaim
//!   proof is verified and becomes the mark the core accepts for a passkey only. The received-log
//!   never holds an assertion, an attestation or a secret (`REQ-AU-8`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ledger_core::Refusal;
use ledger_core::custody::reclaim_challenge;
use ledger_core::id::{AccountId, IdentityId};
use ledger_core::principal::Scheme;
use ledger_core::scheme::PasskeyKey;

use authn::token::{self, ApiKeySecret, Purpose};
use authn::webauthn::{self, Assertion, Policy, Registration};
use axum::http::HeaderMap;

use crate::auth::{Authenticated, CredentialKind, Passkeys, Presented};
use crate::boundary::{Core, CredentialBody, Query, QueryReply};
use crate::service::Shared;
use crate::store::Store;
use crate::wire::{
    AssertionBody, ChallengePurposeBody, ChallengeReply, ChallengeRequest, CommandBody,
    PasskeyRegistrationBody, PasskeySignInBody, ReclaimCustodyBody, ReclaimCustodyCommand,
    RegisterCredentialBody, RegisterCredentialRequest, SessionReply,
};

/// How long an API key's verifier is reused before the core is read again, by default: half of
/// `NFR-20`'s bound (`T-031-13`).
pub const API_KEY_CACHE_TTL: Duration = Duration::from_millis(500);

/// The longest the cache may keep an answer: `NFR-20`'s bound, 1 s at p99 from a revocation's
/// commit (amendment 0012, `REQ-AU-3`).
pub const API_KEY_CACHE_MAX_TTL: Duration = Duration::from_secs(1);

/// The most credentials the cache holds. Past it, expired answers are dropped, and if none has
/// expired, all of them: a caller presenting made-up credentials cannot grow it.
const CACHE_ENTRIES: usize = 65_536;

/// What ingress last read from the core about each API key's credential (`T-031-13`, finding F4
/// of `F-032`): its public material, or that the core holds no working credential under that
/// identifier. Before, every API-key request read the core, and on the core Agent that read waited
/// for the invocation in flight (456 ms p50, up to 1.9 s p99).
///
/// **The bound on a revocation.** An answer is reused for at most the TTL ([`API_KEY_CACHE_TTL`]
/// by default, never more than [`API_KEY_CACHE_MAX_TTL`]) after it was read. A credential removed,
/// or whose identity stops being active, therefore stops authenticating within the TTL of the
/// commit that did it: the short lag the owner accepted for revocations (ruling split-principals,
/// 2026-10-03), inside `NFR-20`'s bound. What the credential may do is still checked by the core
/// on every command, at decision (`REQ-AU-3`). The cache holds no secret: a verifier is the hash
/// of an API key's secret, and the secret presented is compared with it on every request.
#[derive(Debug)]
pub struct VerifierCache {
    ttl_ms: AtomicU64,
    entries: Mutex<HashMap<String, (Instant, Option<CredentialBody>)>>,
}

impl Default for VerifierCache {
    fn default() -> Self {
        VerifierCache {
            ttl_ms: AtomicU64::new(API_KEY_CACHE_TTL.as_millis() as u64),
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl VerifierCache {
    /// Set the TTL. Zero reads the core on every request; above `NFR-20`'s bound is refused.
    pub fn set_ttl(&self, ttl: Duration) -> Result<(), String> {
        if ttl > API_KEY_CACHE_MAX_TTL {
            return Err(format!(
                "the API-key cache's TTL is {} ms; NFR-20 bounds it to {} ms",
                ttl.as_millis(),
                API_KEY_CACHE_MAX_TTL.as_millis()
            ));
        }
        self.ttl_ms.store(
            u64::try_from(ttl.as_millis()).unwrap_or(0),
            Ordering::Relaxed,
        );
        self.lock().clear();
        Ok(())
    }

    /// The TTL.
    pub fn ttl(&self) -> Duration {
        Duration::from_millis(self.ttl_ms.load(Ordering::Relaxed))
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, Option<CredentialBody>)>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The answer read for `credential` within the TTL, if any: `Some(None)` is "no working
    /// credential".
    pub fn get(&self, credential: &str) -> Option<Option<CredentialBody>> {
        let ttl = self.ttl();
        if ttl.is_zero() {
            return None;
        }
        let entries = self.lock();
        let (at, found) = entries.get(credential)?;
        (at.elapsed() < ttl).then(|| found.clone())
    }

    /// Keep what the core answered for `credential`, read at `read_at` (before the read was
    /// sent, so that the TTL runs from a point no later than the state it saw).
    pub fn put(&self, credential: &str, read_at: Instant, found: Option<CredentialBody>) {
        let ttl = self.ttl();
        if ttl.is_zero() {
            return;
        }
        let mut entries = self.lock();
        if entries.len() >= CACHE_ENTRIES && !entries.contains_key(credential) {
            entries.retain(|_, (at, _)| at.elapsed() < ttl);
            if entries.len() >= CACHE_ENTRIES {
                entries.clear();
            }
        }
        entries.insert(credential.to_owned(), (read_at, found));
    }
}

/// Work a credential command needs before it is logged.
pub enum Pending {
    /// Verify a passkey registration; its verifier fills the command.
    RegisterPasskey(PasskeyRegistrationBody),
    /// Draw an API key's secret; its hash fills the command.
    IssueApiKey,
    /// Verify a passkey's reclaim proof; the mark replaces it.
    ReclaimAssertion(AssertionBody),
}

impl Pending {
    /// What the caller sent for this work, digested: part of the request's identity, so a repeat
    /// of the same request matches and another proof under the same client key does not.
    pub fn fingerprint(&self) -> Vec<u8> {
        use sha2::Digest as _;
        let (tag, bytes) = match self {
            Pending::RegisterPasskey(p) => (1u8, serde_json::to_vec(p).unwrap_or_default()),
            Pending::IssueApiKey => (2, Vec::new()),
            Pending::ReclaimAssertion(a) => (3, serde_json::to_vec(a).unwrap_or_default()),
        };
        let mut out = vec![0, tag];
        out.extend_from_slice(&sha2::Sha256::digest(bytes));
        out
    }
}

/// `POST /v1/credentials`'s body into the command the core receives, and what is left to do.
pub fn build_register(
    r: RegisterCredentialRequest,
) -> Result<(CommandBody, Option<Pending>), Refusal> {
    let scheme = match r.scheme.as_deref() {
        None => Scheme::Key,
        Some(name) => Scheme::parse(name).ok_or(Refusal::CredentialInvalid)?,
    };
    let body = |verifier: String| {
        CommandBody::RegisterCredential(RegisterCredentialBody {
            marker: r.marker,
            scope: r.scope.clone(),
            verifier,
            scheme: (scheme != Scheme::Key).then(|| scheme.name().to_owned()),
        })
    };
    Ok(match (scheme, r.verifier.clone(), r.passkey.clone()) {
        (Scheme::Key, Some(v), None) => (body(v), None),
        (Scheme::WebAuthnEs256, None, Some(p)) => {
            (body(String::new()), Some(Pending::RegisterPasskey(p)))
        }
        (Scheme::ApiKey, None, None) => (body(String::new()), Some(Pending::IssueApiKey)),
        _ => return Err(Refusal::CredentialInvalid),
    })
}

/// `POST /v1/accounts/{account}/custody/reclaim`'s body into the command, and what is left to do.
pub fn build_reclaim(
    account: String,
    b: ReclaimCustodyBody,
) -> Result<(CommandBody, Option<Pending>), Refusal> {
    let cmd = |signature| {
        CommandBody::ReclaimCustody(ReclaimCustodyCommand {
            account: account.clone(),
            proof_credential: b.proof_credential.clone(),
            signature,
            passkey_checked: false,
            acknowledged: b.acknowledged.clone(),
        })
    };
    Ok(match (b.signature.clone(), b.assertion.clone()) {
        (Some(sig), None) => (cmd(Some(sig)), None),
        (None, Some(a)) => (cmd(None), Some(Pending::ReclaimAssertion(a))),
        _ => return Err(Refusal::CredentialInvalid),
    })
}

fn b64(s: &str) -> Result<Vec<u8>, Refusal> {
    URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|_| Refusal::CredentialInvalid)
}

fn passkeys<S: Store, C: Core>(shared: &Shared<S, C>) -> Result<std::sync::Arc<Passkeys>, Refusal> {
    shared.auth.passkeys().ok_or(Refusal::CredentialInvalid)
}

/// A working passkey the core holds: its body and its public material.
async fn passkey_of<S: Store, C: Core>(
    shared: &Shared<S, C>,
    credential: &str,
) -> Result<(CredentialBody, PasskeyKey), Refusal> {
    let found = shared
        .core
        .credential(credential)
        .await
        .map_err(|_| Refusal::DecisionUnavailable)?
        .ok_or(Refusal::CredentialInvalid)?;
    if found.scheme != Scheme::WebAuthnEs256.name() {
        return Err(Refusal::CredentialInvalid);
    }
    let key = hex::decode(&found.verifier)
        .ok()
        .and_then(|b| PasskeyKey::decode(&b))
        .ok_or(Refusal::CredentialInvalid)?;
    Ok((found, key))
}

/// Verify `a` for `credential` against `challenge`, and move its sign count in ingress's store
/// (`F-028`): durable, per sign-in, and only ever raised. A count another sign-in already reached
/// is refused, on any replica.
async fn check_assertion<S: Store, C: Core>(
    shared: &Shared<S, C>,
    pk: &Passkeys,
    credential: &str,
    key: &PasskeyKey,
    a: &AssertionBody,
    challenge: &[u8],
) -> Result<(), Refusal> {
    let stored = shared
        .store
        .sign_count(credential)
        .await
        .map_err(|_| Refusal::DecisionUnavailable)?
        .unwrap_or(0)
        .max(key.sign_count);
    let party = pk.party(&key.rp_id).ok_or(Refusal::CredentialInvalid)?;
    let raw_id = b64(&a.raw_id)?;
    let ad = b64(&a.authenticator_data)?;
    let cdj = b64(&a.client_data_json)?;
    let sig = b64(&a.signature)?;
    let count = webauthn::verify_assertion(
        key,
        &Assertion {
            raw_id: &raw_id,
            authenticator_data: &ad,
            client_data_json: &cdj,
            signature: &sig,
        },
        challenge,
        &Policy {
            origins: &party.origins,
            require_user_verification: pk.require_user_verification(),
        },
        stored,
    )
    .map_err(|_| Refusal::CredentialInvalid)?;
    // Zero and zero: a synced passkey, which never counts. Anything else is written first.
    if count > 0
        && !shared
            .store
            .raise_sign_count(credential, count)
            .await
            .map_err(|_| Refusal::DecisionUnavailable)?
    {
        return Err(Refusal::CredentialInvalid);
    }
    Ok(())
}

/// Check a challenge issued for `purpose` that `client_data_json` carries, and use it: once, on
/// any replica (ingress's store). A challenge bound to an identity (amendment 0027) is accepted only
/// for a ceremony into `into`'s set, and is not used up by one that is refused for that.
async fn fresh_challenge<S: Store, C: Core>(
    shared: &Shared<S, C>,
    pk: &Passkeys,
    client_data_json: &[u8],
    purpose: Purpose,
    into: Option<&IdentityId>,
    now: u64,
) -> Result<Vec<u8>, Refusal> {
    let ch = webauthn::presented_challenge(client_data_json).ok_or(Refusal::CredentialInvalid)?;
    let issued =
        token::check_challenge(pk.key(), &ch, purpose, now).ok_or(Refusal::CredentialInvalid)?;
    let admitted = match (issued.target, into) {
        (None, _) => true,
        (Some(_), Some(id)) => issued.admits(id),
        (Some(_), None) => false,
    };
    if !admitted {
        return Err(Refusal::CredentialInvalid);
    }
    let first = shared
        .store
        .use_challenge(issued.nonce, issued.expires_at, now)
        .await
        .map_err(|_| Refusal::DecisionUnavailable)?;
    if !first {
        return Err(Refusal::CredentialInvalid);
    }
    Ok(ch)
}

/// An import's passkeys must name a relying party this deployment accepts (`REQ-MG-7`: the
/// deployment declares which credentials it accepts; `DEC-19`: for passkeys, its configured
/// `rpId`s). Every other command passes. A deployment with no passkeys imports none.
pub fn check_import<S: Store, C: Core>(
    shared: &Shared<S, C>,
    body: &CommandBody,
) -> Result<(), Refusal> {
    let CommandBody::Operator(crate::wire::OperatorCommand::ImportCredentials {
        credentials, ..
    }) = body
    else {
        return Ok(());
    };
    for c in credentials {
        if c.scheme != Scheme::WebAuthnEs256.name() {
            continue;
        }
        let pk = passkeys(shared)?;
        let key = hex::decode(&c.verifier)
            .ok()
            .and_then(|b| PasskeyKey::decode(&b))
            .ok_or(Refusal::CredentialInvalid)?;
        if pk.party(&key.rp_id).is_none() || !webauthn::on_curve(&key.public_key) {
            return Err(Refusal::CredentialInvalid);
        }
    }
    Ok(())
}

/// Authenticate a request (`NFR-7`): a configured token, a session, or an API key whose verifier
/// the core holds, read through the [`VerifierCache`]. A core that cannot be read is
/// `ERR-DecisionUnavailable`: safe to retry, and nothing is cached.
pub async fn authenticate<S: Store, C: Core>(
    shared: &Shared<S, C>,
    headers: &HeaderMap,
) -> Result<Authenticated, Refusal> {
    match shared.auth.presented(headers, shared.clock.now())? {
        Presented::Known(a) => Ok(a),
        Presented::ApiKey(k) => {
            let id = k.credential.to_did();
            let cache = shared.auth.verifiers();
            let found = match cache.get(&id) {
                Some(found) => found,
                None => {
                    let read_at = Instant::now();
                    let found = shared
                        .core
                        .credential(&id)
                        .await
                        .map_err(|_| Refusal::DecisionUnavailable)?;
                    cache.put(&id, read_at, found.clone());
                    found
                }
            };
            shared.auth.finish_api_key(&k, found.as_ref())
        }
    }
}

/// `POST /v1/auth/challenge`.
pub fn challenge<S: Store, C: Core>(
    shared: &Shared<S, C>,
    bytes: &[u8],
) -> Result<ChallengeReply, Refusal> {
    let r: ChallengeRequest =
        serde_json::from_slice(bytes).map_err(|_| Refusal::CredentialInvalid)?;
    let pk = passkeys(shared)?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| Refusal::Internal)?;
    let expires_at = shared.clock.now().saturating_add(pk.challenge_ttl());
    let purpose = match r.purpose {
        ChallengePurposeBody::Authenticate => Purpose::Authenticate,
        ChallengePurposeBody::Register => Purpose::Register,
    };
    // Only a registration is bound to the set it joins (amendment 0027).
    let c = match (purpose, r.identity.as_deref()) {
        (_, None) => token::issue_challenge(pk.key(), purpose, expires_at, nonce).to_vec(),
        (Purpose::Register, Some(id)) => {
            let id = IdentityId::parse(id).map_err(|_| Refusal::CredentialInvalid)?;
            token::issue_bound_challenge(pk.key(), purpose, &id, expires_at, nonce).to_vec()
        }
        (Purpose::Authenticate, Some(_)) => return Err(Refusal::CredentialInvalid),
    };
    Ok(ChallengeReply {
        challenge: URL_SAFE_NO_PAD.encode(c),
        expires_at,
    })
}

/// `POST /v1/auth/passkey`: sign in, and get a session.
pub async fn sign_in<S: Store, C: Core>(
    shared: &Shared<S, C>,
    bytes: &[u8],
) -> Result<(SessionReply, Authenticated), Refusal> {
    let b: PasskeySignInBody =
        serde_json::from_slice(bytes).map_err(|_| Refusal::CredentialInvalid)?;
    let pk = passkeys(shared)?;
    let now = shared.clock.now();
    let cdj = b64(&b.assertion.client_data_json)?;
    let ch = fresh_challenge(shared, &pk, &cdj, Purpose::Authenticate, None, now).await?;
    let (found, key) = passkey_of(shared, &b.credential).await?;
    check_assertion(shared, &pk, &b.credential, &key, &b.assertion, &ch).await?;
    let who = shared.auth.gate(Authenticated {
        principal: found.identity.clone(),
        credential: b.credential.clone(),
        kind: shared.auth.kind_of(&found.identity, &found.kind),
    })?;
    let (session, expires_at) = shared.auth.session(&pk, &who, now)?;
    Ok((
        SessionReply {
            session: session.expose().to_owned(),
            expires_at,
            principal: who.principal.clone(),
            credential: who.credential.clone(),
        },
        who,
    ))
}

/// The identity whose set a `register_credential` from `who`, acting for `on_behalf_of`, joins: as
/// the core resolves it, the named identity for a principal that is not an agent (amendment 0025
/// R2), and the caller's own otherwise.
fn register_target(
    who: &Authenticated,
    on_behalf_of: Option<&IdentityId>,
) -> Result<IdentityId, Refusal> {
    match on_behalf_of {
        Some(t) if who.kind != CredentialKind::Agent => Ok(*t),
        _ => IdentityId::parse(&who.principal).map_err(|_| Refusal::CredentialInvalid),
    }
}

/// Do what a credential command needs before it is logged. Returns an API key's secret, when one
/// was drawn, for the reply that decides the command.
pub async fn prepare<S: Store, C: Core>(
    shared: &Shared<S, C>,
    who: &Authenticated,
    on_behalf_of: Option<&IdentityId>,
    body: &mut CommandBody,
    pending: Pending,
) -> Result<Option<ApiKeySecret>, Refusal> {
    let now = shared.clock.now();
    match (pending, body) {
        (Pending::RegisterPasskey(p), CommandBody::RegisterCredential(b)) => {
            let pk = passkeys(shared)?;
            let cdj = b64(&p.client_data_json)?;
            let att = b64(&p.attestation_object)?;
            let into = register_target(who, on_behalf_of)?;
            let ch =
                fresh_challenge(shared, &pk, &cdj, Purpose::Register, Some(&into), now).await?;
            let key = webauthn::verify_registration(
                &Registration {
                    client_data_json: &cdj,
                    attestation_object: &att,
                },
                &ch,
                pk.parties(),
                pk.require_user_verification(),
            )
            .map_err(|_| Refusal::CredentialInvalid)?;
            b.verifier = hex::encode(key.encode());
            Ok(None)
        }
        (Pending::IssueApiKey, CommandBody::RegisterCredential(b)) => {
            let secret = ApiKeySecret::generate().map_err(|_| Refusal::Internal)?;
            b.verifier = hex::encode(secret.verifier());
            Ok(Some(secret))
        }
        (Pending::ReclaimAssertion(a), CommandBody::ReclaimCustody(c)) => {
            let pk = passkeys(shared)?;
            let account = AccountId::parse(&c.account).map_err(|_| Refusal::UnknownAccount)?;
            // The epoch the proof binds, as the holder sees it (`REQ-AU-13`).
            let epoch = match shared
                .core
                .query(
                    &who.caller(None),
                    &Query::Custody {
                        account: c.account.clone(),
                    },
                    now,
                )
                .await
                .map_err(|_| Refusal::DecisionUnavailable)?
            {
                QueryReply::Custody(v) => v.epoch,
                QueryReply::Refused { error } => return Err(error),
                _ => return Err(Refusal::CredentialInvalid),
            };
            let (_, key) = passkey_of(shared, &c.proof_credential).await?;
            check_assertion(
                shared,
                &pk,
                &c.proof_credential,
                &key,
                &a,
                &reclaim_challenge(&account, epoch),
            )
            .await?;
            c.signature = None;
            c.passkey_checked = true;
            Ok(None)
        }
        _ => Err(Refusal::CredentialInvalid),
    }
}
