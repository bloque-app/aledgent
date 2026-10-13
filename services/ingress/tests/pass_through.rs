// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Pass-through data at ingress (`REQ-PO-5`, amendment 0020 R1, `prq-08`, `T-009-21`): the
//! `reference` a command that submits an operation may carry, kept with the command's row and
//! echoed in its reply, the operation, each notification (inside the signed body) and its
//! movements, to principals of the sender's integrating party only.

use std::time::Duration;

use axum::http::StatusCode;
use ingress::inprocess::Journal;
use ingress::pass_through::Echo;
use ingress::store::{InputKind, Store};
use ingress::testkit::{Deployment, Party, USD, fast_config};
use ledger_core::Refusal;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(20);

fn is_refusal(v: &Value, r: Refusal) -> bool {
    *v == json!({ "error": r.code(), "description": r.description() })
}

/// What an integrating party sends: an order and a graph node, and the movement's own description.
fn order(n: u32) -> Value {
    json!({
        "order": format!("o-{n}"),
        "graph": { "id": "g-7", "node": "transfer_single" },
        "movement": { "type": "p2p", "description": "Lunch" },
    })
}

fn transfer(from: &Party, to: &Party, amount: &str, reference: Option<Value>) -> Value {
    let mut b = json!({ "legs": [{ "from": from.account, "to": to.account, "asset": USD, "amount": amount }] });
    if let Some(r) = reference {
        b["reference"] = r;
    }
    b
}

fn op_of(v: &Value) -> String {
    v["operation_reference"].as_str().unwrap().to_owned()
}

async fn command_rows(d: &Deployment) -> usize {
    d.store
        .rows()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.new.kind == InputKind::Command)
        .count()
}

/// The movements of `who`'s account that belong to `op`, as `who` reads them.
async fn movements_of(d: &Deployment, who: &Party, op: &str) -> Vec<Value> {
    let a = who.account.as_deref().unwrap();
    let (s, page) = d
        .call(
            "GET",
            &format!("/v1/movements?account={a}&limit=50"),
            Some(&who.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{page}");
    page["movements"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["operation"] == op)
        .cloned()
        .collect()
}

/// `AC-M8.1`, `AC-M8.4`, `REQ-PO-5`: a transfer's pass-through data comes back in its
/// acknowledgement, its decided reply, the operation, every notification of its transitions
/// (inside the body the recipient verified) and its movements. The payee, another principal,
/// reads its own movement of the same operation without it.
#[tokio::test(flavor = "multi_thread")]
async fn req_po_5_a_transfers_reference_comes_back_everywhere_its_sender_reads() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let body = transfer(&alice, &bob, "25", Some(order(1)));
    let key = [("client-key", "t-1")];

    let (s, ack) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &key,
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{ack}");
    assert_eq!(ack["state"], "Received");
    assert_eq!(ack["reference"], order(1), "the acknowledgement");
    let op = op_of(&ack);

    let (s, decided) = d
        .call_decided(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &key,
            Some(body),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{decided}");
    assert_eq!(decided["state"], "Settled");
    assert_eq!(decided["reference"], order(1), "the decided reply");

    let (s, got) = d
        .call(
            "GET",
            &format!("/v1/operations/{op}"),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{got}");
    assert_eq!(got["reference"], order(1), "the operation");

    // The notifications: each one the recipient verified carries it, so it is inside the signed
    // body.
    assert!(d.all_delivered(WAIT).await);
    let fresh = d.recipients.received(&alice.endpoint).fresh;
    let mine: Vec<_> = fresh
        .iter()
        .filter(|n| n.operation_reference == op)
        .collect();
    assert!(!mine.is_empty(), "alice is notified of her transfer");
    for n in &mine {
        let r = n
            .reference
            .as_ref()
            .expect("a notification carries the reference");
        assert_eq!(serde_json::from_str::<Value>(r.get()).unwrap(), order(1));
    }

    // The movements: alice's debit carries it; bob, the payee, reads his credit without it.
    let alices = movements_of(&d, &alice, &op).await;
    assert!(!alices.is_empty());
    for m in &alices {
        assert_eq!(m["reference"], order(1), "alice's movement: {m}");
    }
    let bobs = movements_of(&d, &bob, &op).await;
    assert!(!bobs.is_empty(), "bob sees his credit");
    for m in &bobs {
        assert!(
            m.get("reference").is_none(),
            "another party never sees it: {m}"
        );
    }
    // Nor may bob read the operation with it.
    let (_, theirs) = d
        .call(
            "GET",
            &format!("/v1/operations/{op}"),
            Some(&bob.token),
            &[],
            None,
        )
        .await;
    assert!(theirs.get("reference").is_none(), "{theirs}");
}

/// `AC-M8.2`, `REQ-OP-10`: the reference is part of the request. A repeat with the same one gets
/// the same reply; a repeat under the same key with another, or with none, is
/// `ERR-ClientKeyReused` and logs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn req_po_5_a_different_reference_under_one_key_is_another_request() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let key = [("client-key", "r-1")];
    let reserve = |reference: Option<Value>| {
        let mut b = json!({ "payer": alice.account, "asset": USD, "amount": "10" });
        if let Some(r) = reference {
            b["reference"] = r;
        }
        b
    };
    let (s, first) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &key,
            Some(reserve(Some(order(1)))),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{first}");
    let rows = command_rows(&d).await;
    let (s, again) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &key,
            Some(reserve(Some(order(1)))),
        )
        .await;
    assert!(s.is_success(), "{again}");
    assert_eq!(again["operation_reference"], first["operation_reference"]);
    assert_eq!(again["reference"], order(1));
    for other in [Some(order(2)), None] {
        let (s, r) = d
            .call(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &key,
                Some(reserve(other)),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "{r}");
        assert!(is_refusal(&r, Refusal::ClientKeyReused), "{r}");
    }
    assert_eq!(
        command_rows(&d).await,
        rows,
        "a refused repeat logs nothing"
    );
    // Without a client key, the derived key differs with the reference: two requests.
    let (s1, a) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[],
            Some(reserve(Some(order(3)))),
        )
        .await;
    let (s2, b) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[],
            Some(reserve(Some(order(4)))),
        )
        .await;
    assert_eq!((s1, s2), (StatusCode::ACCEPTED, StatusCode::ACCEPTED));
    assert_ne!(a["client_key"], b["client_key"]);
}

