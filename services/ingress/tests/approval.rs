// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Per-command approval at the front door (amendment 0037 `[PENDING AMENDMENT 0037]`, `F-040`
//! `T-040-03`, `T-040-05`, `T-040-06`, `T-040-07`, `T-040-11`), end to end through the surface
//! with a software authenticator: step (a) asks for the challenge, the device signs, step (b)
//! carries the approval in the `aledgent-approval` header.

use authn::token::AuthnKey;
use authn::webauthn::RelyingParty;
use authn::webauthn::testing::Soft;
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ingress::approval::{
    APPROVAL_HEADER, ApprovalBody, ApprovalMode, Approvals, COMMAND_APPROVAL_REQUIRED,
};
use ingress::auth::Passkeys;
use ingress::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, OutboxPage, Query,
    QueryReply,
};
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::store::Store as _;
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, Party, T0, TestCore, fast_config, genesis, test_clerk};
use ingress::wire::AssertionBody;
use ledger_core::Refusal;
use ledger_core::account::Time;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const RP: &str = "api.bloque.app";
const ORIGIN: &str = "https://api.bloque.app";
const DEPLOYMENT: &str = "d9d9d9d9d9d9d9d9d9d9d9d9d9d9d9d9";
const TTL: u64 = 120_000;

fn b64(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

fn is_refusal(v: &Value, r: Refusal) -> bool {
    v["error"] == r.code()
}

fn passkeys(mode: ApprovalMode, require_uv_at_sign_in: bool) -> Passkeys {
    Passkeys::new(
        AuthnKey::new([7; 32]),
        vec![RelyingParty {
            rp_id: RP.into(),
            origins: vec![ORIGIN.into()],
            registrations: true,
        }],
        require_uv_at_sign_in,
        TTL,
        900_000,
    )
    .with_approvals(Approvals::new(mode, Some("test"), DEPLOYMENT).unwrap())
}

async fn challenge<C: Core + Clone>(d: &Deployment<MemStore, C>, purpose: &str) -> Vec<u8> {
    let (s, v) = d
        .call(
            "POST",
            "/v1/auth/challenge",
            None,
            &[],
            Some(json!({ "purpose": purpose })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    URL_SAFE_NO_PAD
        .decode(v["challenge"].as_str().unwrap())
        .unwrap()
}

fn registration(soft: &Soft, challenge: &[u8]) -> Value {
    let (cd, att) = soft.register(challenge, ORIGIN);
    json!({ "client_data_json": b64(&cd), "attestation_object": b64(&att) })
}

fn assertion(soft: &Soft, challenge: &[u8]) -> AssertionBody {
    let (ad, cd, sig) = soft.assert(challenge, ORIGIN);
    AssertionBody {
        raw_id: b64(&soft.raw_id),
        authenticator_data: b64(&ad),
        client_data_json: b64(&cd),
        signature: b64(&sig),
    }
}

/// Register `soft` into `party`'s set with its configured token (exempt), and sign in: the
/// credential and the session.
async fn enrol<C: Core + Clone>(
    d: &Deployment<MemStore, C>,
    party: &Party,
    soft: &Soft,
    key: &str,
) -> (String, String) {
    let ch = challenge(d, "register").await;
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&party.token),
            &[("client-key", key)],
            Some(json!({
                "marker": "ordinary",
                "scheme": "webauthn-es256",
                "passkey": registration(soft, &ch),
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let credential = v["result"]["credential"].as_str().unwrap().to_owned();
    let ch = challenge(d, "authenticate").await;
    let a = assertion(soft, &ch);
    let (s, v) = d
        .call(
            "POST",
            "/v1/auth/passkey",
            None,
            &[],
            Some(json!({ "credential": credential, "assertion": a })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    (credential, v["session"].as_str().unwrap().to_owned())
}

struct World {
    d: Deployment,
    alice: Party,
    bob: Party,
    alice_soft: Soft,
    alice_cred: String,
    alice_session: String,
    bob_soft: Soft,
    bob_cred: String,
}

async fn world(mode: ApprovalMode) -> World {
    let d = Deployment::start().await;
    d.auth.set_passkeys(passkeys(mode, true));
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 1_000).await;
    let alice_soft = Soft::new(11, RP);
    let bob_soft = Soft::new(12, RP);
    let (alice_cred, alice_session) = enrol(&d, &alice, &alice_soft, "pk-alice").await;
    let (bob_cred, _) = enrol(&d, &bob, &bob_soft, "pk-bob").await;
    World {
        d,
        alice,
        bob,
        alice_soft,
        alice_cred,
        alice_session,
        bob_soft,
        bob_cred,
    }
}

fn transfer(w: &World, amount: &str) -> Value {
    json!({ "legs": [{
        "from": w.alice.account.as_deref().unwrap(),
        "to": w.bob.account.as_deref().unwrap(),
        "asset": "USD",
        "amount": amount,
    }]})
}

/// Step (a): the reply.
async fn step_a<C: Core + Clone>(
    d: &Deployment<MemStore, C>,
    token: &str,
    path: &str,
    key: &str,
    body: &Value,
) -> Value {
    let (s, v) = d
        .call(
            "POST",
            &path.replacen("/v1", "/v1/approvals", 1),
            Some(token),
            &[("client-key", key)],
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "step (a): {v}");
    v
}

/// The approval header `soft`, as `credential`, makes over step (a)'s challenge.
fn approve(reply: &Value, soft: &Soft, credential: &str) -> String {
    let ch = URL_SAFE_NO_PAD
        .decode(reply["challenge"].as_str().expect("a challenge"))
        .unwrap();
    ApprovalBody {
        credential: credential.to_owned(),
        assertion: assertion(soft, &ch),
    }
    .header()
}

async fn submit<C: Core + Clone>(
    d: &Deployment<MemStore, C>,
    token: &str,
    path: &str,
    key: &str,
    body: &Value,
    approval: Option<&str>,
) -> (StatusCode, Value) {
    let mut h = vec![("client-key", key)];
    if let Some(a) = approval {
        h.push((APPROVAL_HEADER, a));
    }
    d.call("POST", path, Some(token), &h, Some(body.clone()))
        .await
}

async fn logged(d: &Deployment, w: &World, key: &str) -> bool {
    d.store
        .find(
            &ingress::store::principal_domain(&w.alice.identity),
            ledger_core::lifecycle::keys::Surface::Command,
            key,
        )
        .await
        .unwrap()
        .is_some()
}

/// `AC-A10.1`, `AC-A10.2`, `AC-A10.5`, `AC-A10.7`: under enforce, a session's transfer without an
/// approval is refused and nothing is logged; with one it is taken in; a retry under the same
/// client key is answered from the first result with no new approval; the same key with another
/// command is `ERR-ClientKeyReused`; a read asks for none.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a10_1_2_5_no_approval_refused_an_approval_taken_in_a_retry_answered() {
    let w = world(ApprovalMode::Enforce).await;
    let (d, s) = (&w.d, w.alice_session.as_str());
    let body = transfer(&w, "10");
    let (st, v) = submit(d, s, "/v1/transfers", "t-1", &body, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], COMMAND_APPROVAL_REQUIRED);
    assert!(!logged(d, &w, "t-1").await, "nothing is taken in");

    let a = step_a(d, s, "/v1/transfers", "t-1", &body).await;
    assert_eq!(a["required"], true);
    assert_eq!(a["mode"], "enforce");
    assert!(!logged(d, &w, "t-1").await, "step (a) takes nothing in");
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let (st, first) = submit(d, s, "/v1/transfers", "t-1", &body, Some(&h)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{first}");
    // A retry: the first result, no approval asked for.
    let (st, again) = submit(d, s, "/v1/transfers", "t-1", &body, None).await;
    assert!(st.is_success(), "{again}");
    assert_eq!(again["operation_reference"], first["operation_reference"]);
    // The same key with another command.
    let (_, v) = submit(d, s, "/v1/transfers", "t-1", &transfer(&w, "11"), None).await;
    assert!(is_refusal(&v, Refusal::ClientKeyReused), "{v}");
    // A read asks for none.
    let balances = format!("/v1/balances?identity={}", w.alice.identity);
    let (st, v) = d.call("GET", &balances, Some(s), &[], None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

/// `AC-A10.3`: an approval of one command does not serve another: another amount, destination,
/// client key, or an acted-for identity. Nothing is logged.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a10_3_an_approval_of_one_command_serves_no_other() {
    let w = world(ApprovalMode::Enforce).await;
    let (d, s) = (&w.d, w.alice_session.as_str());
    let body = transfer(&w, "10");
    // Another amount.
    let a = step_a(d, s, "/v1/transfers", "t-3", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let (_, v) = submit(d, s, "/v1/transfers", "t-3", &transfer(&w, "11"), Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    assert!(!logged(d, &w, "t-3").await);
    // Another client key (fresh challenge each time: a refusal before the number is spent).
    let a = step_a(d, s, "/v1/transfers", "t-3", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let (_, v) = submit(d, s, "/v1/transfers", "t-3b", &body, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // The challenge was not spent by those refusals: the right command still goes in with it.
    let (st, v) = submit(d, s, "/v1/transfers", "t-3", &body, Some(&h)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    // Another destination.
    let mut other = body.clone();
    other["legs"][0]["to"] = json!(w.alice.account.as_deref().unwrap());
    let a = step_a(d, s, "/v1/transfers", "t-4", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let (_, v) = submit(d, s, "/v1/transfers", "t-4", &other, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
}

/// `AC-A10.4`: an approval made by another identity's credential, or over a challenge issued for
/// another identity, is refused; an expired one is refused.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a10_4_wrong_identity_and_expired_are_refused() {
    let w = world(ApprovalMode::Enforce).await;
    let (d, s) = (&w.d, w.alice_session.as_str());
    let body = transfer(&w, "10");
    // Bob's passkey signs alice's challenge.
    let a = step_a(d, s, "/v1/transfers", "t-5", &body).await;
    let h = approve(&a, &w.bob_soft, &w.bob_cred);
    let (_, v) = submit(d, s, "/v1/transfers", "t-5", &body, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // A challenge issued for bob's command (bob's own identity), presented on alice's.
    let bob_body = json!({ "legs": [{
        "from": w.bob.account.as_deref().unwrap(),
        "to": w.alice.account.as_deref().unwrap(),
        "asset": "USD", "amount": "10",
    }]});
    let (_, bob_session) = {
        let ch = challenge(d, "authenticate").await;
        let (st, v) = d
            .call(
                "POST",
                "/v1/auth/passkey",
                None,
                &[],
                Some(json!({ "credential": w.bob_cred, "assertion": assertion(&w.bob_soft, &ch) })),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        ((), v["session"].as_str().unwrap().to_owned())
    };
    let a = step_a(d, &bob_session, "/v1/transfers", "t-5", &bob_body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let (_, v) = submit(d, s, "/v1/transfers", "t-5", &body, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // Expired: past the challenge's lifetime on ingress's clock.
    let a = step_a(d, s, "/v1/transfers", "t-5", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    d.clock.advance(TTL);
    let (_, v) = submit(d, s, "/v1/transfers", "t-5", &body, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    assert!(!logged(d, &w, "t-5").await);
}

/// `AC-A10.4`, `T-040-03`: a number spent elsewhere (another instance, or an attempt whose command
/// then failed to log) is not accepted again; a challenge survives a restart of ingress between
/// step (a) and step (b), on another instance over the same store; two concurrent submissions of
/// one approval log one command.
#[tokio::test(flavor = "multi_thread")]
async fn t_040_03_single_use_across_instances_restarts_and_races() {
    let w = world(ApprovalMode::Enforce).await;
    let (d, s) = (&w.d, w.alice_session.as_str());
    let body = transfer(&w, "10");

    // Spent elsewhere first: refused, and the client can ask for a new challenge.
    let a = step_a(d, s, "/v1/transfers", "t-6", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let ch = URL_SAFE_NO_PAD
        .decode(a["challenge"].as_str().unwrap())
        .unwrap();
    let nonce: [u8; 16] = ch[..16].try_into().unwrap();
    assert!(
        d.store
            .use_challenge(
                nonce,
                a["expires_at"].as_u64().unwrap(),
                ingress::clock::Clock::now(&d.clock)
            )
            .await
            .unwrap()
    );
    let (_, v) = submit(d, s, "/v1/transfers", "t-6", &body, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    assert!(!logged(d, &w, "t-6").await);
    let a = step_a(d, s, "/v1/transfers", "t-6", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let (st, v) = submit(d, s, "/v1/transfers", "t-6", &body, Some(&h)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");

    // Restart between (a) and (b): another instance over the same store takes it.
    let a = step_a(d, s, "/v1/transfers", "t-7", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let (_replica, router) = d.another_replica(fast_config("ingress-2"));
    let mut hdrs = vec![("client-key", "t-7"), (APPROVAL_HEADER, h.as_str())];
    let (st, v) = ingress::testkit::call(
        &router,
        "POST",
        "/v1/transfers",
        Some(s),
        &hdrs,
        Some(body.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");

    // Two concurrent submissions of one approval, under two client keys sharing one challenge:
    // impossible (the key is in the digest), so race the same request: one operation results.
    let a = step_a(d, s, "/v1/transfers", "t-8", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    hdrs = vec![("client-key", "t-8"), (APPROVAL_HEADER, h.as_str())];
    let (r1, r2) = tokio::join!(
        ingress::testkit::call(
            &d.router,
            "POST",
            "/v1/transfers",
            Some(s),
            &hdrs,
            Some(body.clone())
        ),
        ingress::testkit::call(
            &router,
            "POST",
            "/v1/transfers",
            Some(s),
            &hdrs,
            Some(body.clone())
        ),
    );
    let refs: Vec<&Value> = [&r1, &r2]
        .iter()
        .filter(|(st, _)| st.is_success())
        .map(|(_, v)| &v["operation_reference"])
        .collect();
    assert!(!refs.is_empty(), "{r1:?} {r2:?}");
    assert!(
        refs.windows(2).all(|p| p[0] == p[1]),
        "one operation: {refs:?}"
    );
    for (st, v) in [&r1, &r2] {
        assert!(
            st.is_success() || is_refusal(v, Refusal::CredentialInvalid),
            "{st} {v}"
        );
    }
}

/// `T-040-03`: two instances racing on one number, at the store: exactly one wins.
#[tokio::test(flavor = "multi_thread")]
async fn t_040_03_racing_on_one_number_one_wins() {
    let d = Deployment::start().await;
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let store = d.store.clone();
        tasks.push(tokio::spawn(async move {
            store.use_challenge([9; 16], 1_000, 10).await.unwrap()
        }));
    }
    let mut wins = 0;
    for t in tasks {
        wins += usize::from(t.await.unwrap());
    }
    assert_eq!(wins, 1);
    // Kept past its expiry for the margin: an instance whose clock runs ahead does not forget it.
    assert!(
        !d.store
            .use_challenge([9; 16], 1_000, 1_000 + 59_000)
            .await
            .unwrap()
    );
}

/// `AC-A10.11`: the device's own check is required on an approval even where sign-in does not
/// ask for it; and a second credential of the identity may approve.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a10_11_user_verification_forced_and_any_credential_of_the_set() {
    let d = Deployment::start().await;
    d.auth.set_passkeys(passkeys(ApprovalMode::Enforce, false));
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 1_000).await;
    let mut no_uv = Soft::new(21, RP);
    no_uv.flags &= !0b0000_0100;
    let (cred, session) = enrol(&d, &alice, &no_uv, "pk-a1").await;
    let second = Soft::new(22, RP);
    let (second_cred, _) = enrol(&d, &alice, &second, "pk-a2").await;
    let body = json!({ "legs": [{
        "from": alice.account.as_deref().unwrap(),
        "to": bob.account.as_deref().unwrap(),
        "asset": "USD", "amount": "10",
    }]});
    let a = step_a(&d, &session, "/v1/transfers", "t-9", &body).await;
    let h = approve(&a, &no_uv, &cred);
    let (_, v) = submit(&d, &session, "/v1/transfers", "t-9", &body, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // The second credential approves a command of the session the first made.
    let a = step_a(&d, &session, "/v1/transfers", "t-9", &body).await;
    let h = approve(&a, &second, &second_cred);
    let (st, v) = submit(&d, &session, "/v1/transfers", "t-9", &body, Some(&h)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
}

/// `AC-A10.8`, `AC-A10.12`: a session alone cannot add a device-bound credential; an existing
/// credential's approval of that registration can; and the approval of one registration does not
/// add another.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a10_8_12_adding_a_passkey_needs_an_approval_of_that_passkey() {
    let w = world(ApprovalMode::Enforce).await;
    let (d, s) = (&w.d, w.alice_session.as_str());
    let new = Soft::new(31, RP);
    let thief = Soft::new(32, RP);
    let reg = |soft: &Soft, ch: &[u8]| json!({ "marker": "ordinary", "scheme": "webauthn-es256", "passkey": registration(soft, ch) });
    let ch = challenge(d, "register").await;
    let body = reg(&new, &ch);
    let (st, v) = submit(d, s, "/v1/credentials", "c-1", &body, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], COMMAND_APPROVAL_REQUIRED);
    // Approved for `new`, sent with the thief's registration: refused.
    let a = step_a(d, s, "/v1/credentials", "c-1", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let ch2 = challenge(d, "register").await;
    let (_, v) = submit(d, s, "/v1/credentials", "c-1", &reg(&thief, &ch2), Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // The registration it approved goes in.
    let (st, v) = submit(d, s, "/v1/credentials", "c-1", &body, Some(&h)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

/// `NFR-7`: step (a)'s routes are default-deny like every route, with and without a path
/// parameter; and a command sent with no client key (the key ingress derives) is approved and taken
/// in, the key being the same at both steps.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_7_step_a_refuses_the_unauthenticated_and_a_derived_key_works() {
    let w = world(ApprovalMode::Enforce).await;
    let d = &w.d;
    for path in [
        "/v1/approvals/transfers",
        "/v1/approvals/withdrawals/x/submit",
    ] {
        let (st, v) = d.call("POST", path, None, &[], Some(json!({}))).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{path}: {v}");
        assert!(is_refusal(&v, Refusal::CredentialInvalid), "{path}: {v}");
    }
    let s = w.alice_session.as_str();
    let body = transfer(&w, "7");
    let (st, a) = d
        .call(
            "POST",
            "/v1/approvals/transfers",
            Some(s),
            &[],
            Some(body.clone()),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{a}");
    assert_eq!(a["client_key_derived"], true);
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let (st, v) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(s),
            &[(APPROVAL_HEADER, h.as_str())],
            Some(body),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["client_key"], a["client_key"]);
}

/// `AC-A10.6`, `T-040-11`: a configured token (a bearer secret presented as it is) needs no
/// approval, under enforce.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a10_6_a_configured_token_is_exempt() {
    let w = world(ApprovalMode::Enforce).await;
    let body = transfer(&w, "10");
    let a = step_a(&w.d, &w.alice.token, "/v1/transfers", "t-10", &body).await;
    assert_eq!(a["required"], false);
    assert!(a.get("challenge").is_none());
    let (st, v) = submit(&w.d, &w.alice.token, "/v1/transfers", "t-10", &body, None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
}

/// `T-040-07`: off changes nothing for an existing caller (a session's transfer with no approval
/// is taken in as before, and a header it might send is ignored); report-only refuses nothing.
#[tokio::test(flavor = "multi_thread")]
async fn t_040_07_off_changes_nothing_and_report_only_refuses_nothing() {
    for mode in [ApprovalMode::Off, ApprovalMode::ReportOnly] {
        let w = world(mode).await;
        let (d, s) = (&w.d, w.alice_session.as_str());
        let body = transfer(&w, "10");
        let (st, v) = submit(d, s, "/v1/transfers", "t-11", &body, None).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{mode:?}: {v}");
        let (st, v) = submit(
            d,
            s,
            "/v1/transfers",
            "t-12",
            &body,
            Some("not-an-approval"),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{mode:?}: {v}");
        let a = step_a(d, s, "/v1/transfers", "t-13", &body).await;
        assert_eq!(a["required"], mode != ApprovalMode::Off, "{mode:?}");
    }
}

/// A core whose credential reads can be made to fail: the book that cannot be read.
#[derive(Clone)]
struct Flaky {
    inner: TestCore,
    down: Arc<AtomicBool>,
}

impl Core for Flaky {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        self.inner.apply(input).await
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        self.inner.apply_batch(inputs).await
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> Result<OutboxPage, CoreError> {
        self.inner.outbox(cursor, undelivered, now).await
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        self.inner.last_tick().await
    }

    async fn next_due(&self) -> Result<Option<Time>, CoreError> {
        self.inner.next_due().await
    }

    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        self.inner.query(caller, query, now).await
    }

    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(CoreError::Unavailable);
        }
        self.inner.credential(credential).await
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        self.inner.party_of(principal).await
    }

    async fn genesis_open(&self) -> Result<bool, CoreError> {
        self.inner.genesis_open().await
    }
}

/// `AC-A10.13`: while the current credential list cannot be read, an approval is refused with
/// `ERR-DecisionUnavailable` and no stored copy is used; and a credential removed from the set
/// approves nothing.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a10_13_an_unreadable_list_and_a_removed_credential() {
    let journal = MemJournal::new();
    let inner = InProcessCore::in_memory(genesis(T0), journal.clone()).with_clerk(test_clerk());
    inner.start().unwrap();
    let founded = inner.founded().unwrap();
    let core = Flaky {
        inner,
        down: Arc::default(),
    };
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        founded,
        journal,
    )
    .await;
    d.auth.set_passkeys(passkeys(ApprovalMode::Enforce, true));
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 1_000).await;
    let first = Soft::new(41, RP);
    let second = Soft::new(42, RP);
    let (_, session) = enrol(&d, &alice, &first, "pk-41").await;
    let (second_cred, _) = enrol(&d, &alice, &second, "pk-42").await;
    let body = json!({ "legs": [{
        "from": alice.account.as_deref().unwrap(),
        "to": bob.account.as_deref().unwrap(),
        "asset": "USD", "amount": "10",
    }]});

    // The book cannot be read: refused with ERR-DecisionUnavailable, even though ingress's
    // durable copy holds the credential (the sign-ins above wrote it).
    let a = step_a(&d, &session, "/v1/transfers", "t-20", &body).await;
    let h = approve(&a, &second, &second_cred);
    core.down.store(true, Ordering::SeqCst);
    let (_, v) = submit(&d, &session, "/v1/transfers", "t-20", &body, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::DecisionUnavailable), "{v}");
    core.down.store(false, Ordering::SeqCst);

    // The second credential removed (with the configured token, which is exempt): it approves
    // nothing.
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/credentials/{second_cred}/remove"),
            Some(&alice.token),
            &[("client-key", "rm-42")],
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let a = step_a(&d, &session, "/v1/transfers", "t-21", &body).await;
    let h = approve(&a, &second, &second_cred);
    let (_, v) = submit(&d, &session, "/v1/transfers", "t-21", &body, Some(&h)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
}

/// `AC-A10.6`, `T-040-11`: an issued secret (an API key), presented as it is, needs no approval
/// under enforce.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a10_6_an_api_key_is_exempt() {
    let w = world(ApprovalMode::Enforce).await;
    let (s, v) =
        w.d.call(
            "POST",
            "/v1/credentials",
            Some(&w.alice.token),
            &[("client-key", "ak-1")],
            Some(json!({ "marker": "ordinary", "scheme": "api-key" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let key = v["api_key"].as_str().expect("the API key").to_owned();
    let body = transfer(&w, "10");
    let a = step_a(&w.d, &key, "/v1/transfers", "t-22", &body).await;
    assert_eq!(a["required"], false);
    let (st, v) = submit(&w.d, &key, "/v1/transfers", "t-22", &body, None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
}

/// `T-040-03`, `AC-A10.4`: two instances verifying one approval at the same moment accept it
/// once; the one-time number is the only thing that stops the second. (Two client keys cannot
/// share one approval: the key is in the digest, so the second would be refused for its digest
/// before the number is read. The race is therefore run on the approval check itself, which
/// passes twice without the single-use store.)
#[tokio::test(flavor = "multi_thread")]
async fn t_040_03_two_instances_verifying_one_approval_accept_it_once() {
    let w = world(ApprovalMode::Enforce).await;
    let (d, s) = (&w.d, w.alice_session.as_str());
    let body = transfer(&w, "10");
    let a = step_a(d, s, "/v1/transfers", "t-23", &body).await;
    let h = approve(&a, &w.alice_soft, &w.alice_cred);
    let digest: [u8; 32] = hex::decode(a["command_digest"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let who = ingress::auth::Authenticated {
        principal: w.alice.identity.clone(),
        credential: w.alice_cred.clone(),
        kind: ingress::auth::CredentialKind::Holder,
        proof: ingress::auth::Proof::Session,
    };
    let (other, _router) = d.another_replica(fast_config("ingress-2"));
    let (one, two) = (d.replica.shared.clone(), other.shared.clone());
    let pk = d.auth.passkeys().unwrap();
    let kind = ingress::approval::Kind::Transfer;
    let (r1, r2) = tokio::join!(
        ingress::approval::verify(&one, &pk, &who, kind, &digest, &h),
        ingress::approval::verify(&two, &pk, &who, kind, &digest, &h),
    );
    assert_eq!(
        usize::from(r1.is_ok()) + usize::from(r2.is_ok()),
        1,
        "{r1:?} {r2:?}"
    );
    for r in [r1, r2] {
        assert!(
            r.is_ok() || r == Err(ingress::approval::Rejected::NumberSpent),
            "{r:?}"
        );
    }
}
