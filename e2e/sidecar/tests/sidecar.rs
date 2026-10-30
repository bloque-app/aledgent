// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The sidecar as a CI job uses it: seeded at start, then a party, its account, an issuance and
//! the balance, through the surface; and the health check over a real socket.

use std::time::{Duration, Instant};

use aledgent_sidecar::{
    DEFAULT_ASSETS, DEFAULT_PRINCIPALS, GENESIS_OP, Options, PER_DAY_LIMIT, PrincipalRole,
    PrincipalSpec, RecipientSpec, Sidecar, SystemAccountSpec, TEST_OPERATOR_TOKEN, health,
    load_principals, parse_assets, parse_max_legs, parse_principals, parse_recipients,
    parse_system_accounts, parse_tokens, probe_address, test_principal_token,
};
use axum::http::StatusCode;
use ingress::config::GenesisConfig;
use serde_json::{Value, json};

async fn started() -> Sidecar {
    Sidecar::start(parse_assets(DEFAULT_ASSETS).expect("the default registry parses"))
        .await
        .expect("the sidecar starts")
}

/// Repeat a command until it is decided: a transfer or a reservation is acknowledged at
/// `Received` (`REQ-CS-3`); everything else answers with its decision.
async fn decided(s: &Sidecar, path: &str, key: &str, body: Value) -> (StatusCode, Value) {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let r = s
            .call("POST", path, &[("client-key", key)], Some(body.clone()))
            .await;
        if r.0 != StatusCode::ACCEPTED || Instant::now() >= until {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn req_as_2_the_default_registry_is_copm_tsta_tstb_tstc() {
    let a = parse_assets(DEFAULT_ASSETS).unwrap();
    let got: Vec<(String, u8)> = a.iter().map(|a| (a.id.clone(), a.precision)).collect();
    assert_eq!(
        got,
        vec![
            ("COPM".into(), 2),
            ("TSTA".into(), 2),
            ("TSTB".into(), 2),
            ("TSTC".into(), 6)
        ]
    );
    assert!(parse_assets("COPM/2,copm/2").is_err(), "listed twice");
    assert!(parse_assets("COPM").is_err(), "no precision");
    assert!(parse_assets("").is_err(), "empty");
}

#[tokio::test(flavor = "multi_thread")]
async fn req_as_2_founded_identifiers_are_the_same_on_every_start() {
    let a = started().await;
    let b = started().await;
    assert_eq!(a.describe()["operator"], b.describe()["operator"]);
    assert_eq!(a.describe()["issuance"], b.describe()["issuance"]);
    // And what `aledgent-ingress founded` would print for the same genesis reference.
    let g = GenesisConfig {
        genesis_op: hex::encode(GENESIS_OP),
        ..aledgent_sidecar::genesis_config(vec![], 0)
    };
    assert_eq!(
        a.describe()["operator"],
        g.founded().unwrap().operator.to_did()
    );
}

/// Open a person and a customer account in delegated custody for it, as the operator.
async fn open(s: &Sidecar, name: &str) -> String {
    let (st, v) = s
        .call(
            "POST",
            "/v1/identities",
            &[("client-key", &format!("identity-{name}"))],
            Some(json!({ "kind": "person", "verifier": "0a" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "open_identity: {v}");
    let identity = v["result"]["identity"].as_str().unwrap().to_owned();
    let (st, v) = s
        .call(
            "POST",
            "/v1/accounts",
            &[("client-key", &format!("account-{name}"))],
            Some(json!({ "owner": identity, "kind": "customer", "custody": "delegated" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "open_account: {v}");
    v["result"]["account"].as_str().unwrap().to_owned()
}

async fn available(s: &Sidecar, account: &str, asset: &str) -> String {
    let (st, v) = s
        .call(
            "GET",
            &format!("/v1/balances?account={account}&asset={asset}"),
            &[],
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "balances {asset}: {v}");
    v["rows"][0]["available"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn req_ba_2_a_party_is_opened_funded_and_read_in_every_seeded_asset() {
    let s = started().await;
    let account = open(&s, "alice").await;

    for (asset, amount) in [
        ("COPM", "150000"),
        ("TSTA", "2500"),
        ("TSTB", "990"),
        ("TSTC", "1000001"),
    ] {
        let (st, v) = decided(
            &s,
            "/v1/issuances",
            &format!("issue-{asset}"),
            json!({ "asset": asset, "destination": account, "amount": amount }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "issue {asset}: {v}");
        assert_eq!(available(&s, &account, asset).await, amount, "{asset}");
    }

    // The operator moves value between the accounts it opened (its grant over delegated custody).
    let bob = open(&s, "bob").await;
    let (st, v) = decided(
        &s,
        "/v1/transfers",
        "alice-to-bob",
        json!({ "legs": [{ "from": account, "to": bob, "asset": "COPM", "amount": "500" }] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "transfer: {v}");
    assert_eq!(available(&s, &account, "COPM").await, "149500");
    assert_eq!(available(&s, &bob, "COPM").await, "500");

    // An asset outside the registry is refused, not invented.
    let (st, v) = decided(
        &s,
        "/v1/issuances",
        "issue-usd",
        json!({ "asset": "USD", "destination": account, "amount": "1" }),
    )
    .await;
    assert_ne!(st, StatusCode::OK, "USD is not seeded: {v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn req_au_8_a_request_without_the_test_token_is_refused() {
    let s = started().await;
    let (st, _) = ingress::testkit::call(&s.router, "GET", "/metrics", None, &[], None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) =
        ingress::testkit::call(&s.router, "GET", "/metrics", Some("wrong"), &[], None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = ingress::testkit::call(
        &s.router,
        "GET",
        "/metrics",
        Some(TEST_OPERATOR_TOKEN),
        &[],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_health_check_passes_once_listening_and_fails_before() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let addr = probe_address(&format!("0.0.0.0:{port}")).unwrap();
    assert_eq!(addr.to_string(), format!("127.0.0.1:{port}"));
    assert!(
        tokio::task::spawn_blocking(move || health(addr))
            .await
            .unwrap()
            .is_err(),
        "nothing listens yet"
    );

    let s = started().await;
    let operator = s.describe()["operator"].clone();
    let app = s.app();
    let (st, v) = ingress::testkit::call(&app, "GET", "/sidecar", None, &[], None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["operator"], operator);
    assert!(v.get("token").is_none(), "the token is not served: {v}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tokio::spawn(s.serve(listener));
    let r = tokio::task::spawn_blocking(move || health(addr))
        .await
        .unwrap();
    assert_eq!(r, Ok(()));
}

async fn started_with(options: Options) -> Sidecar {
    Sidecar::start_with(
        parse_assets(DEFAULT_ASSETS).expect("the default registry parses"),
        &options,
    )
    .await
    .expect("the sidecar starts")
}

#[test]
fn the_extra_tokens_and_max_legs_are_read_from_their_spelling() {
    assert_eq!(
        parse_tokens(" a-1 ,b-2,,a-1, ").unwrap(),
        vec!["a-1".to_owned(), "b-2".to_owned()]
    );
    assert_eq!(parse_tokens("").unwrap(), Vec::<String>::new());
    assert_eq!(
        parse_tokens(TEST_OPERATOR_TOKEN).unwrap(),
        Vec::<String>::new(),
        "the test token is not an extra one"
    );
    let e = parse_tokens("fine,not fine").unwrap_err();
    assert!(!e.contains("fine"), "an error never repeats a token: {e}");
    assert_eq!(parse_max_legs(" 3 "), Ok(3));
    assert!(parse_max_legs("0").is_err());
    assert!(parse_max_legs("many").is_err());
    assert!(parse_max_legs("").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn req_au_8_an_extra_token_authenticates_as_the_same_operator() {
    let extra = "ci-run-1234-secret";
    let s = started_with(Options {
        extra_tokens: vec![extra.into(), "ci-run-5678-secret".into()],
        max_legs: None,
        ..Options::default()
    })
    .await;
    let path = "/v1/principals/self/answering";
    let (st, as_test) =
        ingress::testkit::call(&s.router, "GET", path, Some(TEST_OPERATOR_TOKEN), &[], None).await;
    assert_eq!(st, StatusCode::OK, "{as_test}");
    for token in [extra, "ci-run-5678-secret"] {
        let (st, v) = ingress::testkit::call(&s.router, "GET", path, Some(token), &[], None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v, as_test, "the same principal answers under every token");
    }
    // The test token stays, and an unlisted one is still refused.
    let (st, _) = ingress::testkit::call(&s.router, "GET", path, Some("other"), &[], None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // And it acts as the operator, not merely reads: it opens an identity.
    let (st, v) = ingress::testkit::call(
        &s.router,
        "POST",
        "/v1/identities",
        Some(extra),
        &[("client-key", "identity-extra")],
        Some(json!({ "kind": "person", "verifier": "0a" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // describe() counts the extra tokens and shows none of them.
    let d = s.describe();
    assert_eq!(d["extra_tokens"], 2);
    assert!(
        !d.to_string().contains("secret"),
        "no extra token in describe: {d}"
    );
}

/// Fund `from`, then submit one transfer of `n` one-unit legs `from -> to` and return its answer.
async fn transfer_legs(s: &Sidecar, n: usize, key: &str) -> (StatusCode, Value) {
    let from = open(s, &format!("from-{key}")).await;
    let to = open(s, &format!("to-{key}")).await;
    let (st, v) = decided(
        s,
        "/v1/issuances",
        &format!("fund-{key}"),
        json!({ "asset": "COPM", "destination": from, "amount": "1000" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "fund: {v}");
    let legs: Vec<Value> = (0..n)
        .map(|_| json!({ "from": from, "to": to, "asset": "COPM", "amount": "1" }))
        .collect();
    let r = decided(s, "/v1/transfers", key, json!({ "legs": legs })).await;
    if r.0 == StatusCode::OK {
        assert_eq!(available(s, &to, "COPM").await, n.to_string(), "{key}");
    } else {
        assert_eq!(available(s, &from, "COPM").await, "1000", "nothing moved");
    }
    r
}

#[tokio::test(flavor = "multi_thread")]
async fn req_cs_10_the_default_max_legs_is_ten_and_an_eleventh_leg_is_refused() {
    let s = started().await;
    assert_eq!(s.describe()["max_legs"], 10);
    let (st, v) = transfer_legs(&s, 10, "ten").await;
    assert_eq!(st, StatusCode::OK, "10 legs: {v}");
    let (st, v) = transfer_legs(&s, 11, "eleven").await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "11 legs: {v}");
    assert_eq!(v["error"], "ERR-BatchTooLarge");
}

#[tokio::test(flavor = "multi_thread")]
async fn req_cs_10_max_legs_is_configurable_three_legs_pass_and_four_are_refused() {
    let s = started_with(Options {
        extra_tokens: vec![],
        max_legs: Some(3),
        ..Options::default()
    })
    .await;
    assert_eq!(s.describe()["max_legs"], 3);
    let (st, v) = transfer_legs(&s, 3, "three").await;
    assert_eq!(st, StatusCode::OK, "3 legs: {v}");
    let (st, v) = transfer_legs(&s, 4, "four").await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "4 legs: {v}");
    assert_eq!(v["error"], "ERR-BatchTooLarge");
    // /sidecar serves the limit and the number of tokens, no value.
    let (st, v) = ingress::testkit::call(&s.app(), "GET", "/sidecar", None, &[], None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        (v["max_legs"].as_u64(), v["extra_tokens"].as_u64()),
        (Some(3), Some(0))
    );
}

/// `REQ-PO-5` (amendment 0020 R1, `T-009-21`): the sidecar takes a transfer's `reference` and gives
/// it back, as an integrating party's CI relies on: in the reply, the operation and the movements.
#[tokio::test(flavor = "multi_thread")]
async fn req_po_5_the_sidecar_passes_the_reference_through() {
    let s = started().await;
    let alice = open(&s, "alice").await;
    let bob = open(&s, "bob").await;
    let (st, v) = decided(
        &s,
        "/v1/issuances",
        "fund-alice",
        json!({ "asset": "COPM", "destination": alice, "amount": "1000" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "fund: {v}");
    let reference = json!({
        "order": "o-1",
        "graph": { "id": "g-7", "node": "transfer_single" },
        "movement": { "type": "p2p", "description": "Lunch" },
    });
    let (st, t) = decided(
        &s,
        "/v1/transfers",
        "with-reference",
        json!({
            "legs": [{ "from": alice, "to": bob, "asset": "COPM", "amount": "5" }],
            "reference": reference,
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "transfer: {t}");
    assert_eq!(t["reference"], reference);
    let op = t["operation_reference"].as_str().unwrap();
    let (st, o) = s
        .call("GET", &format!("/v1/operations/{op}"), &[], None)
        .await;
    assert_eq!(st, StatusCode::OK, "{o}");
    assert_eq!(o["reference"], reference);
    let (st, page) = s
        .call(
            "GET",
            &format!("/v1/movements?account={alice}&limit=10"),
            &[],
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    let moved: Vec<&Value> = page["movements"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["operation"] == op)
        .collect();
    assert!(!moved.is_empty());
    assert!(moved.iter().all(|m| m["reference"] == reference), "{page}");
    // Over 1 KiB: refused, as everywhere.
    let (st, v) = s
        .call(
            "POST",
            "/v1/transfers",
            &[],
            Some(json!({
                "legs": [{ "from": alice, "to": bob, "asset": "COPM", "amount": "1" }],
                "reference": "x".repeat(1024),
            })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "ERR-MalformedCommand");
}

#[test]
fn req_ev_4_recipients_parse_endpoint_and_secret_name_defaulting_to_the_operator() {
    let r = parse_recipients(
        " http://host.docker.internal:3000/custodian/notifications#CUSTODIAN_SECRET ;\
         did:aledgent:prin:abc=https://x.test/hook?a=b#OTHER_1;",
    )
    .unwrap();
    assert_eq!(
        r,
        vec![
            RecipientSpec {
                principal: None,
                endpoint: "http://host.docker.internal:3000/custodian/notifications".into(),
                secret_env: "CUSTODIAN_SECRET".into(),
            },
            RecipientSpec {
                principal: Some("did:aledgent:prin:abc".into()),
                endpoint: "https://x.test/hook?a=b".into(),
                secret_env: "OTHER_1".into(),
            },
        ]
    );
    assert_eq!(
        parse_recipients("operator=http://a/b#S").unwrap()[0].principal,
        None,
        "operator is the default, spelled out"
    );
    assert_eq!(parse_recipients("").unwrap(), vec![]);
    assert!(
        parse_recipients("http://a/b").is_err(),
        "no secret variable"
    );
    assert!(parse_recipients("ftp://a/b#S").is_err(), "not http(s)");
    assert!(
        parse_recipients("Bob=http://a/b#S").is_err(),
        "neither operator, a principal's name nor a did"
    );
    assert!(
        parse_recipients("http://a/b#S;operator=http://c/d#T").is_err(),
        "the operator twice"
    );
    let e = parse_recipients("http://a/b#s3cr3t value").unwrap_err();
    assert!(e.contains("not a variable name"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn req_ev_4_a_recipient_whose_secret_is_not_set_fails_the_start() {
    let options = Options {
        recipients: parse_recipients("http://127.0.0.1:9/n#ALEDGENT_SIDECAR_TEST_UNSET").unwrap(),
        ..Options::default()
    };
    let e =
        match Sidecar::start_with_lookup(parse_assets(DEFAULT_ASSETS).unwrap(), &options, |_| None)
            .await
        {
            Ok(_) => panic!("started without the recipient's secret"),
            Err(e) => e,
        };
    assert!(
        e.contains("ALEDGENT_SIDECAR_RECIPIENTS")
            && e.contains("ALEDGENT_SIDECAR_TEST_UNSET is not set"),
        "{e}"
    );
}

/// What a recipient received: each attempt's headers and body.
type Received = std::sync::Arc<std::sync::Mutex<Vec<(Vec<(String, String)>, Vec<u8>)>>>;

/// A recipient on a local port, as an integrating service is in CI: it keeps every POST to
/// `path` and answers `200`.
async fn recipient_listener(path: &'static str) -> (String, Received) {
    let got: Received = Default::default();
    let keep = std::sync::Arc::clone(&got);
    let app = axum::Router::new().route(
        path,
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let keep = std::sync::Arc::clone(&keep);
                async move {
                    let headers = headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
                        .collect();
                    keep.lock().unwrap().push((headers, body.to_vec()));
                    StatusCode::OK
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}{path}"), got)
}

/// `REQ-EV-4`, `REQ-PO-5`: a recipient registered at start receives the operator's notifications
/// of a transfer, signed with its secret, carrying the transfer's `reference`. `/sidecar` shows
/// the recipient's endpoint and never its secret.
#[tokio::test(flavor = "multi_thread")]
async fn req_ev_4_a_registered_recipient_receives_signed_notifications_with_the_reference() {
    const SECRET: &str = "custodian-ci-notification-secret";
    const SERVICE_SECRET: &str = "custodian-own-notification-secret";
    let (endpoint, got) = recipient_listener("/custodian/_internal/aledgent-notifications").await;
    // A second recipient, registered by a service principal's name: the custodian's own
    // principal, under a token of its own variable.
    let (service_endpoint, service_got) = recipient_listener("/custodian/own").await;
    let options = Options {
        recipients: parse_recipients(&format!(
            "{endpoint}#CUSTODIAN_NOTIFICATION_SECRET;\
             custodian={service_endpoint}#CUSTODIAN_OWN_SECRET"
        ))
        .unwrap(),
        services: parse_principals(
            r#"{"principals": [{"name": "custodian", "role": "operator_authority",
                                "token_env": "CUSTODIAN_TOKEN"}]}"#,
        )
        .unwrap(),
        ..Options::default()
    };
    let s = Sidecar::start_with_lookup(parse_assets(DEFAULT_ASSETS).unwrap(), &options, |v| {
        match v {
            "CUSTODIAN_NOTIFICATION_SECRET" => Some(SECRET),
            "CUSTODIAN_OWN_SECRET" => Some(SERVICE_SECRET),
            "CUSTODIAN_TOKEN" => Some(CUSTODIAN_TOKEN),
            _ => None,
        }
        .map(str::to_owned)
    })
    .await
    .expect("the sidecar starts with its recipients");

    let (st, served) = ingress::testkit::call(&s.app(), "GET", "/sidecar", None, &[], None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(served["recipients"], 2);
    assert_eq!(
        served["recipient_endpoints"],
        json!([
            { "principal": served["operator"], "endpoint": endpoint },
            {
                "principal": served["service_principals"]["custodian"]["principal"],
                "endpoint": service_endpoint,
            },
        ])
    );
    assert!(
        !served.to_string().contains(SECRET),
        "the secret is not served"
    );

    let alice = open(&s, "alice").await;
    let bob = open(&s, "bob").await;
    let (st, v) = decided(
        &s,
        "/v1/issuances",
        "issue-alice",
        json!({ "asset": "COPM", "destination": alice, "amount": "1000" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "issue: {v}");
    let reference = json!({ "order": "o-1", "graph": { "node": "transfer_single" } });
    let (st, v) = decided(
        &s,
        "/v1/transfers",
        "notified",
        json!({
            "legs": [{ "from": alice, "to": bob, "asset": "COPM", "amount": "25" }],
            "reference": reference,
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "transfer: {v}");
    let op = v["operation_reference"].as_str().unwrap().to_owned();

    // The delivery worker pushes them; wait for the transfer's.
    let until = Instant::now() + Duration::from_secs(30);
    let mine = loop {
        let mine: Vec<_> = got
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| {
                serde_json::from_slice::<Value>(body).unwrap()["operation_reference"] == op
            })
            .cloned()
            .collect();
        if !mine.is_empty() || Instant::now() >= until {
            break mine;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(!mine.is_empty(), "the transfer's notification arrived");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut verifier = ingress::delivery::ReferenceVerifier::new(SECRET, 600_000);
    for (headers, body) in &mine {
        assert!(
            verifier.verify(headers, body, now).is_ok(),
            "the signature verifies with the secret"
        );
        let n: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(n["reference"], reference, "it carries the reference: {n}");
    }
    // And with another secret it does not.
    let (headers, body) = &mine[0];
    assert!(
        ingress::delivery::ReferenceVerifier::new("not-the-secret", 600_000)
            .verify(headers, body, now)
            .is_err()
    );

    // A command the custodian submits with its own token is delivered to the recipient
    // registered for `custodian`, signed with that recipient's secret.
    let (st, v) = as_(
        &s,
        CUSTODIAN_TOKEN,
        "/v1/transfers",
        "custodian-notified",
        json!({ "legs": [{ "from": alice, "to": bob, "asset": "COPM", "amount": "5" }] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the custodian's transfer: {v}");
    let op = v["operation_reference"].as_str().unwrap().to_owned();
    let until = Instant::now() + Duration::from_secs(30);
    let theirs = loop {
        let theirs: Vec<_> = service_got
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| {
                serde_json::from_slice::<Value>(body).unwrap()["operation_reference"] == op
            })
            .cloned()
            .collect();
        if !theirs.is_empty() || Instant::now() >= until {
            break theirs;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        !theirs.is_empty(),
        "the custodian's recipient got its transfer's notification"
    );
    let mut verifier = ingress::delivery::ReferenceVerifier::new(SERVICE_SECRET, 600_000);
    for (headers, body) in &theirs {
        assert!(
            verifier.verify(headers, body, now).is_ok(),
            "signed with its secret"
        );
    }
}

/// A name names a service principal in `ALEDGENT_SIDECAR_RECIPIENTS`, and a name that is not a
/// service principal of the configuration fails the start.
#[tokio::test(flavor = "multi_thread")]
async fn req_ev_4_a_recipient_by_name_needs_that_service_principal() {
    let r = parse_recipients("custodian=http://a/b#S;issuer=https://c/d#T").unwrap();
    assert_eq!(r[0].principal.as_deref(), Some("custodian"));
    assert_eq!(r[1].principal.as_deref(), Some("issuer"));
    let options = Options {
        recipients: parse_recipients("onboarding=http://127.0.0.1:9/n#S").unwrap(),
        ..Options::default()
    };
    let e =
        match Sidecar::start_with_lookup(parse_assets(DEFAULT_ASSETS).unwrap(), &options, |_| {
            Some("x".to_owned())
        })
        .await
        {
            Ok(_) => panic!("started"),
            Err(e) => e,
        };
    assert!(e.contains("onboarding is not a service principal"), "{e}");
}

/// The service principals' identifiers are the same on every start of the same configuration, as
/// the operator's are (each opened under a client key derived from its name), and differ by name.
#[tokio::test(flavor = "multi_thread")]
async fn req_op_10_service_principal_ids_are_the_same_on_every_start() {
    let a = with_services().await.describe();
    let b = with_services().await.describe();
    for role in ["issuer", "custodian", "onboarding"] {
        for k in ["principal", "credential"] {
            assert_eq!(
                a["service_principals"][role][k], b["service_principals"][role][k],
                "{role} {k}"
            );
        }
    }
    assert_eq!(a["operator"], b["operator"]);
    let ids: std::collections::BTreeSet<String> = ["issuer", "custodian", "onboarding"]
        .iter()
        .map(|r| {
            a["service_principals"][r]["principal"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(ids.len(), 3, "one principal per name");
}

// ------------------------------------------------------------------ service principals

/// The sample configuration (`e2e/sidecar/principals.json`), which the binary opens by default:
/// one principal of each role, under neutral names, each opting in to its public test token.
#[test]
fn the_sample_configuration_names_one_principal_of_each_role() {
    let ops = |o: &[&str]| o.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let sample = parse_principals(DEFAULT_PRINCIPALS).unwrap();
    assert_eq!(
        sample,
        vec![
            PrincipalSpec {
                name: "issuer".into(),
                role: PrincipalRole::IssuanceAuthority,
                token_env: None,
                test_token: true,
                ops: ops(&["issue", "redeem"]),
                assets: None,
                per_day_limit: PER_DAY_LIMIT.into(),
            },
            PrincipalSpec {
                name: "custodian".into(),
                role: PrincipalRole::OperatorAuthority,
                token_env: None,
                test_token: true,
                ops: ops(&["transfer", "reserve", "capture", "release"]),
                assets: None,
                per_day_limit: PER_DAY_LIMIT.into(),
            },
            PrincipalSpec {
                name: "onboarding".into(),
                role: PrincipalRole::IdentitiesPrincipal,
                token_env: None,
                test_token: true,
                ops: vec![],
                assets: None,
                per_day_limit: PER_DAY_LIMIT.into(),
            },
        ]
    );
    assert_eq!(load_principals(None, None).unwrap(), sample, "the default");
    for (inline, file) in [(Some("  "), None), (None, Some(""))] {
        let e = load_principals(inline, file).unwrap_err();
        assert!(
            e.contains("is set but blank"),
            "set but blank is never unset: {e}"
        );
    }
    assert_eq!(test_principal_token("issuer"), ISSUER_TOKEN);
    assert_eq!(test_principal_token("custodian"), CUSTODIAN_TOKEN);
    assert_eq!(test_principal_token("onboarding"), ONBOARDING_TOKEN);
    // The sample's own file is what the crate embeds.
    let file =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/principals.json")).unwrap();
    assert_eq!(file, DEFAULT_PRINCIPALS);
}

/// A configuration is checked before anything starts, and every fault names the principal and the
/// field; no error repeats a token or a `token_env` value.
#[test]
fn req_au_8_a_principals_configuration_is_validated_with_clear_errors() {
    let err = |c: &str| parse_principals(c).unwrap_err();
    let one = |p: &str| format!(r#"{{"principals": [{p}]}}"#);
    // A principal of `role` with a test token and `extra` fields.
    let with = |role: &str, extra: &str| {
        one(&format!(
            r#"{{"name": "x", "role": "{role}", "test_token": true{extra}}}"#
        ))
    };

    assert_eq!(parse_principals(r#"{"principals": []}"#).unwrap(), vec![]);
    assert!(err("issuer=ISSUER_TOKEN").contains("retired"));
    assert!(err("[]").contains("retired"), "a top-level array");
    assert!(err("{").contains("not valid JSON"));
    assert!(err(r#"{"principals": [], "extra": 1}"#).contains("unknown field \"extra\""));
    assert!(err(r#"{}"#).contains("no \"principals\""));
    assert!(err(r#"{"principals": {}}"#).contains("not a list"));
    assert!(err(&one(r#""issuer""#)).contains("principal 1: not an object"));
    assert!(
        err(&one(
            r#"{"role": "issuance_authority", "test_token": true}"#
        ))
        .contains("principal 1: no \"name\"")
    );
    assert!(
        err(&one(r#"{"name": 7, "role": "issuance_authority"}"#)).contains("name is not a string")
    );
    let long = "a".repeat(65);
    for bad in ["Issuer", "operator", "1st", "a b", "a/b", "", long.as_str()] {
        let e = err(&one(&format!(
            r#"{{"name": {bad:?}, "role": "issuance_authority", "test_token": true}}"#
        )));
        assert!(e.contains("is not a name"), "{bad:?}: {e}");
    }
    let ok = "a".repeat(64);
    assert!(
        parse_principals(&one(&format!(
            r#"{{"name": "{ok}", "role": "issuance_authority", "test_token": true}}"#
        )))
        .is_ok(),
        "64 bytes is a name"
    );
    let e = err(&with("admin", ""));
    assert!(
        e.contains("principal 1 (x): \"admin\" is not a role")
            && e.contains("issuance_authority, operator_authority, identities_principal"),
        "{e}"
    );
    assert!(err(&one(r#"{"name": "x"}"#)).contains("principal 1 (x): no \"role\""));
    assert!(err(&one(r#"{"name": "x", "role": 1}"#)).contains("role is not a string"));
    let e = err(&with("issuance_authority", r#", "tokn": "A""#));
    assert!(e.contains("\"tokn\" is not a field"), "{e}");
    let e = err(&with("identities_principal", r#", "ops": ["issue"]"#));
    assert!(
        e.contains("\"ops\" is not a field of the identities_principal role"),
        "{e}"
    );
    let e = err(&with("operator_authority", r#", "assets": ["TSTC"]"#));
    assert!(
        e.contains("\"assets\" is not a field of the operator_authority role"),
        "{e}"
    );
    let e = err(&with("issuance_authority", r#", "ops": ["mint"]"#));
    assert!(
        e.contains("ops: \"mint\" is not one of issue, redeem"),
        "{e}"
    );
    let e = err(&with("operator_authority", r#", "ops": ["issue"]"#));
    assert!(e.contains("ops: \"issue\" is not one of transfer"), "{e}");
    let e = err(&with("operator_authority", r#", "ops": []"#));
    assert!(e.contains("ops is empty"), "{e}");
    let e = err(&with(
        "operator_authority",
        r#", "ops": ["transfer", "transfer"]"#,
    ));
    assert!(e.contains("ops names transfer twice"), "{e}");
    let e = err(&with(
        "issuance_authority",
        r#", "assets": ["TSTC", "tstc"]"#,
    ));
    assert!(e.contains("assets names TSTC twice"), "{e}");

    // The token: exactly one source, never assumed (REQ-AU-8).
    let e = err(&one(r#"{"name": "x", "role": "issuance_authority"}"#));
    assert!(
        e.contains("principal 1 (x): no token") && e.contains("REQ-AU-8"),
        "{e}"
    );
    let e = err(&one(
        r#"{"name": "x", "role": "issuance_authority", "test_token": false}"#,
    ));
    assert!(e.contains("no token"), "{e}");
    let e = err(&with("issuance_authority", r#", "token_env": "X_TOKEN""#));
    assert!(e.contains("token_env and test_token both given"), "{e}");
    let e = err(&one(
        r#"{"name": "x", "role": "issuance_authority", "token_env": "s3cr3t-token value"}"#,
    ));
    assert!(e.contains("token_env names no variable"), "{e}");
    assert!(
        !e.contains("s3cr3t"),
        "no error repeats a token_env value: {e}"
    );
    let e = err(&one(
        r#"{"name": "x", "role": "issuance_authority", "token_env": 5}"#,
    ));
    assert!(e.contains("token_env is not a string"), "{e}");
    let e = err(&with("issuance_authority", r#", "test_token": "yes""#));
    assert!(
        e.contains("is given twice"),
        "a field twice is refused first: {e}"
    );
    let e = err(&one(
        r#"{"name": "x", "role": "issuance_authority", "test_token": "yes"}"#,
    ));
    assert!(e.contains("test_token is not true or false"), "{e}");

    // The bound: a canonical decimal the core takes, 1 to u64::MAX.
    for limit in [
        r#""0""#,
        r#""1.5""#,
        "1000",
        r#""""#,
        r#""-1""#,
        r#""+5""#,
        r#""007""#,
        r#""18446744073709551616""#,
        r#""99999999999999999999999999999999999999""#,
    ] {
        let e = err(&with(
            "issuance_authority",
            &format!(r#", "per_day_limit": {limit}"#),
        ));
        assert!(e.contains("per_day_limit"), "{limit}: {e}");
    }
    let p = parse_principals(&with(
        "operator_authority",
        r#", "per_day_limit": "18446744073709551615""#,
    ))
    .unwrap();
    assert_eq!(
        p[0].per_day_limit, "18446744073709551615",
        "u64::MAX is taken"
    );

    // A field given twice, at any depth, is refused rather than read as its last value.
    let e = err(
        r#"{"principals": [{"name": "x", "role": "issuance_authority",
                                    "name": "y", "role": "operator_authority"}]}"#,
    );
    assert!(e.contains("the field \"name\" is given twice"), "{e}");
    let e = err(r#"{"principals": [], "principals": []}"#);
    assert!(e.contains("is given twice"), "{e}");

    let e = err(r#"{"principals": [
        {"name": "x", "role": "issuance_authority", "test_token": true},
        {"name": "x", "role": "operator_authority", "test_token": true}]}"#);
    assert!(e.contains("principal 2 (x): x is named twice"), "{e}");
    let e = err(r#"{"principals": [
        {"name": "a", "role": "identities_principal", "test_token": true},
        {"name": "b", "role": "identities_principal", "test_token": true}]}"#);
    assert!(
        e.contains("principal 2 (b): a already plays the identities principal"),
        "{e}"
    );
    // Assets are upper-cased, as the registry is.
    let p = parse_principals(&with(
        "issuance_authority",
        r#", "assets": ["tstc"], "per_day_limit": "7""#,
    ))
    .unwrap();
    assert_eq!(p[0].assets, Some(vec!["TSTC".to_owned()]));
    assert_eq!(p[0].per_day_limit, "7");
}

/// The same rules hold for principals built by hand: a start checks them before anything is
/// opened.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_checks_hand_built_principals_as_a_configuration_is_checked() {
    let base = PrincipalSpec {
        name: "x".into(),
        role: PrincipalRole::OperatorAuthority,
        token_env: None,
        test_token: true,
        ops: vec!["transfer".into()],
        assets: None,
        per_day_limit: PER_DAY_LIMIT.into(),
    };
    for (spec, want) in [
        (
            PrincipalSpec {
                ops: vec!["issue".into()],
                ..base.clone()
            },
            "ops: \"issue\" is not one of",
        ),
        (
            PrincipalSpec {
                per_day_limit: "0".into(),
                ..base.clone()
            },
            "per_day_limit",
        ),
        (
            PrincipalSpec {
                token_env: Some("not a var; x".into()),
                test_token: false,
                ..base.clone()
            },
            "token_env names no variable",
        ),
        (
            PrincipalSpec {
                test_token: false,
                ..base.clone()
            },
            "no token",
        ),
        (
            PrincipalSpec {
                assets: Some(vec!["TSTC".into()]),
                ..base.clone()
            },
            "assets is not a field of the operator_authority role",
        ),
    ] {
        let options = Options {
            services: vec![spec],
            ..Options::default()
        };
        let e = match Sidecar::start_with_lookup(
            parse_assets(DEFAULT_ASSETS).unwrap(),
            &options,
            |_| Some("t".to_owned()),
        )
        .await
        {
            Ok(_) => panic!("started"),
            Err(e) => e,
        };
        assert!(
            e.starts_with("ALEDGENT_SIDECAR_SERVICE_PRINCIPALS: principal 1 (x): ")
                && e.contains(want),
            "{want}: {e}"
        );
    }
}

/// No two principals share a token, whichever way each gets it: two variables of one value, or a
/// variable holding another principal's test token, fails the start without repeating the token.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_8_no_two_principals_share_a_token() {
    let start = |config: &'static str, lookup: fn(&str) -> Option<String>| async move {
        let options = Options {
            services: parse_principals(config).unwrap(),
            ..Options::default()
        };
        match Sidecar::start_with_lookup(parse_assets(DEFAULT_ASSETS).unwrap(), &options, lookup)
            .await
        {
            Ok(_) => panic!("started"),
            Err(e) => e,
        }
    };
    let e = start(
        r#"{"principals": [
            {"name": "a", "role": "issuance_authority", "token_env": "A_TOKEN"},
            {"name": "b", "role": "operator_authority", "token_env": "B_TOKEN"}]}"#,
        |_| Some("one-shared-value".to_owned()),
    )
    .await;
    assert!(e.contains("B_TOKEN is another principal's token"), "{e}");
    assert!(!e.contains("one-shared-value"), "{e}");
    let e = start(
        r#"{"principals": [
            {"name": "a", "role": "issuance_authority", "test_token": true},
            {"name": "b", "role": "operator_authority", "token_env": "B_TOKEN"}]}"#,
        |_| Some("aledgent-sidecar-test-a-token".to_owned()),
    )
    .await;
    assert!(e.contains("B_TOKEN is another principal's token"), "{e}");
    assert!(!e.contains("aledgent-sidecar-test-a-token"), "{e}");
}

/// The configuration comes inline or from a file, never both; a file that cannot be read names its
/// path.
#[test]
fn a_principals_configuration_is_read_inline_or_from_a_file() {
    let config =
        r#"{"principals": [{"name": "desk", "role": "issuance_authority", "test_token": true}]}"#;
    let inline = load_principals(Some(config), None).unwrap();
    assert_eq!(inline[0].name, "desk");
    let dir = std::env::temp_dir().join(format!("aledgent-sidecar-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("principals.json");
    std::fs::write(&path, config).unwrap();
    let path = path.to_str().unwrap();
    assert_eq!(load_principals(None, Some(path)).unwrap(), inline);
    let e = load_principals(Some(config), Some(path)).unwrap_err();
    assert!(e.contains("are both set"), "{e}");
    let missing = dir.join("missing.json");
    let e = load_principals(None, missing.to_str()).unwrap_err();
    assert!(
        e.starts_with("ALEDGENT_SIDECAR_SERVICE_PRINCIPALS_FILE") && e.contains("missing.json"),
        "{e}"
    );
    let e = load_principals(Some("issuer=ISSUER_TOKEN"), None).unwrap_err();
    assert!(
        e.starts_with("ALEDGENT_SIDECAR_SERVICE_PRINCIPALS:") && e.contains("retired"),
        "{e}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn system_accounts_are_read_from_their_spelling() {
    assert_eq!(
        parse_system_accounts("issuer/fee=issuer; issuer/provider-cost/x_1=issuer").unwrap(),
        vec![
            SystemAccountSpec {
                name: "issuer/fee".into(),
                service: "issuer".into()
            },
            SystemAccountSpec {
                name: "issuer/provider-cost/x_1".into(),
                service: "issuer".into()
            },
        ]
    );
    assert!(
        parse_system_accounts("Issuer/Fee=issuer").is_err(),
        "upper case"
    );
    assert!(
        parse_system_accounts("issuer/fee=Not A Name").is_err(),
        "not a principal's name"
    );
    assert!(
        parse_system_accounts("issuer/fee=issuer;issuer/fee=custodian").is_err(),
        "a role twice"
    );
}

/// The principals are whatever the configuration names: an arbitrary name set starts, each name
/// keys `/sidecar`, answers as its own principal and holds exactly the grants its parameters say
/// (`REQ-AU-2`, `REQ-AU-10`, `REQ-AU-5`, `REQ-ID-6`).
#[tokio::test(flavor = "multi_thread")]
async fn an_arbitrary_set_of_names_and_grant_parameters_is_opened_as_configured() {
    const KYC_TOKEN: &str = "ci-kyc-token";
    let options = Options {
        services: parse_principals(
            r#"{"principals": [
                {"name": "mint-desk", "role": "issuance_authority", "test_token": true,
                 "assets": ["TSTC"], "ops": ["issue"], "per_day_limit": "500"},
                {"name": "wallets", "role": "operator_authority", "test_token": true,
                 "ops": ["transfer"]},
                {"name": "kyc", "role": "identities_principal", "token_env": "KYC_TOKEN"}
            ]}"#,
        )
        .unwrap(),
        system_accounts: parse_system_accounts("mint-desk/fee=mint-desk").unwrap(),
        ..Options::default()
    };
    let s = Sidecar::start_with_lookup(parse_assets(DEFAULT_ASSETS).unwrap(), &options, |v| {
        (v == "KYC_TOKEN").then(|| KYC_TOKEN.to_owned())
    })
    .await
    .expect("the sidecar starts with the configured names");
    let (_, served) = ingress::testkit::call(&s.app(), "GET", "/sidecar", None, &[], None).await;
    let sp = served["service_principals"].as_object().unwrap();
    let mut names: Vec<&str> = sp.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["kyc", "mint-desk", "wallets"]);

    let desk = test_principal_token("mint-desk");
    let wallets = test_principal_token("wallets");
    for (name, token) in [
        ("mint-desk", desk.as_str()),
        ("wallets", wallets.as_str()),
        ("kyc", KYC_TOKEN),
    ] {
        let (st, v) = get_as(&s, token, "/v1/principals/self/answering").await;
        assert_eq!(st, StatusCode::OK, "{name}: {v}");
        assert_eq!(
            v["principal"], sp[name]["principal"],
            "{name} answers as itself"
        );
    }
    // mint-desk: one issuance grant, over TSTC, issue only, bounded at 500 a day; and the
    // account-scoped grant on its system account.
    let g = sp["mint-desk"]["grants"].as_array().unwrap();
    let issuance: Vec<&Value> = g.iter().filter(|g| g["kind"] == "issuance").collect();
    assert_eq!(issuance.len(), 1, "{g:?}");
    assert_eq!(issuance[0]["subject"], json!({ "asset": "TSTC" }));
    assert_eq!(issuance[0]["ops"], json!(["issue"]));
    assert_eq!(issuance[0]["per_window"]["limit"], "500");
    assert_eq!(served["system_accounts"][0]["service"], "mint-desk");
    // wallets: an operator grant over delegated accounts, transfer only.
    assert_eq!(sp["wallets"]["grants"][0]["ops"], json!(["transfer"]));
    assert_eq!(sp["wallets"]["grants"][0]["subject"], "delegated_accounts");
    assert_eq!(sp["kyc"]["grants"][0]["kinds"], json!(["open_identity"]));

    // kyc opens an identity and its account; mint-desk issues TSTC into it within its bound, and
    // is refused another asset, beyond its bound, and a redemption.
    let (st, v) = as_(
        &s,
        KYC_TOKEN,
        "/v1/identities",
        "kyc-user",
        json!({ "kind": "person", "verifier": "0a" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "kyc opens an identity: {v}");
    let user = v["result"]["identity"].as_str().unwrap().to_owned();
    let (st, v) = as_(
        &s,
        KYC_TOKEN,
        "/v1/accounts",
        "kyc-account",
        json!({ "owner": user, "kind": "customer", "custody": "delegated" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "kyc opens an account: {v}");
    let acct = v["result"]["account"].as_str().unwrap().to_owned();
    let issue = |asset: &str, amount: &str| json!({ "asset": asset, "destination": acct, "amount": amount });
    let (st, v) = as_(
        &s,
        &desk,
        "/v1/issuances",
        "desk-tstc",
        issue("TSTC", "400"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "mint-desk issues TSTC: {v}");
    refused(
        &as_(&s, &desk, "/v1/issuances", "desk-copm", issue("COPM", "1")).await,
        "mint-desk issuing an asset it holds no authority over",
    );
    let (st, v) = as_(
        &s,
        &desk,
        "/v1/issuances",
        "desk-over",
        issue("TSTC", "101"),
    )
    .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("ERR-BoundExceeded")),
        "mint-desk beyond its per-day bound: {v}"
    );
    // wallets holds transfer only: it moves value between delegated accounts, and a reservation
    // on the same account is refused.
    let (st, v) = as_(
        &s,
        KYC_TOKEN,
        "/v1/accounts",
        "kyc-account-2",
        json!({ "owner": user, "kind": "customer", "custody": "delegated" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "kyc opens a second account: {v}");
    let other = v["result"]["account"].as_str().unwrap().to_owned();
    let (st, v) = as_(
        &s,
        &wallets,
        "/v1/transfers",
        "wallets-transfer",
        json!({ "legs": [{ "from": acct, "to": other, "asset": "TSTC", "amount": "1" }] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "wallets transfers: {v}");
    let (st, v) = as_(
        &s,
        &wallets,
        "/v1/reservations",
        "wallets-reserve",
        json!({ "payer": acct, "asset": "TSTC", "amount": "1" }),
    )
    .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("ERR-NotAuthorised")),
        "wallets reserving, a kind its grant does not name: {v}"
    );
    refused(
        &as_(
            &s,
            &desk,
            "/v1/redemptions",
            "desk-redeem",
            json!({ "asset": "TSTC", "source": acct, "amount": "1" }),
        )
        .await,
        "mint-desk redeeming without redeem",
    );
}

/// An asset a principal names must be in the registry.
#[tokio::test(flavor = "multi_thread")]
async fn a_principal_over_an_asset_outside_the_registry_fails_the_start() {
    let options = Options {
        services: parse_principals(
            r#"{"principals": [{"name": "x", "role": "issuance_authority", "test_token": true,
                                "assets": ["EURX"]}]}"#,
        )
        .unwrap(),
        ..Options::default()
    };
    let e =
        match Sidecar::start_with_lookup(parse_assets(DEFAULT_ASSETS).unwrap(), &options, |_| None)
            .await
        {
            Ok(_) => panic!("started"),
            Err(e) => e,
        };
    assert!(e.contains("x: EURX is not an asset of the registry"), "{e}");
}

// The sample configuration's principals name no variable: each is recognised under its public
// test token (`test_principal_token`).
const ISSUER_TOKEN: &str = "aledgent-sidecar-test-issuer-token";
const CUSTODIAN_TOKEN: &str = "aledgent-sidecar-test-custodian-token";
const ONBOARDING_TOKEN: &str = "aledgent-sidecar-test-onboarding-token";

/// A sidecar with the sample configuration's three service principals and the issuer's fee
/// account.
async fn with_services() -> Sidecar {
    let options = Options {
        services: parse_principals(DEFAULT_PRINCIPALS).unwrap(),
        system_accounts: parse_system_accounts("issuer/fee=issuer").unwrap(),
        ..Options::default()
    };
    Sidecar::start_with_lookup(parse_assets(DEFAULT_ASSETS).unwrap(), &options, |_| None)
        .await
        .expect("the sidecar starts with its service principals")
}

/// A command under `token`, repeated under its client key until decided.
async fn as_(s: &Sidecar, token: &str, path: &str, key: &str, body: Value) -> (StatusCode, Value) {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let r = ingress::testkit::call(
            &s.router,
            "POST",
            path,
            Some(token),
            &[("client-key", key)],
            Some(body.clone()),
        )
        .await;
        if r.0 != StatusCode::ACCEPTED || Instant::now() >= until {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn get_as(s: &Sidecar, token: &str, path: &str) -> (StatusCode, Value) {
    ingress::testkit::call(&s.router, "GET", path, Some(token), &[], None).await
}

fn refused(r: &(StatusCode, Value), what: &str) {
    assert!(
        r.0 == StatusCode::FORBIDDEN || r.0 == StatusCode::NOT_FOUND,
        "{what} is refused: {} {}",
        r.0,
        r.1
    );
}

/// `REQ-AU-7`: each service token answers as its **own** principal, never the operator, so "the
/// operator acted" is told from "the service acted"; `/sidecar` shows each principal, its role and
/// its grants, never a token.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_7_each_service_token_answers_as_its_own_principal() {
    let s = with_services().await;
    let (st, served) = ingress::testkit::call(&s.app(), "GET", "/sidecar", None, &[], None).await;
    assert_eq!(st, StatusCode::OK);
    let operator = served["operator"].as_str().unwrap().to_owned();
    let mut seen = vec![operator.clone()];
    for (role, token) in [
        ("issuer", ISSUER_TOKEN),
        ("custodian", CUSTODIAN_TOKEN),
        ("onboarding", ONBOARDING_TOKEN),
    ] {
        let (st, v) = get_as(&s, token, "/v1/principals/self/answering").await;
        assert_eq!(st, StatusCode::OK, "{role}: {v}");
        let principal = served["service_principals"][role]["principal"].clone();
        assert_eq!(v["principal"], principal, "{role} answers as itself");
        let principal = principal.as_str().unwrap().to_owned();
        assert!(!seen.contains(&principal), "{role}: a principal of its own");
        seen.push(principal);
    }
    // The grants each role holds.
    let sp = &served["service_principals"];
    assert_eq!(sp["issuer"]["role"], "issuance_authority");
    assert_eq!(sp["custodian"]["role"], "operator_authority");
    assert_eq!(sp["onboarding"]["role"], "identities_principal");
    let issuer = sp["issuer"]["grants"].as_array().unwrap();
    assert_eq!(
        issuer.iter().filter(|g| g["kind"] == "issuance").count(),
        4,
        "an issuance grant per asset: {issuer:?}"
    );
    assert!(issuer.iter().all(|g| g["grant"].is_string()), "{issuer:?}");
    assert!(
        issuer
            .iter()
            .any(|g| g["kind"] == "delegated" && g["subject"]["account"].is_string()),
        "the fee account's grant: {issuer:?}"
    );
    assert_eq!(
        sp["custodian"]["grants"],
        json!([{
            "grant": sp["custodian"]["grants"][0]["grant"],
            "kind": "operator",
            "principal": sp["custodian"]["principal"],
            "subject": "delegated_accounts",
            "ops": ["transfer", "reserve", "capture", "release"],
            "per_window": { "length": 86_400_000u64, "limit": "1000000000000000" },
        }])
    );
    assert_eq!(
        sp["onboarding"]["grants"][0]["kinds"],
        json!(["open_identity"])
    );
    assert_eq!(served["system_accounts"][0]["role"], "issuer/fee");
    assert_eq!(served["system_accounts"][0]["service"], "issuer");
    let text = served.to_string();
    for t in [ISSUER_TOKEN, CUSTODIAN_TOKEN, ONBOARDING_TOKEN] {
        assert!(!text.contains(t), "no token is served: {text}");
    }
}

/// `REQ-ID-6`, `REQ-AU-5` with amendment 0025: onboarding opens an identity and its account, the
/// custodian opens a further account for it and moves value, the issuer issues and acts on its own
/// system account, each with its own token, and each is refused outside its grants. No call uses
/// the operator's token.
#[tokio::test(flavor = "multi_thread")]
async fn req_id_6_req_au_5_each_service_acts_within_its_grants_and_is_refused_outside_them() {
    let s = with_services().await;
    let d = s.describe();
    let custodian = d["service_principals"]["custodian"]["principal"]
        .as_str()
        .unwrap()
        .to_owned();
    let issuer = d["service_principals"]["issuer"]["principal"]
        .as_str()
        .unwrap()
        .to_owned();

    // Onboarding, the identities principal: an identity, and its default account.
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/identities",
        "onboarding-user",
        json!({ "kind": "person", "verifier": "0a", "reference": { "registration": "reg-42" } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "onboarding opens an identity: {v}");
    // Its justification, as pass-through (amendment 0025 R1), comes back as sent.
    assert_eq!(v["reference"], json!({ "registration": "reg-42" }), "{v}");
    let user = v["result"]["identity"].as_str().unwrap().to_owned();
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/accounts",
        "onboarding-default",
        json!({ "owner": user, "kind": "customer", "custody": "delegated" }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "onboarding opens the default account: {v}"
    );
    let default = v["result"]["account"].as_str().unwrap().to_owned();

    // The custodian opens nothing for the user until the identities principal grants it.
    let virtual_body = json!({ "owner": user, "kind": "customer", "custody": "delegated" });
    refused(
        &as_(
            &s,
            CUSTODIAN_TOKEN,
            "/v1/accounts",
            "custodian-early",
            virtual_body.clone(),
        )
        .await,
        "the custodian opening an account with no grant over the identity",
    );
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/grants",
        "onboarding-grant-custodian",
        json!({
            "kind": "delegated",
            "principal": custodian,
            "subject": { "identity": user },
            "ops": ["open_account", "transfer", "reserve", "capture", "release"],
            "per_window": { "length": 86_400_000u64, "limit": "1000000" },
        }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "onboarding grants the custodian over the identity: {v}"
    );
    let identity_grant = v["result"]["grant"].as_str().unwrap().to_owned();
    let (st, v) = as_(
        &s,
        CUSTODIAN_TOKEN,
        "/v1/accounts",
        "custodian-virtual",
        virtual_body,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the custodian opens a further account: {v}"
    );
    let further = v["result"]["account"].as_str().unwrap().to_owned();
    // The opener grants the issuer its per-account grant, under its grant over the identity
    // (amendment 0025 R4).
    let (st, v) = as_(
        &s,
        CUSTODIAN_TOKEN,
        "/v1/grants",
        "custodian-grant-issuer",
        json!({
            "kind": "delegated",
            "principal": issuer,
            "subject": { "account": further },
            "ops": ["transfer"],
            "per_operation": "100",
            "parent": identity_grant,
        }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the custodian grants the issuer over the account it opened: {v}"
    );

    // The issuer issues into the default account.
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/issuances",
        "issuer-issue",
        json!({ "asset": "COPM", "destination": default, "amount": "1000" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the issuer issues: {v}");

    // The custodian moves it, a batch of two legs, between the user's accounts.
    let (st, v) = as_(
        &s,
        CUSTODIAN_TOKEN,
        "/v1/transfers",
        "custodian-batch",
        json!({ "legs": [
            { "from": default, "to": further, "asset": "COPM", "amount": "300" },
            { "from": default, "to": further, "asset": "COPM", "amount": "200" },
        ] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the custodian transfers: {v}");
    let (st, v) = get_as(
        &s,
        CUSTODIAN_TOKEN,
        &format!("/v1/balances?account={further}&asset=COPM"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["rows"][0]["available"], "500");

    // The issuer acts on its own system account, found by role.
    let (st, v) = get_as(
        &s,
        ISSUER_TOKEN,
        "/v1/aliases?source=system-role&name=issuer/fee",
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the issuer resolves its fee account: {v}"
    );
    let fee = v["account"].as_str().unwrap().to_owned();
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/issuances",
        "issuer-issue-fee",
        json!({ "asset": "COPM", "destination": fee, "amount": "50" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/transfers",
        "issuer-fee-out",
        json!({ "legs": [{ "from": fee, "to": default, "asset": "COPM", "amount": "10" }] }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the issuer moves from its system account: {v}"
    );

    // Each is refused outside its grants.
    let person = json!({ "kind": "person", "verifier": "0b" });
    refused(
        &as_(
            &s,
            CUSTODIAN_TOKEN,
            "/v1/identities",
            "custodian-id",
            person.clone(),
        )
        .await,
        "the custodian opening an identity",
    );
    refused(
        &as_(&s, ISSUER_TOKEN, "/v1/identities", "issuer-id", person).await,
        "the issuer opening an identity",
    );
    refused(
        &as_(
            &s,
            CUSTODIAN_TOKEN,
            "/v1/issuances",
            "custodian-issue",
            json!({ "asset": "COPM", "destination": default, "amount": "1" }),
        )
        .await,
        "the custodian issuing",
    );
    refused(
        &as_(
            &s,
            ONBOARDING_TOKEN,
            "/v1/issuances",
            "onboarding-issue",
            json!({ "asset": "COPM", "destination": default, "amount": "1" }),
        )
        .await,
        "onboarding issuing",
    );
    let leg =
        json!({ "legs": [{ "from": default, "to": further, "asset": "COPM", "amount": "1" }] });
    refused(
        &as_(
            &s,
            ISSUER_TOKEN,
            "/v1/transfers",
            "issuer-customer",
            leg.clone(),
        )
        .await,
        "the issuer moving a customer's value without a per-account grant",
    );
    refused(
        &as_(
            &s,
            ONBOARDING_TOKEN,
            "/v1/transfers",
            "onboarding-move",
            leg,
        )
        .await,
        "onboarding moving value",
    );

    // The issuer acts on the account the custodian opened, within the grant the custodian gave it.
    let out = |amount: &str| json!({ "legs": [{ "from": further, "to": default, "asset": "COPM", "amount": amount }] });
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/transfers",
        "issuer-under-custodian",
        out("100"),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the issuer moves within its per-account grant: {v}"
    );
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/transfers",
        "issuer-over-bound",
        out("101"),
    )
    .await;
    assert_eq!(
        v["error"], "ERR-BoundExceeded",
        "the issuer beyond its per-account bound: {st} {v}"
    );
    // Onboarding, the opener of the default account, grants the issuer over it too.
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/grants",
        "onboarding-grant-issuer",
        json!({
            "kind": "delegated",
            "principal": issuer,
            "subject": { "account": default },
            "ops": ["transfer"],
            "per_operation": "100",
        }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "onboarding grants the issuer over the default account: {v}"
    );
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/transfers",
        "issuer-default",
        json!({ "legs": [{ "from": default, "to": further, "asset": "COPM", "amount": "1" }] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // au4-q2 (amendment 0025 R3): the custodian's broad grant never reaches the issuer's system
    // account, while the issuer's account-scoped grant does (above).
    refused(
        &as_(
            &s,
            CUSTODIAN_TOKEN,
            "/v1/transfers",
            "custodian-fee",
            json!({ "legs": [{ "from": fee, "to": default, "asset": "COPM", "amount": "1" }] }),
        )
        .await,
        "the custodian moving funds out of the issuer's system account",
    );
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/transfers",
        "issuer-fee-out-again",
        json!({ "legs": [{ "from": fee, "to": default, "asset": "COPM", "amount": "1" }] }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the issuer still moves from its system account: {v}"
    );
    refused(
        &as_(
            &s,
            CUSTODIAN_TOKEN,
            "/v1/ops/operator-grants",
            "custodian-escalate",
            json!({ "principal": custodian, "kinds": ["open_identity"], "justification": "x" }),
        )
        .await,
        "the custodian granting itself the identities principal's role",
    );
}

/// Amendment 0026: the custodian reads its grant over a user's identity, `GET
/// /v1/identities/{identity}/grants`, as a holder whose grant came from elsewhere (onboarding, or
/// the chainspec for a migrated user) would: it gets the grant's id, names it as `parent` of the
/// grant it gives the issuer on an account it opened, and the issuer transfers on that account. The
/// issuer, holding no grant over the identity, is refused; onboarding, the identities principal,
/// sees every one.
#[tokio::test(flavor = "multi_thread")]
async fn req_rc_7_amendment_0026_the_custodian_reads_its_grant_over_an_identity_and_names_it_as_parent()
 {
    let s = with_services().await;
    let d = s.describe();
    let custodian = d["service_principals"]["custodian"]["principal"]
        .as_str()
        .unwrap()
        .to_owned();
    let issuer = d["service_principals"]["issuer"]["principal"]
        .as_str()
        .unwrap()
        .to_owned();

    // Onboarding opens the user and grants the custodian over the identity. The custodian is not
    // told the id.
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/identities",
        "a0026-user",
        json!({ "kind": "person", "verifier": "0a" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let user = v["result"]["identity"].as_str().unwrap().to_owned();
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/grants",
        "a0026-grant-custodian",
        json!({
            "kind": "delegated",
            "principal": custodian,
            "subject": { "identity": user },
            "ops": ["open_account", "transfer"],
            "per_window": { "length": 86_400_000u64, "limit": "1000000" },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let given = v["result"]["grant"].as_str().unwrap().to_owned();

    // The custodian opens an account for the user under that grant.
    let (st, v) = as_(
        &s,
        CUSTODIAN_TOKEN,
        "/v1/accounts",
        "a0026-virtual",
        json!({ "owner": user, "kind": "customer", "custody": "delegated" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let account = v["result"]["account"].as_str().unwrap().to_owned();
    let (st, v) = as_(
        &s,
        CUSTODIAN_TOKEN,
        "/v1/accounts",
        "a0026-other",
        json!({ "owner": user, "kind": "customer", "custody": "delegated" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let other = v["result"]["account"].as_str().unwrap().to_owned();

    // The custodian reads its grant over the identity: its id, ops, bounds and parent.
    let path = format!("/v1/identities/{user}/grants");
    let (st, v) = get_as(&s, CUSTODIAN_TOKEN, &path).await;
    assert_eq!(st, StatusCode::OK, "the custodian reads its grant: {v}");
    assert_eq!(v["identity"], json!(user));
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "its own grant only: {v}");
    let read = items[0]["grant"].as_str().unwrap().to_owned();
    assert_eq!(read, given, "the id it read is the one onboarding made");
    let terms = &items[0]["terms"];
    assert_eq!(terms["kind"], "delegated");
    assert_eq!(terms["principal"], json!(custodian));
    assert_eq!(terms["subject"], json!({ "identity": user }));
    let ops = terms["ops"].as_array().unwrap();
    assert!(
        ops.contains(&json!("open_account")) && ops.contains(&json!("transfer")),
        "{v}"
    );
    assert_eq!(terms["per_window"]["limit"], "1000000");
    assert!(terms.get("parent").is_none(), "{v}");
    assert!(items[0].get("revoked").is_none(), "{v}");

    // It names that id as the parent of the issuer's grant on the account it opened.
    let (st, v) = as_(
        &s,
        CUSTODIAN_TOKEN,
        "/v1/grants",
        "a0026-grant-issuer",
        json!({
            "kind": "delegated",
            "principal": issuer,
            "subject": { "account": account },
            "ops": ["transfer"],
            "per_operation": "100",
            "parent": read,
        }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the custodian grants the issuer with the parent it read: {v}"
    );

    // The issuer transfers on that account, within the grant.
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/issuances",
        "a0026-issue",
        json!({ "asset": "COPM", "destination": account, "amount": "500" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = as_(
        &s,
        ISSUER_TOKEN,
        "/v1/transfers",
        "a0026-issuer-moves",
        json!({ "legs": [{ "from": account, "to": other, "asset": "COPM", "amount": "100" }] }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the issuer transfers on the account: {v}"
    );
    let (st, v) = get_as(
        &s,
        ISSUER_TOKEN,
        &format!("/v1/balances?account={account}&asset=COPM"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["rows"][0]["available"], "400");

    // The issuer holds no grant over the identity: refused. Onboarding, the identities principal,
    // and The operator see every grant over it.
    let (st, v) = get_as(&s, ISSUER_TOKEN, &path).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "ERR-NotAuthorised", "{v}");
    let (st, v) = get_as(&s, ONBOARDING_TOKEN, &path).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["grant"], json!(given), "{v}");
    let (st, v) = get_as(&s, TEST_OPERATOR_TOKEN, &path).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["items"].as_array().map(Vec::len), Some(1), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn req_au_8_a_service_token_that_is_not_set_or_is_the_operators_fails_the_start() {
    let services = parse_principals(
        r#"{"principals": [{"name": "issuer", "role": "issuance_authority",
                            "token_env": "ISSUER_TOKEN"}]}"#,
    )
    .unwrap();
    let start = |lookup: fn(&str) -> Option<String>, accounts: &str| {
        let options = Options {
            services: services.clone(),
            system_accounts: parse_system_accounts(accounts).unwrap(),
            ..Options::default()
        };
        async move {
            match Sidecar::start_with_lookup(
                parse_assets(DEFAULT_ASSETS).unwrap(),
                &options,
                lookup,
            )
            .await
            {
                Ok(_) => panic!("started"),
                Err(e) => e,
            }
        }
    };
    let e = start(|_| None, "").await;
    assert!(e.contains("ISSUER_TOKEN is not set"), "{e}");
    let e = start(|_| Some(TEST_OPERATOR_TOKEN.to_owned()), "").await;
    assert!(e.contains("another principal's token"), "{e}");
    assert!(!e.contains(TEST_OPERATOR_TOKEN), "{e}");
    let e = start(|_| Some("fine".to_owned()), "custodian/x=custodian").await;
    assert!(e.contains("not a service principal"), "{e}");
}

// ------------------------------------------------------------------ passkeys

/// `F-028`: the sidecar accepts passkeys of its relying party, `localhost` by default, and issues
/// challenges for a registration and a sign-in without a token. `/sidecar` shows the party.
#[tokio::test(flavor = "multi_thread")]
async fn f_028_the_sidecar_accepts_passkeys_of_its_relying_party() {
    let s = started().await;
    let (_, served) = ingress::testkit::call(&s.app(), "GET", "/sidecar", None, &[], None).await;
    assert_eq!(
        served["passkeys"],
        json!({ "rp_id": "localhost", "origins": ["http://localhost"] })
    );
    for purpose in ["register", "authenticate"] {
        let (st, v) = ingress::testkit::call(
            &s.router,
            "POST",
            "/v1/auth/challenge",
            None,
            &[],
            Some(json!({ "purpose": purpose })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{purpose}: {v}");
        assert!(v["challenge"].is_string(), "{v}");
    }

    let s = started_with(Options {
        rp_id: Some("ci.example.test".into()),
        rp_origins: aledgent_sidecar::parse_origins(
            "https://ci.example.test, http://localhost:3000",
        ),
        ..Options::default()
    })
    .await;
    assert_eq!(s.describe()["passkeys"]["rp_id"], "ci.example.test");
    assert_eq!(
        s.describe()["passkeys"]["origins"],
        json!(["https://ci.example.test", "http://localhost:3000"])
    );
    for bad in [
        Options {
            rp_id: Some("Not A Host".into()),
            ..Options::default()
        },
        Options {
            rp_origins: vec!["http://ci.example.test".into()],
            ..Options::default()
        },
    ] {
        let r = Sidecar::start_with(parse_assets(DEFAULT_ASSETS).unwrap(), &bad).await;
        assert!(
            r.is_err(),
            "a relying party ingress would refuse fails the start"
        );
    }
}

/// Amendment 0025 clause R2 (ruling `pk-register`), end to end: onboarding opens a user, the user's
/// device runs the WebAuthn ceremony on an ingress-issued `register` challenge, and onboarding
/// authorises adding the passkey to the user's set, acting for the user. The user then signs in
/// with it and answers as itself. The custodian, without the role, cannot do the same.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_20_amendment_0025_r2_onboarding_registers_a_users_passkey_and_the_user_signs_in() {
    use authn::webauthn::testing::Soft;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    const ORIGIN: &str = "http://localhost";
    let b64 = |b: &[u8]| URL_SAFE_NO_PAD.encode(b);

    let s = with_services().await;
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/identities",
        "onboarding-passkey-user",
        json!({ "kind": "person", "verifier": "0a" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let user = v["result"]["identity"].as_str().unwrap().to_owned();
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/accounts",
        "onboarding-passkey-account",
        json!({ "owner": user, "kind": "customer", "custody": "delegated" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let challenge = |purpose: &'static str| {
        let router = s.router.clone();
        async move {
            let (st, v) = ingress::testkit::call(
                &router,
                "POST",
                "/v1/auth/challenge",
                None,
                &[],
                Some(json!({ "purpose": purpose })),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{v}");
            URL_SAFE_NO_PAD
                .decode(v["challenge"].as_str().unwrap())
                .unwrap()
        }
    };
    let soft = Soft::new(21, "localhost");
    let register = |by: &'static str, key: &'static str, ch: Vec<u8>| {
        let (cd, att) = soft.register(&ch, ORIGIN);
        let router = s.router.clone();
        let user = user.clone();
        async move {
            ingress::testkit::call(
                &router,
                "POST",
                "/v1/credentials",
                Some(by),
                &[("client-key", key), ("on-behalf-of", &user)],
                Some(json!({
                    "marker": "ordinary",
                    "scheme": "webauthn-es256",
                    "passkey": { "client_data_json": b64(&cd), "attestation_object": b64(&att) },
                })),
            )
            .await
        }
    };

    // The custodian does not hold the role.
    refused(
        &register(
            CUSTODIAN_TOKEN,
            "custodian-passkey",
            challenge("register").await,
        )
        .await,
        "the custodian registering a user's passkey",
    );
    let (st, v) = register(
        ONBOARDING_TOKEN,
        "onboarding-passkey",
        challenge("register").await,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "onboarding registers the user's passkey: {v}"
    );
    let credential = v["result"]["credential"].as_str().unwrap().to_owned();

    // The user signs in with it, and its session answers as the user.
    let ch = challenge("authenticate").await;
    let (ad, cd, sig) = soft.assert(&ch, ORIGIN);
    let (st, v) = ingress::testkit::call(
        &s.router,
        "POST",
        "/v1/auth/passkey",
        None,
        &[],
        Some(json!({
            "credential": credential,
            "assertion": {
                "raw_id": b64(&soft.raw_id),
                "authenticator_data": b64(&ad),
                "client_data_json": b64(&cd),
                "signature": b64(&sig),
            },
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "sign-in: {v}");
    assert_eq!(v["principal"], user.as_str());
    let session = v["session"].as_str().unwrap().to_owned();
    let (st, v) = get_as(&s, &session, "/v1/principals/self/answering").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["principal"], user.as_str(), "the session is the user's");
}

/// Amendment 0027 over clause R2: a `register` challenge issued for one user serves onboarding's
/// registration into that user's set only, and once. Bound to user A, it is refused for user B.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_20_amendment_0027_a_challenge_bound_to_one_user_does_not_register_on_another() {
    use authn::webauthn::testing::Soft;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    const ORIGIN: &str = "http://localhost";
    let b64 = |b: &[u8]| URL_SAFE_NO_PAD.encode(b);

    let s = with_services().await;
    let mut users = Vec::new();
    for name in ["a", "b"] {
        let (st, v) = as_(
            &s,
            ONBOARDING_TOKEN,
            "/v1/identities",
            &format!("bound-user-{name}"),
            json!({ "kind": "person", "verifier": "0a" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let user = v["result"]["identity"].as_str().unwrap().to_owned();
        let (st, v) = as_(
            &s,
            ONBOARDING_TOKEN,
            "/v1/accounts",
            &format!("bound-account-{name}"),
            json!({ "owner": user, "kind": "customer", "custody": "delegated" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        users.push(user);
    }
    let (a, b) = (&users[0], &users[1]);

    let (st, v) = ingress::testkit::call(
        &s.router,
        "POST",
        "/v1/auth/challenge",
        None,
        &[],
        Some(json!({ "purpose": "register", "identity": a })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let ch = URL_SAFE_NO_PAD
        .decode(v["challenge"].as_str().unwrap())
        .unwrap();
    let (cd, att) = Soft::new(22, "localhost").register(&ch, ORIGIN);
    let body = json!({
        "marker": "ordinary",
        "scheme": "webauthn-es256",
        "passkey": { "client_data_json": b64(&cd), "attestation_object": b64(&att) },
    });
    let register = |key: &'static str, user: &str| {
        let router = s.router.clone();
        let user = user.to_owned();
        let body = body.clone();
        async move {
            ingress::testkit::call(
                &router,
                "POST",
                "/v1/credentials",
                Some(ONBOARDING_TOKEN),
                &[("client-key", key), ("on-behalf-of", &user)],
                Some(body),
            )
            .await
        }
    };

    let (_, v) = register("bound-on-b", b).await;
    assert_eq!(
        v["error"], "ERR-CredentialInvalid",
        "bound to A, for B: {v}"
    );
    let (st, v) = register("bound-on-a", a).await;
    assert_eq!(st, StatusCode::OK, "bound to A, for A: {v}");
    let (_, v) = register("bound-on-a-again", a).await;
    assert_eq!(v["error"], "ERR-CredentialInvalid", "replayed: {v}");
}

/// Amendment 0028: the custodian, holding a grant over a user's identity, reads an account opened
/// for that user, `GET /v1/accounts/{account}`, and sees the user as its owner before it adopts the
/// account as the user's default; another user's account shows that user, not this one. An account
/// outside its reach — the issuer's fee account, which the operator owns and no grant over every
/// delegated account covers — is refused as an account that does not exist.
///
/// In this sidecar the custodian also holds its operator grant over every account in delegated
/// custody, which reaches both users' accounts; ingress's own test isolates the grant over the
/// identity.
#[tokio::test(flavor = "multi_thread")]
async fn req_rc_7_amendment_0028_the_custodian_reads_the_owner_of_an_account_opened_for_its_user() {
    let s = with_services().await;
    let d = s.describe();
    let custodian = d["service_principals"]["custodian"]["principal"]
        .as_str()
        .unwrap()
        .to_owned();

    // Onboarding opens two users and grants the custodian over the first one's identity.
    let mut users = Vec::new();
    for key in ["a0028-user", "a0028-other-user"] {
        let (st, v) = as_(
            &s,
            ONBOARDING_TOKEN,
            "/v1/identities",
            key,
            json!({ "kind": "person", "verifier": "0a" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        users.push(v["result"]["identity"].as_str().unwrap().to_owned());
    }
    let (user, other_user) = (users[0].clone(), users[1].clone());
    let (st, v) = as_(
        &s,
        ONBOARDING_TOKEN,
        "/v1/grants",
        "a0028-grant-custodian",
        json!({
            "kind": "delegated",
            "principal": custodian,
            "subject": { "identity": user },
            "ops": ["open_account", "transfer"],
            "per_window": { "length": 86_400_000u64, "limit": "1000000" },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // Onboarding opens an account for each user, as it does at registration.
    let mut accounts = Vec::new();
    for (key, owner) in [
        ("a0028-account", &user),
        ("a0028-other-account", &other_user),
    ] {
        let (st, v) = as_(
            &s,
            ONBOARDING_TOKEN,
            "/v1/accounts",
            key,
            json!({ "owner": owner, "kind": "customer", "custody": "delegated" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        accounts.push(v["result"]["account"].as_str().unwrap().to_owned());
    }

    // The custodian reads the user's account and sees the user as its owner.
    let (st, v) = get_as(
        &s,
        CUSTODIAN_TOKEN,
        &format!("/v1/accounts/{}", accounts[0]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the custodian reads the account: {v}");
    assert_eq!(v["account"], json!(accounts[0]));
    assert_eq!(v["owner"], json!(user), "the owner is the user: {v}");
    assert!(user.starts_with("did:aledgent:id:"), "{user}");
    assert_eq!(v["kind"], "customer");
    assert_eq!(v["custody"], "delegated");
    assert_eq!(v["closed"], false);
    assert!(v["opened_at"].is_u64(), "{v}");

    // Another user's account names that user: adopting it for this one would fail the check.
    let (st, v) = get_as(
        &s,
        CUSTODIAN_TOKEN,
        &format!("/v1/accounts/{}", accounts[1]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["owner"], json!(other_user), "{v}");
    assert_ne!(v["owner"], json!(user), "{v}");

    // An unrelated account, outside the custodian's reach: refused as one that does not exist.
    let (st, v) = get_as(
        &s,
        ISSUER_TOKEN,
        "/v1/aliases?source=system-role&name=issuer/fee",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let fee = v["account"].as_str().unwrap().to_owned();
    let r = get_as(&s, CUSTODIAN_TOKEN, &format!("/v1/accounts/{fee}")).await;
    assert_eq!(r.0, StatusCode::NOT_FOUND, "{}", r.1);
    assert_eq!(r.1["error"], "ERR-UnknownAccount", "{}", r.1);
    // The issuer, which holds its grant over that account, reads it: the operator owns it.
    let (st, v) = get_as(&s, ISSUER_TOKEN, &format!("/v1/accounts/{fee}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_ne!(v["owner"], json!(user), "{v}");
}