/// `AC-M8.3`, `REQ-ER-3`: 1 024 bytes are taken; 1 025, a body whose reference is not one JSON
/// value, and a reference on a command that submits no operation of its own are
/// `ERR-MalformedCommand`, and nothing is logged.
#[tokio::test(flavor = "multi_thread")]
async fn req_po_5_a_reference_over_1_kib_or_not_one_value_is_malformed() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    // A JSON string of n bytes, quotes included.
    let of_len = |n: usize| Value::String("x".repeat(n - 2));
    let (s, ok) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(transfer(&alice, &bob, "1", Some(of_len(1024)))),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{ok}");
    assert_eq!(ok["reference"], of_len(1024));
    let rows = command_rows(&d).await;

    let (s, r) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            Some(transfer(&alice, &bob, "1", Some(of_len(1025)))),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    assert!(is_refusal(&r, Refusal::MalformedCommand), "{r}");

    let raw = format!(
        r#"{{"legs":[{{"from":"{}","to":"{}","asset":"USD","amount":"1"}}],"reference":{{"order":}}}}"#,
        alice.account.as_deref().unwrap(),
        bob.account.as_deref().unwrap()
    );
    let (s, r) = d
        .call_raw(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[],
            raw.into_bytes(),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    assert!(is_refusal(&r, Refusal::MalformedCommand), "{r}");

    // grant_authority takes none: no field for it. (`open_identity` and `open_account` do, since
    // amendment 0025, for the identities principal's justification.)
    let (s, r) = d
        .call(
            "POST",
            "/v1/grants",
            Some(&d.operator.token),
            &[],
            Some(json!({
                "kind": "operator", "principal": alice.identity, "subject": "delegated_accounts",
                "ops": ["transfer"], "per_operation": "1", "reference": order(1),
            })),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    assert!(is_refusal(&r, Refusal::MalformedCommand), "{r}");
    assert_eq!(command_rows(&d).await, rows, "nothing refused is logged");
}

/// `INV-14`, `REQ-PO-5`: a capture and a release act on the reservation's operation, so their
/// replies carry the reservation's reference; they take none of their own.
#[tokio::test(flavor = "multi_thread")]
async fn req_po_5_capture_and_release_carry_the_reservations_reference() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let mut ops = Vec::new();
    for (n, key) in [(1, "r-capture"), (2, "r-release")] {
        let (s, r) = d
            .call_decided(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[("client-key", key)],
                Some(json!({ "payer": alice.account, "asset": USD, "amount": "7", "reference": order(n) })),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{r}");
        assert_eq!(r["state"], "Reserved");
        ops.push(op_of(&r));
    }
    let capture = format!("/v1/reservations/{}/capture", ops[0]);
    let (s, r) = d
        .call(
            "POST",
            &capture,
            Some(&alice.token),
            &[],
            Some(json!({ "destination": bob.account, "cleared": "7", "reference": order(9) })),
        )
        .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "a capture takes none of its own: {r}"
    );
    let (s, c) = d
        .call(
            "POST",
            &capture,
            Some(&alice.token),
            &[],
            Some(json!({ "destination": bob.account, "cleared": "7" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["state"], "Settled");
    assert_eq!(c["reference"], order(1), "the capture's reply: {c}");
    let (s, rel) = d
        .call(
            "POST",
            &format!("/v1/reservations/{}/release", ops[1]),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{rel}");
    assert_eq!(rel["reference"], order(2), "the release's reply: {rel}");
    // The settled reservation's movements carry the reservation's reference too.
    for m in movements_of(&d, &alice, &ops[0]).await {
        assert_eq!(m["reference"], order(1), "{m}");
    }
}

/// `REQ-PO-5`, `REQ-AR-7`: the reference is kept with the command's row, committed before the
/// acknowledgement. Taken in while the core is down, it survives the loss of the ingress replica
/// and the core's replay from its journal: the operation, the notifications and a repeat carry it.
/// The core never received it.
#[tokio::test(flavor = "multi_thread")]
async fn req_po_5_a_reference_survives_a_restart_and_never_reaches_the_core() {
    let mut d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let marker = json!({ "order": "o-restart-4f1c", "graph": "g-restart" });
    let body = transfer(&alice, &bob, "3", Some(marker.clone()));
    let key = [("client-key", "t-restart")];
    d.core.stop();
    let (s, ack) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &key,
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{ack}");
    assert_eq!(ack["reference"], marker);
    let op = op_of(&ack);

    d.replica.kill();
    d.core.start().unwrap();
    let (replica, surface) = d.another_replica(fast_config("ingress-after"));
    d.replica = replica;
    d.router = surface;
    assert!(d.quiesce(WAIT).await);

    let (s, got) = d
        .call(
            "GET",
            &format!("/v1/operations/{op}"),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{got}");
    assert_eq!(got["state"], "Settled");
    assert_eq!(got["reference"], marker);
    let (s, again) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &key,
            Some(body),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["reference"], marker);
    assert!(d.all_delivered(WAIT).await);
    let notified: Vec<_> = d
        .recipients
        .received(&alice.endpoint)
        .fresh
        .into_iter()
        .filter(|n| n.operation_reference == op)
        .collect();
    assert!(!notified.is_empty());
    assert!(notified.iter().all(|n| {
        n.reference
            .as_ref()
            .is_some_and(|r| serde_json::from_str::<Value>(r.get()).unwrap() == marker)
    }));

    // The core's journal, everything it was given, holds none of it (`REQ-PO-4`).
    let journal = serde_json::to_string(&d.journal.entries().unwrap()).unwrap();
    assert!(
        !journal.contains("o-restart-4f1c"),
        "the core never receives it"
    );
    // The row keeps it, exactly as sent.
    let row = d
        .store
        .rows()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.new.client_key.as_deref() == Some("t-restart"))
        .unwrap();
    let kept = row.new.pass_through.unwrap();
    assert_eq!(serde_json::from_str::<Value>(kept.get()).unwrap(), marker);
    assert!(!String::from_utf8_lossy(&row.new.body).contains("o-restart"));
}

/// `REQ-PO-5`, `REQ-ID-7`: who may see a principal's reference. The principal itself; another
/// principal of its integrating party; nobody tied to another party, or to none.
#[tokio::test(flavor = "multi_thread")]
async fn req_po_5_a_reference_is_shown_within_the_senders_party_only() {
    let d = Deployment::start().await;
    let shared = &d.replica.shared;
    for (principal, party) in [("did:x:alice", "did:x:acme"), ("did:x:bob", "did:x:acme")] {
        d.store.put_tie(principal, party).await.unwrap();
    }
    d.store.put_tie("did:x:carol", "did:x:other").await.unwrap();
    let shown = |reader: &'static str, sender: &'static str| async move {
        Echo::new(shared, reader).shown(sender).await
    };
    assert!(shown("did:x:alice", "did:x:alice").await);
    assert!(shown("did:x:bob", "did:x:alice").await, "of the same party");
    assert!(
        !shown("did:x:carol", "did:x:alice").await,
        "of another party"
    );
    // An untied principal is its own party: only it sees its data.
    assert!(shown("did:aledgent:id:dave", "did:aledgent:id:dave").await);
    assert!(!shown("did:x:alice", "did:aledgent:id:dave").await);
}
