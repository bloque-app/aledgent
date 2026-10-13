// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Integrating parties at ingress, and the client key's domain (`REQ-ID-7`, `REQ-OP-10`,
//! amendment 0011 R4, `OQ-65`, T-009-16): the operator routes, and dedup on (integrating party,
//! surface, key) with the initiating principal part of the request.

use std::time::Duration;

use axum::http::StatusCode;
use ingress::store::{InputKind, Store, party_domain, principal_domain};
use ingress::testkit::{Deployment, Party, USD};
use ledger_core::Refusal;
use serde_json::{Value, json};

fn is_refusal(v: &Value, r: Refusal) -> bool {
    *v == json!({ "error": r.code(), "description": r.description() })
}

/// An organisation identity, opened by the operator: an integrating party's identifier.
async fn organisation(d: &Deployment, name: &str) -> String {
    let (s, v) = d
        .call(
            "POST",
            "/v1/identities",
            Some(&d.operator.token),
            &[("client-key", &format!("org-{name}"))],
            Some(json!({ "kind": "organisation", "verifier": "0b" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "open_identity: {v}");
    v["result"]["identity"].as_str().unwrap().to_owned()
}

async fn register(
    d: &Deployment,
    token: &str,
    party: &str,
    principals: &[&Party],
) -> (StatusCode, Value) {
    d.call(
        "POST",
        "/v1/ops/integrating-parties",
        Some(token),
        &[],
        Some(json!({
            "party": party,
            "principals": principals.iter().map(|p| p.identity.clone()).collect::<Vec<_>>(),
            "justification": "onboarding",
        })),
    )
    .await
}

async fn tie(
    d: &Deployment,
    token: &str,
    party: &str,
    who: &Party,
    why: &str,
) -> (StatusCode, Value) {
    d.call(
        "POST",
        &format!("/v1/ops/integrating-parties/{party}/principals"),
        Some(token),
        &[],
        Some(json!({ "principal": who.identity, "justification": why })),
    )
    .await
}

async fn reserve(d: &Deployment, who: &Party, key: &str, amount: &str) -> (StatusCode, Value) {
    d.call(
        "POST",
        "/v1/reservations",
        Some(&who.token),
        &[("client-key", key)],
        Some(json!({ "payer": who.account, "asset": USD, "amount": amount })),
    )
    .await
}

/// `REQ-ID-7`: the operator registers a party with its principals, ties another later, and lists
/// them; nobody else can do either (`ERR-NotAuthorised`); both need a justification
/// (`ERR-JustificationRequired`); a principal is tied to at most one party.
#[tokio::test(flavor = "multi_thread")]
async fn req_id_7_the_operator_registers_parties_and_ties_principals() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 0).await;
    let bob = d.party("bob", 0).await;
    let party = organisation(&d, "acme").await;
    let other = organisation(&d, "globex").await;
    let op = d.operator.token.clone();

    // Not a holder, and not without a reason.
    let (s, v) = register(&d, &alice.token, &party, &[&alice]).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(is_refusal(&v, Refusal::NotAuthorised), "{v}");
    let (s, v) = d
        .call(
            "POST",
            "/v1/ops/integrating-parties",
            Some(&op),
            &[],
            Some(json!({ "party": party, "principals": [alice.identity], "justification": " " })),
        )
        .await;
    assert!(is_refusal(&v, Refusal::JustificationRequired), "{s} {v}");

    let (s, v) = register(&d, &op, &party, &[&alice]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["party"], party);
    // Registered once.
    let (s, v) = register(&d, &op, &party, &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");

    let (s, v) = tie(&d, &alice.token, &party, &bob, "self-service").await;
    assert!(is_refusal(&v, Refusal::NotAuthorised), "{s} {v}");
    let (s, v) = tie(&d, &op, &party, &bob, "").await;
    assert!(is_refusal(&v, Refusal::JustificationRequired), "{s} {v}");
    let (s, v) = tie(&d, &op, &party, &bob, "a second service").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // At most one party.
    let (s, v) = register(&d, &op, &other, &[&bob]).await;
    assert!(is_refusal(&v, Refusal::NotAuthorised), "{s} {v}");
    let (s, v) = register(&d, &op, &other, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = tie(&d, &op, &other, &alice, "move her").await;
    assert!(is_refusal(&v, Refusal::NotAuthorised), "{s} {v}");

    // The operator's list.
    let (s, v) = d
        .call("GET", "/v1/ops/integrating-parties", Some(&op), &[], None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let acme = items.iter().find(|i| i["party"] == party).unwrap();
    let mut tied: Vec<String> = acme["principals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["principal"].as_str().unwrap().to_owned())
        .collect();
    tied.sort();
    let mut want = vec![alice.identity.clone(), bob.identity.clone()];
    want.sort();
    assert_eq!(tied, want);
    let (s, v) = d
        .call(
            "GET",
            "/v1/ops/integrating-parties",
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
}

/// T-009-16, `REQ-OP-10`, `ERR-ClientKeyReused`: the same key from two integrating parties is
/// two requests; from two principals of one party it is refused, never answered with the other's
/// reply; a principal tied to none is its own domain.
#[tokio::test(flavor = "multi_thread")]
async fn err_client_key_reused_the_same_key_from_two_principals_of_one_party() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 1_000).await;
    let carol = d.party("carol", 1_000).await;
    let dave = d.party("dave", 1_000).await;
    let x = organisation(&d, "x").await;
    let y = organisation(&d, "y").await;
    let op = d.operator.token.clone();
    assert_eq!(
        register(&d, &op, &x, &[&alice, &bob]).await.0,
        StatusCode::OK
    );
    assert_eq!(register(&d, &op, &y, &[&carol]).await.0, StatusCode::OK);

    let (s, a) = reserve(&d, &alice, "order-1", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{a}");
    // Bob, of the same party: refused, and not given alice's reservation.
    let (s, b) = reserve(&d, &bob, "order-1", "10").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert!(is_refusal(&b, Refusal::ClientKeyReused), "{b}");
    // Carol's party, and dave's own domain: requests of their own.
    let (s, c) = reserve(&d, &carol, "order-1", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{c}");
    let (s, e) = reserve(&d, &dave, "order-1", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{e}");
    let refs = [&a, &c, &e].map(|r| r["operation_reference"].as_str().unwrap().to_owned());
    assert!(refs[0] != refs[1] && refs[1] != refs[2] && refs[0] != refs[2]);
    // Alice's repeat is still hers.
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let (s, again) = reserve(&d, &alice, "order-1", "10").await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["operation_reference"], a["operation_reference"]);

    // The rows carry their domains: the party's for alice, carol's party's, dave's own.
    let rows = d.store.rows().await.unwrap();
    let domain_of = |who: &Party| {
        rows.iter()
            .filter(|r| r.new.kind == InputKind::Command)
            .filter(|r| r.new.client_key.as_deref() == Some("order-1"))
            .find(|r| r.new.caller.as_ref().unwrap().principal == who.identity)
            .and_then(|r| r.new.key_domain.clone())
    };
    assert_eq!(domain_of(&alice), Some(party_domain(&x)));
    assert_eq!(domain_of(&carol), Some(party_domain(&y)));
    assert_eq!(domain_of(&dave), Some(principal_domain(&dave.identity)));
    assert_eq!(domain_of(&bob), None, "bob's refused repeat logged nothing");
}

/// T-009-16, the late tie (`NOTES.md`): a key a principal used before it was tied stays in its
/// own domain, answered to it alone; the keys it uses after the tie are the party's.
#[tokio::test(flavor = "multi_thread")]
async fn req_op_10_a_key_used_before_a_tie_stays_in_the_principals_own_domain() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let dave = d.party("dave", 1_000).await;
    let x = organisation(&d, "x").await;
    let op = d.operator.token.clone();
    assert_eq!(register(&d, &op, &x, &[&alice]).await.0, StatusCode::OK);

    let (s, before) = reserve(&d, &dave, "early", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{before}");
    assert!(d.quiesce(Duration::from_secs(10)).await);
    assert_eq!(tie(&d, &op, &x, &dave, "joins x").await.0, StatusCode::OK);

    // Dave's repeat is answered from his own row; a changed request under it is refused.
    let (s, again) = reserve(&d, &dave, "early", "10").await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["operation_reference"], before["operation_reference"]);
    let (s, v) = reserve(&d, &dave, "early", "11").await;
    assert!(is_refusal(&v, Refusal::ClientKeyReused), "{s} {v}");
    // Alice's "early" was never in dave's domain: a request of her own.
    let (s, a) = reserve(&d, &alice, "early", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{a}");
    assert_ne!(a["operation_reference"], before["operation_reference"]);

    // After the tie, dave's new keys are the party's.
    let (s, late) = reserve(&d, &dave, "late", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{late}");
    let (s, v) = reserve(&d, &alice, "late", "10").await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(is_refusal(&v, Refusal::ClientKeyReused), "{v}");
    // Ingress kept the tie the core accepted.
    assert_eq!(
        d.store.tie_of(&dave.identity).await.unwrap(),
        Some(x.clone())
    );
}

/// `T-032-13` (stress finding F14): once the core has said a principal is tied to no party, its
/// commands no longer ask the core (they waited for the decision in flight); a tie made later is
/// still found, in the store the forwarder keeps it in before answering the tie (`REQ-ID-7`,
/// `REQ-OP-10`).
#[tokio::test(flavor = "multi_thread")]
async fn req_op_10_an_untied_principal_asks_the_core_once_and_a_later_tie_still_counts() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let dave = d.party("dave", 1_000).await;
    let x = organisation(&d, "x").await;
    let op = d.operator.token.clone();
    assert_eq!(register(&d, &op, &x, &[&alice]).await.0, StatusCode::OK);

    let (s, v) = reserve(&d, &dave, "one", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let (s, v) = reserve(&d, &dave, "two", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert!(
        ingress::service::party_of(&d.replica.shared, &dave.identity)
            .await
            .is_none()
    );
    assert!(d.quiesce(Duration::from_secs(10)).await);

    // The tie reaches the store with its decision; the cached "none" does not hide it.
    assert_eq!(tie(&d, &op, &x, &dave, "joins x").await.0, StatusCode::OK);
    assert_eq!(
        ingress::service::party_of(&d.replica.shared, &dave.identity).await,
        Some(x.clone())
    );
    let (s, v) = reserve(&d, &dave, "three", "10").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let (s, v) = reserve(&d, &alice, "three", "10").await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(is_refusal(&v, Refusal::ClientKeyReused), "{v}");
}
