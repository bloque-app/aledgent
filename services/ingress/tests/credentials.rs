// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-028` through ingress (`DEC-19`): passkeys registered, signed in and removed; API keys issued
//! once, used, rotated and revoked; a passkey's reclaim proof; and identities opened with several
//! initial credentials, which is how passkeys imported from Kreivo `Pass` land.

use authn::token::AuthnKey;
use authn::webauthn::RelyingParty;
use authn::webauthn::testing::Soft;
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ingress::auth::Passkeys;
use ingress::store::Store as _;
use ingress::testkit::Deployment;
use ledger_core::Refusal;
use ledger_core::custody::reclaim_challenge;
use ledger_core::id::AccountId;
use serde_json::{Value, json};

const RP: &str = "api.bloque.app";
const ORIGIN: &str = "https://api.bloque.app";

fn is_refusal(v: &Value, r: Refusal) -> bool {
    v["error"] == r.code()
}

fn b64(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

async fn with_passkeys() -> Deployment {
    let d = Deployment::start().await;
    d.auth.set_passkeys(Passkeys::new(
        AuthnKey::new([7; 32]),
        vec![RelyingParty {
            rp_id: RP.into(),
            origins: vec![ORIGIN.into()],
            registrations: true,
        }],
        true,
        120_000,
        900_000,
    ));
    d
}

async fn challenge(d: &Deployment, purpose: &str) -> Vec<u8> {
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

fn assertion(soft: &Soft, challenge: &[u8]) -> Value {
    let (ad, cd, sig) = soft.assert(challenge, ORIGIN);
    json!({
        "raw_id": b64(&soft.raw_id),
        "authenticator_data": b64(&ad),
        "client_data_json": b64(&cd),
        "signature": b64(&sig),
    })
}

/// Register `soft` into `token`'s set; its credential.
async fn register(d: &Deployment, token: &str, soft: &Soft, key: &str) -> String {
    let ch = challenge(d, "register").await;
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(token),
            &[("client-key", key)],
            Some(json!({
                "marker": "ordinary",
                "scheme": "webauthn-es256",
                "passkey": registration(soft, &ch),
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["result"]["credential"].as_str().unwrap().to_owned()
}

/// Sign in with `soft` as `credential`; the session.
async fn sign_in(d: &Deployment, soft: &Soft, credential: &str) -> (StatusCode, Value) {
    let ch = challenge(d, "authenticate").await;
    d.call(
        "POST",
        "/v1/auth/passkey",
        None,
        &[],
        Some(json!({ "credential": credential, "assertion": assertion(soft, &ch) })),
    )
    .await
}

/// `REQ-AU-20`, `REQ-AU-3`, `NFR-7`: a passkey joins the set through a member, signs in, acts
/// through its session, and stops at once when removed. A registration's challenge is single-use,
/// and a repeat under the same client key gets the stored reply, not a second verification.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_20_a_passkey_registers_signs_in_and_stops_when_removed() {
    let d = with_passkeys().await;
    let alice = d.party("alice", 100).await;
    let soft = Soft::new(11, RP);

    let ch = challenge(&d, "register").await;
    let body = json!({
        "marker": "ordinary",
        "scheme": "webauthn-es256",
        "passkey": registration(&soft, &ch),
    });
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[("client-key", "pk-1")],
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let passkey = v["result"]["credential"].as_str().unwrap().to_owned();
    // The same request again: the stored reply (REQ-OP-10).
    let (s, again) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[("client-key", "pk-1")],
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["result"]["credential"], passkey.as_str());
    // Under another client key, the used challenge is refused.
    let (_, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[("client-key", "pk-2")],
            Some(body),
        )
        .await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");

    // Sign in; the session acts as alice, with the passkey.
    let (s, v) = sign_in(&d, &soft, &passkey).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["principal"], alice.identity.as_str());
    let session = v["session"].as_str().unwrap().to_owned();
    let balances = format!("/v1/balances?identity={}", alice.identity);
    let (s, v) = d.call("GET", &balances, Some(&session), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // A wrong signer, an unknown credential, a reused assertion: the same refusal.
    let (_, v) = sign_in(&d, &Soft::new(12, RP), &passkey).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    let (_, v) = sign_in(&d, &soft, &alice.credential).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    let ch = challenge(&d, "authenticate").await;
    let reused = json!({ "credential": passkey, "assertion": assertion(&soft, &ch) });
    let (s, _) = d
        .call("POST", "/v1/auth/passkey", None, &[], Some(reused.clone()))
        .await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = d
        .call("POST", "/v1/auth/passkey", None, &[], Some(reused))
        .await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // A challenge for one ceremony does not serve the other.
    let ch = challenge(&d, "register").await;
    let (_, v) = d
        .call(
            "POST",
            "/v1/auth/passkey",
            None,
            &[],
            Some(json!({ "credential": passkey, "assertion": assertion(&soft, &ch) })),
        )
        .await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");

    // Removed: the session stops at the next call (REQ-AU-3).
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/credentials/{passkey}/remove"),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = d.call("GET", &balances, Some(&session), &[], None).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    let (_, v) = sign_in(&d, &soft, &passkey).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
}

/// Amendment 0027: a `register` challenge that names an identity serves a registration into that
/// identity's set only, still once; a refused attempt into another set does not use it up. One that
/// names none serves any set, as before. Only a `register` challenge may name one.
#[tokio::test(flavor = "multi_thread")]
async fn amendment_0027_a_bound_register_challenge_serves_only_its_identity() {
    let d = with_passkeys().await;
    let alice = d.party("alice", 0).await;
    let bob = d.party("bob", 0).await;
    let bound = |identity: &str, purpose: &str| json!({ "purpose": purpose, "identity": identity });
    let (s, v) = d
        .call(
            "POST",
            "/v1/auth/challenge",
            None,
            &[],
            Some(bound(&alice.identity, "register")),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ch = URL_SAFE_NO_PAD
        .decode(v["challenge"].as_str().unwrap())
        .unwrap();
    let soft = Soft::new(14, RP);
    let body = json!({
        "marker": "ordinary",
        "scheme": "webauthn-es256",
        "passkey": registration(&soft, &ch),
    });
    let post = |token: String, key: &'static str, body: Value| {
        let d = &d;
        async move {
            d.call(
                "POST",
                "/v1/credentials",
                Some(&token),
                &[("client-key", key)],
                Some(body),
            )
            .await
        }
    };

    // Into bob's set: refused, and the challenge is not used up.
    let (_, v) = post(bob.token.clone(), "bound-bob", body.clone()).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // Into alice's: accepted.
    let (s, v) = post(alice.token.clone(), "bound-alice", body.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let passkey = v["result"]["credential"].as_str().unwrap().to_owned();
    let (s, v) = sign_in(&d, &soft, &passkey).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["principal"], alice.identity.as_str());
    // Once: under another client key, the used challenge is refused.
    let (_, v) = post(alice.token.clone(), "bound-alice-2", body).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");

    // An unbound challenge serves bob, as before.
    let other = Soft::new(15, RP);
    register(&d, &bob.token, &other, "unbound-bob").await;

    // Only `register` takes an identity, and it must parse.
    for req in [
        bound(&alice.identity, "authenticate"),
        bound("did:aledgent:id:nope", "register"),
    ] {
        let (_, v) = d
            .call("POST", "/v1/auth/challenge", None, &[], Some(req))
            .await;
        assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    }
}

/// `NFR-7`, `DEP-7`: a holder's session is refused while the switch is off, and the two auth
/// routes refuse everything on a deployment that accepts no passkey.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_7_sessions_follow_the_dep_7_switch_and_no_passkeys_means_no_route() {
    let d = with_passkeys().await;
    let alice = d.party("alice", 0).await;
    let soft = Soft::new(13, RP);
    let passkey = register(&d, &alice.token, &soft, "pk").await;
    let (s, v) = sign_in(&d, &soft, &passkey).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let session = v["session"].as_str().unwrap().to_owned();
    d.auth.allow_holders(false);
    let balances = format!("/v1/balances?identity={}", alice.identity);
    let (_, v) = d.call("GET", &balances, Some(&session), &[], None).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    let (_, v) = sign_in(&d, &soft, &passkey).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");

    let plain = Deployment::start().await;
    let (s, v) = plain
        .call(
            "POST",
            "/v1/auth/challenge",
            None,
            &[],
            Some(json!({ "purpose": "authenticate" })),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // A session forged under another key is no session.
    let (_, v) = plain
        .call("GET", &balances, Some(&session), &[], None)
        .await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
}

/// `REQ-AU-8`, `REQ-AU-20`, `INV-22`, `AC-G2.2`: an API key is shown once and kept as a hash; it
/// authenticates as its identity, limited to its scope; a scoped key cannot register a wider
/// credential; rotation and removal take effect at once.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_8_an_api_key_is_shown_once_scoped_rotated_and_revoked() {
    let d = with_passkeys().await;
    let alice = d.party("alice", 100).await;
    let account = alice.account.clone().unwrap();
    let issue = |key: &'static str, scope: Option<Vec<String>>| {
        let mut body = json!({ "marker": "ordinary", "scheme": "api-key" });
        if let Some(s) = scope {
            body["scope"] = json!(s);
        }
        (key, body)
    };
    let (k, body) = issue("ak-1", Some(vec![account.clone()]));
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[("client-key", k)],
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let key = v["api_key"].as_str().unwrap().to_owned();
    let credential = v["result"]["credential"].as_str().unwrap().to_owned();
    assert!(key.starts_with("aledgent_ak1."));
    // Shown once: a repeat returns the credential, not the key.
    let (s, again) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[("client-key", k)],
            Some(body),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["result"]["credential"], credential.as_str());
    assert!(again.get("api_key").is_none(), "{again}");
    // Nothing the received-log holds is the key.
    let rows = d.store.rows().await.unwrap();
    let secret = key.rsplit('.').next().unwrap();
    assert!(
        rows.iter()
            .all(|r| !String::from_utf8_lossy(&r.new.body).contains(secret))
    );

    // It authenticates, within its scope.
    let balances = format!("/v1/balances?account={account}");
    let (s, v) = d.call("GET", &balances, Some(&key), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // A wrong secret for the same credential is refused.
    let forged = format!("{}.{}", key.rsplit_once('.').unwrap().0, b64(&[0; 32]));
    let (_, v) = d.call("GET", &balances, Some(&forged), &[], None).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // A scoped key cannot register an unscoped one (INV-22).
    let (_, unscoped) = issue("ak-wide", None);
    let (_, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&key),
            &[("client-key", "ak-wide")],
            Some(unscoped),
        )
        .await;
    assert!(is_refusal(&v, Refusal::NotAuthorised), "{v}");

    // Rotate: a new key, then the old one removed. The old one stops at once.
    let (k2, body2) = issue("ak-2", Some(vec![account.clone()]));
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[("client-key", k2)],
            Some(body2),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let key2 = v["api_key"].as_str().unwrap().to_owned();
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/credentials/{credential}/remove"),
            Some(&key2),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = d.call("GET", &balances, Some(&key), &[], None).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    let (s, v) = d.call("GET", &balances, Some(&key2), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // DEP-7: a holder's key is refused while the switch is off.
    d.auth.allow_holders(false);
    let (_, v) = d.call("GET", &balances, Some(&key2), &[], None).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
}

/// `AC-A7.3`, `AC-A7.4`, `REQ-AU-13`: a passkey proves itself for a reclaim with an assertion over
/// the account's reclaim challenge; a stale epoch, or an assertion for a `key` credential, is
/// refused.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a7_3_a_passkey_reclaims_custody_and_ac_a7_4_a_bad_proof_does_not() {
    let d = with_passkeys().await;
    let alice = d.party("alice", 100).await;
    let a = alice.account.clone().unwrap();
    let soft = Soft::new(14, RP);
    let passkey = register(&d, &alice.token, &soft, "pk").await;
    let custody = format!("/v1/accounts/{a}/custody");
    let (_, v) = d.call("GET", &custody, Some(&alice.token), &[], None).await;
    assert_eq!(v["custody"], "delegated", "{v}");
    let epoch = v["epoch"].as_u64().unwrap();
    let (_, m) = d
        .call(
            "GET",
            &format!("/v1/accounts/{a}/recovery-mechanisms"),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    let account = AccountId::parse(&a).unwrap();
    let reclaim = format!("/v1/accounts/{a}/custody/reclaim");
    let attempt = |credential: &str, challenge: [u8; 32]| {
        json!({
            "proof_credential": credential,
            "assertion": assertion(&soft, &challenge),
            "acknowledged": m["digest"],
        })
    };
    // A stale epoch, and a `key` credential presenting an assertion.
    for body in [
        attempt(&passkey, reclaim_challenge(&account, epoch + 1)),
        attempt(&alice.credential, reclaim_challenge(&account, epoch)),
    ] {
        let (_, v) = d
            .call("POST", &reclaim, Some(&alice.token), &[], Some(body))
            .await;
        assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    }
    // Both a signature and an assertion: refused before anything is checked.
    let (_, v) = d
        .call(
            "POST",
            &reclaim,
            Some(&alice.token),
            &[],
            Some(json!({
                "proof_credential": passkey,
                "signature": "11".repeat(64),
                "assertion": assertion(&soft, &reclaim_challenge(&account, epoch)),
            })),
        )
        .await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    let (s, v) = d
        .call(
            "POST",
            &reclaim,
            Some(&alice.token),
            &[],
            Some(attempt(&passkey, reclaim_challenge(&account, epoch))),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = d.call("GET", &custody, Some(&alice.token), &[], None).await;
    assert_eq!(v["custody"], "owned", "{v}");
}

/// Grant the operator its import authority (`REQ-OS-1`).
async fn import_grant(d: &Deployment) {
    let (s, v) = d
        .call(
            "POST",
            "/v1/ops/operator-grants",
            Some(&d.operator.token),
            &[("client-key", "import-grant")],
            Some(json!({
                "principal": d.operator.identity,
                "kinds": ["import_credentials"],
                "justification": "migration from Kreivo Pass",
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

async fn import(
    d: &Deployment,
    identity: &str,
    verifiers: &[String],
    key: &str,
) -> (StatusCode, Value) {
    d.call(
        "POST",
        &format!("/v1/ops/identities/{identity}/credentials"),
        Some(&d.operator.token),
        &[("client-key", key)],
        Some(json!({
            "credentials": verifiers
                .iter()
                .map(|v| json!({ "scheme": "webauthn-es256", "verifier": v }))
                .collect::<Vec<_>>(),
            "justification": "migration from Kreivo Pass, batch 1",
        })),
    )
    .await
}

/// `AC-N1.1`, `AC-N1.2`, `REQ-MG-7`: the operator imports passkeys into an identity's set; each is a
/// member and signs in. A passkey of an `rpId` the deployment does not accept is refused, and so is
/// an import without the import grant.
#[tokio::test(flavor = "multi_thread")]
async fn ac_n1_1_imported_passkeys_sign_in_and_ac_n1_2_unaccepted_ones_do_not() {
    let d = with_passkeys().await;
    let alice = d.party("alice", 0).await;
    let one = Soft::new(15, RP);
    let two = Soft::new(16, RP);
    let verifier = |s: &Soft| hex::encode(s.key().encode());
    let (_, v) = import(&d, &alice.identity, &[verifier(&one)], "no-grant").await;
    assert!(is_refusal(&v, Refusal::NotAuthorised), "{v}");
    import_grant(&d).await;
    let (s, v) = import(
        &d,
        &alice.identity,
        &[verifier(&one), verifier(&two)],
        "batch-1",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let credentials: Vec<String> = v["result"]["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(credentials.len(), 2);
    for (soft, credential) in [(&one, &credentials[0]), (&two, &credentials[1])] {
        let (s, r) = sign_in(&d, soft, credential).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        assert_eq!(r["principal"], alice.identity.as_str());
    }
    // Another relying party's passkey, a malformed one, one already held.
    let other = Soft::new(17, "evil.example");
    for (bad, key, why) in [
        (verifier(&other), "other-rp", Refusal::CredentialInvalid),
        ("00".to_owned(), "malformed", Refusal::CredentialInvalid),
        (verifier(&one), "held", Refusal::CredentialInvalid),
    ] {
        let (_, v) = import(&d, &alice.identity, &[bad], key).await;
        assert!(is_refusal(&v, why), "{key}: {v}");
    }
}

/// `AC-N1.3`, `REQ-MG-8`: no import while an account of the identity is owned.
#[tokio::test(flavor = "multi_thread")]
async fn ac_n1_3_no_import_into_an_identity_with_an_owned_account() {
    let d = with_passkeys().await;
    let alice = d.party("alice", 0).await;
    import_grant(&d).await;
    let a = alice.account.clone().unwrap();
    // Take the account into owned custody with alice's key credential.
    let soft = Soft::new(18, RP);
    let passkey = register(&d, &alice.token, &soft, "pk").await;
    let (_, v) = d
        .call(
            "GET",
            &format!("/v1/accounts/{a}/custody"),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    let epoch = v["epoch"].as_u64().unwrap();
    let (_, m) = d
        .call(
            "GET",
            &format!("/v1/accounts/{a}/recovery-mechanisms"),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    let account = AccountId::parse(&a).unwrap();
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/accounts/{a}/custody/reclaim"),
            Some(&alice.token),
            &[],
            Some(json!({
                "proof_credential": passkey,
                "assertion": assertion(&soft, &reclaim_challenge(&account, epoch)),
                "acknowledged": m["digest"],
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = import(
        &d,
        &alice.identity,
        &[hex::encode(Soft::new(19, RP).key().encode())],
        "owned",
    )
    .await;
    assert!(is_refusal(&v, Refusal::NotAuthorised), "{v}");
}

/// `F-028` (owner, 2026-10-02): a sign count is durable and shared. A replica that never saw a
/// sign-in refuses an assertion whose count another replica already accepted.
#[tokio::test(flavor = "multi_thread")]
async fn a_sign_count_is_durable_across_replicas() {
    let d = with_passkeys().await;
    let alice = d.party("alice", 0).await;
    let mut soft = Soft::new(20, RP);
    soft.flags &= !0b0001_1000; // device-bound: no BE, no BS, so the count matters.
    let passkey = register(&d, &alice.token, &soft, "pk").await;
    soft.counter = 7;
    let ch = challenge(&d, "authenticate").await;
    let body = json!({ "credential": passkey, "assertion": assertion(&soft, &ch) });
    let (s, v) = d
        .call("POST", "/v1/auth/passkey", None, &[], Some(body))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // The same count, on another replica over the same store, with a fresh challenge.
    let (_replica, other) = d.another_replica(ingress::testkit::fast_config("ingress-b"));
    let (_, v) = ingress::testkit::call(
        &other,
        "POST",
        "/v1/auth/challenge",
        None,
        &[],
        Some(json!({ "purpose": "authenticate" })),
    )
    .await;
    let ch = URL_SAFE_NO_PAD
        .decode(v["challenge"].as_str().unwrap())
        .unwrap();
    let body = json!({ "credential": passkey, "assertion": assertion(&soft, &ch) });
    let (_, v) =
        ingress::testkit::call(&other, "POST", "/v1/auth/passkey", None, &[], Some(body)).await;
    assert!(is_refusal(&v, Refusal::CredentialInvalid), "{v}");
    // A higher count passes there.
    soft.counter = 8;
    let (_, v) = ingress::testkit::call(
        &other,
        "POST",
        "/v1/auth/challenge",
        None,
        &[],
        Some(json!({ "purpose": "authenticate" })),
    )
    .await;
    let ch = URL_SAFE_NO_PAD
        .decode(v["challenge"].as_str().unwrap())
        .unwrap();
    let body = json!({ "credential": passkey, "assertion": assertion(&soft, &ch) });
    let (s, v) =
        ingress::testkit::call(&other, "POST", "/v1/auth/passkey", None, &[], Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}
