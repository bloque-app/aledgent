// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The command surface through the router (`T-009-01`, `T-009-02`, `T-009-04`, `T-009-05`,
//! `T-009-09`): deny-by-default authentication, no credential in the log, client-key dedup, the
//! `Received` acknowledgement, the synchronous reservation path, the refusal shape against the
//! OpenAPI document, and query routing.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use ingress::boundary::{Core, CoreInput, InputBody, InputKey};
use ingress::inprocess::Journal;
use ingress::openapi;
use ingress::store::{InputKind, Status, Store};
use ingress::surface::{Handler, ROUTES, refusal};
use ingress::testkit::{Deployment, USD};
use ledger_core::Refusal;
use serde_json::{Value, json};

fn path_of(template: &str) -> String {
    template
        .replace("{reference}", "did:aledgent:op:0000000000000000000000000a")
        .replace("{account}", "did:aledgent:acct:0000000000000000000000000a")
        .replace("{grant}", "did:aledgent:grant:0000000000000000000000000a")
        .replace("{asset}", "USD")
}

async fn commands(d: &Deployment) -> usize {
    d.store
        .rows()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.new.kind == InputKind::Command)
        .count()
}

fn is_refusal(v: &Value, r: Refusal) -> bool {
    *v == json!({ "error": r.code(), "description": r.description() })
}

/// `NFR-7`: every route, an unknown one and a wrong method included, refuses a caller with no
/// credential or an unknown one, with `ERR-CredentialInvalid`.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_7_every_route_refuses_the_unauthenticated() {
    let d = Deployment::start().await;
    let mut routes: Vec<(String, String)> = ROUTES
        .iter()
        .map(|r| (r.method.to_owned(), path_of(r.path)))
        .collect();
    routes.push(("GET".into(), "/v1/no-such-route".into()));
    routes.push(("DELETE".into(), "/v1/transfers".into()));
    for (method, path) in &routes {
        for token in [None, Some("not-a-token")] {
            let (s, v) = d.call(method, path, token, &[], Some(json!({}))).await;
            assert_eq!(
                s,
                StatusCode::UNAUTHORIZED,
                "{method} {path} with {token:?}"
            );
            assert!(
                is_refusal(&v, Refusal::CredentialInvalid),
                "{method} {path}: {v}"
            );
        }
    }
    // Nothing unauthenticated reached the log.
    assert!(d.store.rows().await.unwrap().iter().all(|r| {
        r.new.kind != InputKind::Command
            || r.new
                .caller
                .as_ref()
                .is_some_and(|c| c.principal == d.operator.identity)
    }));
}

/// `AC-A1.3` at the surface, `REQ-OP-10`: a repeat with the same client key and body returns the
/// stored reply; a changed body is `ERR-ClientKeyReused`; another principal's same key is its own.
#[tokio::test(flavor = "multi_thread")]
async fn ac_a1_3_a_repeat_returns_the_stored_reply_and_a_changed_body_is_refused() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 1_000).await;
    let body = |amount: &str| json!({ "payer": alice.account, "asset": USD, "amount": amount });
    let key = [("client-key", "order-1")];
    let (s1, r1) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &key,
            Some(body("10")),
        )
        .await;
    // Acknowledged at `Received` (amendment 0007, `OQ-61` as ruled on 2026-09-29).
    assert_eq!(s1, StatusCode::ACCEPTED, "{r1}");
    assert_eq!(r1["state"], "Received");
    assert_eq!(r1["terminal"], false);
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let (s2, r2) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &key,
            Some(body("010")),
        )
        .await;
    // The same request, spelt differently, is a repeat: once decided, it gets the decision.
    assert_eq!(s2, StatusCode::OK, "{r2}");
    assert_eq!(r2["operation_reference"], r1["operation_reference"]);
    assert_eq!(r2["client_key"], r1["client_key"]);
    assert_eq!(r2["state"], "Reserved");
    assert_eq!(r2["terminal"], false);
    let (s3, r3) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &key,
            Some(body("11")),
        )
        .await;
    assert_eq!(s3, StatusCode::CONFLICT);
    assert!(is_refusal(&r3, Refusal::ClientKeyReused), "{r3}");
    // Bob's "order-1" is Bob's own.
    let (s4, r4) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&bob.token),
            &key,
            Some(json!({ "payer": bob.account, "asset": USD, "amount": "10" })),
        )
        .await;
    assert_eq!(s4, StatusCode::ACCEPTED, "{r4}");
    assert_ne!(r4["operation_reference"], r1["operation_reference"]);
    // One row each: the repeat appended nothing.
    let commands = d
        .store
        .rows()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.new.client_key.as_deref() == Some("order-1"))
        .count();
    assert_eq!(commands, 2);
}

/// `REQ-OP-11`, `REQ-RT-2`: without a client key, one is derived from the request, returned, and
/// the same on every retry; the retry gets the same reply.
#[tokio::test(flavor = "multi_thread")]
async fn req_op_11_a_derived_key_is_returned_and_the_same_on_every_retry() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let t = json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "7" }] });
    let (s1, r1) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(t.clone()),
        )
        .await;
    assert_eq!(s1, StatusCode::ACCEPTED, "{r1}");
    assert_eq!(r1["client_key_derived"], true);
    let key = r1["client_key"].as_str().unwrap().to_owned();
    assert_eq!(key.len(), 64);
    let (_, r2) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(t.clone()),
        )
        .await;
    assert_eq!(r2["client_key"], key);
    assert_eq!(r2["operation_reference"], r1["operation_reference"]);
    // Supplying the returned key names the same request.
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let (s3, r3) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[("client-key", &key)],
            Some(t),
        )
        .await;
    assert_eq!(s3, StatusCode::OK, "{r3}");
    assert_eq!(r3["operation_reference"], r1["operation_reference"]);
    assert_eq!(r3["state"], "Settled");
}

/// `REQ-CS-2`, `REQ-CS-6`, `REQ-OP-7`: a transfer is acknowledged at `Received`, labelled
/// non-terminal, before the core decides it; `operation(reference)` then shows the decision.
#[tokio::test(flavor = "multi_thread")]
async fn req_cs_2_a_transfer_is_acknowledged_at_received_and_labelled_non_terminal() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    d.core.stop();
    let (s, r) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "3" }] })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    assert_eq!(r["state"], "Received");
    assert_eq!(r["terminal"], false);
    assert_eq!(r["class"], "open-non-refunding");
    let reference = r["operation_reference"].as_str().unwrap().to_owned();
    // While the core is down, the query answers from the log: still Received, non-terminal.
    let path = format!("/v1/operations/{reference}");
    let (s, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{q}");
    assert_eq!(
        (q["state"].clone(), q["terminal"].clone()),
        (json!("Received"), json!(false))
    );
    d.core.start().unwrap();
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let (s, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{q}");
    assert_eq!(q["state"], "Settled");
    assert_eq!(q["terminal"], true);
    // Bob, whose account it credited, sees it; Carol, who has nothing to do with it, does not
    // (F-011's rule, INV-16).
    let (s, _) = d.call("GET", &path, Some(&bob.token), &[], None).await;
    assert_eq!(s, StatusCode::OK);
    let carol = d.party("carol", 0).await;
    let (s, q) = d.call("GET", &path, Some(&carol.token), &[], None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(is_refusal(&q, Refusal::UnknownOperation));
}

/// `CTR-OPS` through ingress (`F-013`'s semantics, routed): operator commands take their client
/// keys in a domain of their own (`DEP-10`), need their justification, and are refused to anyone
/// without the capability. A frozen account's transfer is rejected with its condition.
#[tokio::test(flavor = "multi_thread")]
async fn operator_commands_are_routed_on_their_own_surface() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 100).await;
    let bob = d.party("bob", 0).await;
    let op = Some(d.operator.token.as_str());
    let a = alice.account.clone().unwrap();
    let (s, g) = d
        .call(
            "POST",
            "/v1/ops/operator-grants",
            op,
            &[("client-key", "same-key")],
            Some(json!({
                "principal": d.operator.identity,
                "kinds": ["freeze_account", "unfreeze_account"],
                "justification": "runbook 7",
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert!(g["result"]["grant"].is_string());
    // The same client key on the command surface is another request's, not a reuse.
    let (s, r) = d
        .call(
            "POST",
            "/v1/identities",
            op,
            &[("client-key", "same-key")],
            Some(json!({ "kind": "person", "verifier": "0b" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let freeze = format!("/v1/ops/accounts/{a}/freeze");
    let (s, r) = d
        .call(
            "POST",
            &freeze,
            op,
            &[],
            Some(json!({ "justification": "" })),
        )
        .await;
    assert!(is_refusal(&r, Refusal::JustificationRequired), "{s} {r}");
    let (s, r) = d
        .call(
            "POST",
            &freeze,
            Some(&alice.token),
            &[],
            Some(json!({ "justification": "mine" })),
        )
        .await;
    assert!(is_refusal(&r, Refusal::NotAuthorised), "{s} {r}");
    let (s, r) = d
        .call(
            "POST",
            &freeze,
            op,
            &[],
            Some(json!({ "justification": "incident 42" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, t) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(
                json!({ "legs": [{ "from": a, "to": bob.account, "asset": USD, "amount": "1" }] }),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let path = format!(
        "/v1/operations/{}",
        t["operation_reference"].as_str().unwrap()
    );
    let (_, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
    assert_eq!(q["state"], "Rejected");
    assert!(is_refusal(&q["error"], Refusal::AccountFrozen), "{q}");
    let (s, r) = d
        .call(
            "POST",
            &format!("/v1/ops/accounts/{a}/unfreeze"),
            op,
            &[],
            Some(json!({ "justification": "incident 42 closed" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
}

/// `T-009-09`, `REQ-QR-1`: `F-011`'s queries through ingress — balances, movements with a
/// cursor, a statement, the registry, `can_spend` and the operator's trial balance — each
/// refusing what `F-011` refuses.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_09_the_queries_of_f_011_reach_the_core_through_ingress() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    for i in 0..3 {
        let (s, _) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&alice.token),
                &[("client-key", &format!("q{i}"))],
                Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "10" }] })),
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED);
    }
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let a = alice.account.clone().unwrap();
    let t = Some(alice.token.as_str());
    let (s, b) = d
        .call(
            "GET",
            &format!("/v1/balances?account={a}&asset=USD"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["rows"][0]["available"], "970");
    assert_eq!(b["rows"][0]["total"], "970");
    // Movements, two at a time, along the cursor.
    let (s, p1) = d
        .call(
            "GET",
            &format!("/v1/movements?account={a}&limit=2"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p1}");
    assert_eq!(p1["movements"].as_array().unwrap().len(), 2);
    let next = p1["next"].as_str().unwrap();
    let (s, p2) = d
        .call(
            "GET",
            &format!("/v1/movements?account={a}&limit=2&cursor={next}"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p2}");
    assert!(!p2["movements"].as_array().unwrap().is_empty());
    // An unbounded page is refused.
    let (s, u) = d
        .call("GET", &format!("/v1/movements?account={a}"), t, &[], None)
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(is_refusal(&u, Refusal::QueryUnbounded), "{u}");
    // Both subjects, or none, are refused the same way.
    let (s, u) = d
        .call(
            "GET",
            &format!("/v1/movements?account={a}&identity={a}&limit=2"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(is_refusal(&u, Refusal::QueryUnbounded), "{u}");
    let (s, u) = d.call("GET", "/v1/movements?limit=2", t, &[], None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(is_refusal(&u, Refusal::QueryUnbounded), "{u}");
    // A parameter the route does not know is refused, not ignored.
    let (s, u) = d
        .call(
            "GET",
            &format!("/v1/movements?account={a}&limit=2&acount={a}"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(is_refusal(&u, Refusal::NotAuthorised), "{u}");
    let (s, u) = d
        .call(
            "GET",
            &format!("/v1/balances?account={a}&since=0"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(is_refusal(&u, Refusal::NotAuthorised), "{u}");
    // A statement refuses an unknown parameter as unbounded.
    let (s, st) = d
        .call(
            "GET",
            &format!("/v1/statements?account={a}&asset=USD&from=0&to=9999999999999&acount={a}"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(is_refusal(&st, Refusal::QueryUnbounded), "{st}");
    let (s, st) = d
        .call(
            "GET",
            &format!("/v1/statements?account={a}&asset=USD&from=0&to=9999999999999"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{st}");
    assert_eq!(st["closing"]["available"], "970");
    let (s, st) = d
        .call(
            "GET",
            &format!("/v1/statements?account={a}&asset=USD"),
            t,
            &[],
            None,
        )
        .await;
    assert!(is_refusal(&st, Refusal::QueryUnbounded), "{s} {st}");
    let (s, assets) = d.call("GET", "/v1/assets", t, &[], None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(assets.as_array().unwrap().iter().any(|x| x["id"] == USD));
    let (s, ok) = d
        .call(
            "GET",
            &format!("/v1/accounts/{a}/can-spend?asset=USD&amount=970"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!((s, ok["ok"].clone()), (StatusCode::OK, json!(true)));
    let (s, no) = d
        .call(
            "GET",
            &format!("/v1/accounts/{a}/can-spend?asset=USD&amount=971"),
            t,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(is_refusal(&no, Refusal::InsufficientAvailable), "{no}");
    // Bob cannot read Alice's balances (INV-16); the trial balance is the operator's.
    let (s, _) = d
        .call(
            "GET",
            &format!("/v1/balances?account={a}"),
            Some(&bob.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = d.call("GET", "/v1/trial-balance", t, &[], None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, tb) = d
        .call(
            "GET",
            "/v1/trial-balance",
            Some(&d.operator.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{tb}");
    assert!(tb["unbalanced"].as_array().unwrap().is_empty());
}

/// A transfer the core refuses after the acknowledgement: `Received → Rejected`, the refusal
/// named on the operation and in the repeat's reply.
#[tokio::test(flavor = "multi_thread")]
async fn a_received_transfer_refused_by_the_core_is_rejected_with_its_condition() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 10).await;
    let bob = d.party("bob", 0).await;
    let t = json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "11" }] });
    let (s, r) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(t.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let path = format!(
        "/v1/operations/{}",
        r["operation_reference"].as_str().unwrap()
    );
    let (_, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
    assert_eq!(q["state"], "Rejected");
    assert!(
        is_refusal(&q["error"], Refusal::InsufficientAvailable),
        "{q}"
    );
    let (s, r2) = d
        .call("POST", "/v1/transfers", Some(&alice.token), &[], Some(t))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(is_refusal(&r2, Refusal::InsufficientAvailable));
}

/// `AC-C1.1` through ingress, `REQ-CS-3` (amendment 0007, `OQ-61` as ruled on 2026-09-29): a
/// reservation is acknowledged at `Received` at once, even while the core is down, and its
/// decision follows by query. A refusal is recorded as `Rejected` with its condition, and a repeat
/// gets the same refusal.
#[tokio::test(flavor = "multi_thread")]
async fn ac_c1_1_a_reservation_is_acknowledged_and_its_decision_follows() {
    let d = Arc::new(Deployment::start().await);
    let alice = d.party("alice", 1_000).await;
    d.core.stop();
    let before = d.journal.len();
    let body = json!({ "payer": alice.account, "asset": USD, "amount": "100" });
    let (s, r) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[],
            Some(body),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    assert_eq!(r["state"], "Received");
    assert_eq!(r["terminal"], false);
    assert_eq!(d.journal.len(), before, "acknowledged, not decided");
    let path = format!(
        "/v1/operations/{}",
        r["operation_reference"].as_str().unwrap()
    );
    let (_, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
    assert_eq!(q["state"], "Received");
    d.core.start().unwrap();
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let (_, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
    assert_eq!(q["state"], "Reserved", "{q}");
    assert!(d.journal.len() > before, "the decision is durable");
    // The reserved balance is taken (REQ-BA-4): the rest is available, no more.
    let over = json!({ "payer": alice.account, "asset": USD, "amount": "901" });
    let (s, r) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[],
            Some(over.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let path = format!(
        "/v1/operations/{}",
        r["operation_reference"].as_str().unwrap()
    );
    let (_, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
    assert_eq!(q["state"], "Rejected", "{q}");
    assert!(
        is_refusal(&q["error"], Refusal::InsufficientAvailable),
        "{q}"
    );
    let (s, r) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[],
            Some(over),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(is_refusal(&r, Refusal::InsufficientAvailable), "{r}");
    let latency = d.replica.shared.metrics.latency();
    assert!(latency.samples >= 2);
    println!(
        "NFR-1 reservation acknowledgement: p50 {} us, p99 {} us",
        latency.p50_us, latency.p99_us
    );
}

/// `T-009-04`: reservations go to the front of the forwarder's queue.
#[tokio::test(flavor = "multi_thread")]
async fn reservations_are_forwarded_before_earlier_transfers() {
    let d = Arc::new(Deployment::start().await);
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    d.core.stop();
    for i in 0..5 {
        let (s, _) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&alice.token),
                &[("client-key", &format!("t{i}"))],
                Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "1" }] })),
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED);
    }
    let reserve = {
        let d = Arc::clone(&d);
        let body = json!({ "payer": alice.account, "asset": USD, "amount": "1" });
        let token = alice.token.clone();
        tokio::spawn(async move {
            d.call("POST", "/v1/reservations", Some(&token), &[], Some(body))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    d.core.start().unwrap();
    let (s, _) = reserve.await.unwrap();
    assert_eq!(s, StatusCode::ACCEPTED);
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let rows = d.store.rows().await.unwrap();
    let seq_of = |pred: &dyn Fn(&ingress::store::Row) -> bool| {
        rows.iter()
            .filter(|r| pred(r))
            .filter_map(|r| r.core_input_seq)
            .min()
            .unwrap()
    };
    let reserve_seq = seq_of(&|r| r.new.reserves);
    // `t0` may be in flight when the reservation arrives; it jumps every transfer still queued.
    let second_transfer = seq_of(&|r| r.new.client_key.as_deref() == Some("t1"));
    assert!(reserve_seq < second_transfer, "the reservation went first");
}

/// `REQ-CS-10`, `notes/FIT-VOS-CIPHERCLERK.md`: a command over the core's 16 KiB invocation limit is refused
/// before it is logged — a unit of legs with `ERR-BatchTooLarge` — and a body that is not a
/// command at all is `ERR-MalformedCommand` (amendment 0021, `REQ-ER-3`).
#[tokio::test(flavor = "multi_thread")]
async fn a_command_over_the_invocation_limit_is_refused_before_it_is_logged() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let before = commands(&d).await;
    let leg = json!({ "from": alice.account, "to": bob.account, "asset": USD, "amount": "1" });
    let legs: Vec<Value> = std::iter::repeat_n(leg, 200).collect();
    let (s, r) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(json!({ "legs": legs })),
        )
        .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(is_refusal(&r, Refusal::BatchTooLarge), "{r}");
    let (s, r) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(json!({ "nonsense": 1 })),
        )
        .await;
    // A shape fault (amendment 0021, `REQ-ER-3`).
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(is_refusal(&r, Refusal::MalformedCommand), "{r}");
    assert_eq!(
        commands(&d).await,
        before,
        "nothing refused here was logged"
    );
}

/// `REQ-CS-4`, `REQ-ER-1`, `T-009-05`: every refusal the surface can send has the status and the
/// body the OpenAPI document publishes for it. A difference fails the build.
#[tokio::test]
async fn req_cs_4_every_refusal_matches_its_openapi_description() {
    let doc = openapi::document();
    let alternatives = doc["components"]["schemas"]["Refusal"]["oneOf"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(alternatives.len(), Refusal::ALL.len());
    for r in Refusal::ALL {
        let published = alternatives
            .iter()
            .find(|a| a["properties"]["error"]["const"] == r.code())
            .unwrap_or_else(|| panic!("{} is not published", r.code()));
        let resp = refusal(r);
        let status = resp.status();
        // Every refusal is a 4xx but the two of amendment 0006 item 8, which are the server's:
        // `ERR-DecisionUnavailable` (503, retry with the same client key) and `ERR-Internal`.
        // `DEC-27` (g), `T-038-23`: the 503 says when to retry; nothing else does.
        let retry = resp.headers().get(axum::http::header::RETRY_AFTER).cloned();
        match r {
            Refusal::DecisionUnavailable => {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(
                    retry.as_ref().and_then(|v| v.to_str().ok()),
                    Some(
                        ingress::surface::DECISION_RETRY_AFTER_SECS
                            .to_string()
                            .as_str()
                    )
                );
            }
            Refusal::Internal => assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR),
            _ => assert!(status.is_client_error(), "{}: a refusal is a 4xx", r.code()),
        }
        if r != Refusal::DecisionUnavailable {
            assert!(retry.is_none(), "{}: no Retry-After", r.code());
        }
        assert_eq!(
            u64::from(status.as_u16()),
            published["x-http-status"].as_u64().unwrap(),
            "{}",
            r.code()
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let obj = body.as_object().unwrap();
        assert_eq!(obj.len(), 2, "{}: only error and description", r.code());
        assert_eq!(body["error"], published["properties"]["error"]["const"]);
        assert_eq!(
            body["description"],
            published["properties"]["description"]["const"]
        );
    }
    // Every route answers refusals with the published schema.
    for route in ROUTES {
        let op = &doc["paths"][route.path][route.method.to_ascii_lowercase()];
        for class in ["4XX", "5XX"] {
            assert_eq!(
                op["responses"][class]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/Refusal",
                "{} {class}",
                route.path
            );
        }
        assert_eq!(
            op["responses"]["5XX"]["headers"]["Retry-After"]["$ref"],
            "#/components/headers/RetryAfter",
            "{}: the 503 declares Retry-After",
            route.path
        );
        if let Handler::Command(_) = route.handler {
            assert!(op["requestBody"].is_object(), "{}", route.path);
        }
    }
}

/// `T-009-01`: the OpenAPI file in the repository is the one the code generates. CI builds it
/// here; `UPDATE_OPENAPI=1` rewrites it.
#[test]
fn the_committed_openapi_document_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi.json");
    let generated = openapi::document_text();
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(&path, &generated).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "services/ingress/openapi.json is stale: run `UPDATE_OPENAPI=1 cargo test -p ingress --test surface`"
    );
    let doc: Value = serde_json::from_str(&committed).unwrap();
    assert_eq!(doc["openapi"], openapi::OPENAPI_VERSION);
}

/// `INV-18`: an input forwarded twice changes nothing the second time, and gets its first input
/// number back.
#[tokio::test(flavor = "multi_thread")]
async fn inv_18_a_forwarded_input_sent_twice_changes_nothing() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let rows = d.store.rows().await.unwrap();
    let issue = rows
        .iter()
        .rev()
        .find(|r| r.new.kind == InputKind::Command && r.status == Status::Decided)
        .unwrap();
    let input = ingress::service::input_of(issue).unwrap();
    // The ticker and the delivery worker keep journalling their own inputs in the background,
    // so count the entries of the input under test, not the journal's length (#56).
    let journalled = |key: &InputKey| {
        d.journal
            .entries()
            .unwrap()
            .iter()
            .filter(|(_, i)| &i.key == key)
            .count()
    };
    assert_eq!(journalled(&input.key), 1);
    let root = d.core.with_ledger(|l| l.book().root().unwrap()).unwrap();
    let again = d.core.apply(&input).await.unwrap();
    assert_eq!(Some(again.input_seq), issue.core_input_seq);
    assert_eq!(journalled(&input.key), 1);
    assert_eq!(
        d.core.with_ledger(|l| l.book().root().unwrap()).unwrap(),
        root
    );
    // A tick sent twice, too.
    let tick = CoreInput {
        key: InputKey([0x77; 16]),
        body: InputBody::Tick { now: 5 },
    };
    let first = d.core.apply(&tick).await.unwrap();
    assert_eq!(d.core.apply(&tick).await.unwrap(), first);
    assert_eq!(journalled(&tick.key), 1);
    let _ = alice;
}

/// `T-009-09`, `REQ-QR-3`, `REQ-PV-5`: a query reaches the core with the principal attributed,
/// and writes nothing to the received-log.
#[tokio::test(flavor = "multi_thread")]
async fn queries_are_attributed_and_write_nothing_to_the_log() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let rows = commands(&d).await;
    let (s, r) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[],
            Some(json!({ "payer": alice.account, "asset": USD, "amount": "1" })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let path = format!(
        "/v1/operations/{}",
        r["operation_reference"].as_str().unwrap()
    );
    for _ in 0..3 {
        let (s, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        assert_eq!(q["state"], "Reserved");
    }
    let (s, _) = d
        .call(
            "GET",
            "/v1/notifications/undelivered",
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "the operator's alone");
    let (s, u) = d
        .call(
            "GET",
            "/v1/notifications/undelivered",
            Some(&d.operator.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!(
        commands(&d).await,
        rows + 1,
        "only the reservation was logged"
    );
    let (s, m) = d
        .call("GET", "/metrics", Some(&alice.token), &[], None)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        m.as_str()
            .unwrap()
            .contains("aledgent_ingress_oldest_undecided_age_ms")
    );
}

/// `NFR-4`, `REQ-CS-7` (`T-032-14`, stress finding F16): a command acknowledged at `Received` is
/// readable while undecided, not only by the principal that sent it but by every principal that
/// would see its operation once decided through an account it pays from: the operator acting for
/// a holder reads the holder's reservation, and the holder reads the one the operator made for
/// it. Before, both were told `ERR-UnknownOperation`, which the stress suite's loss check counted
/// as lost. A principal that reaches no payer is still told `ERR-UnknownOperation` (`INV-16`).
#[tokio::test(flavor = "multi_thread")]
async fn nfr_4_a_received_operation_is_readable_by_whoever_reaches_its_payer() {
    let mut d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let carol = d.party("carol", 0).await;
    let op = d.operator.token.clone();
    // The operator acts on accounts in delegated custody, as an issuing client does (the stress world's grant).
    let (s, v) = d
        .call(
            "POST",
            "/v1/grants",
            Some(&op),
            &[],
            Some(json!({
                "kind": "operator",
                "principal": d.operator.identity,
                "subject": "delegated_accounts",
                "ops": ["transfer", "reserve", "capture", "release"],
                "per_window": { "length": 86_400_000u64, "limit": "1000000000" },
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(d.quiesce(Duration::from_secs(10)).await);
    // Nothing is forwarded from now on: what is logged stays `Received`; the core still reads.
    d.replica.kill();
    let reserve = |token: String| {
        let d = &d;
        let body = json!({ "payer": alice.account, "asset": USD, "amount": "5" });
        async move {
            let (s, r) = d
                .call("POST", "/v1/reservations", Some(&token), &[], Some(body))
                .await;
            assert_eq!(s, StatusCode::ACCEPTED, "{r}");
            assert_eq!(r["state"], "Received");
            format!(
                "/v1/operations/{}",
                r["operation_reference"].as_str().unwrap()
            )
        }
    };
    let by_alice = reserve(alice.token.clone()).await;
    let by_operator = reserve(op.clone()).await;
    for (path, reader) in [
        (&by_alice, &alice.token),
        (&by_alice, &op),
        (&by_operator, &op),
        (&by_operator, &alice.token),
    ] {
        let (s, q) = d.call("GET", path, Some(reader), &[], None).await;
        assert_eq!(s, StatusCode::OK, "{path}: {q}");
        assert_eq!(q["state"], "Received", "{path}: {q}");
    }
    for path in [&by_alice, &by_operator] {
        let (s, q) = d.call("GET", path, Some(&carol.token), &[], None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{q}");
        assert!(is_refusal(&q, Refusal::UnknownOperation), "{q}");
    }
}
