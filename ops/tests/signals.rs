// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The signal exporter (`T-018-04`, `NFR-11`, `REQ-OP-8`, `REQ-EV-6`, `REQ-SA-7`, `REQ-SN-2`,
//! `REQ-PV-5`, `AC-C4.2`): over a running deployment, all six signals and the three extras are
//! scraped; the core's signal query goes through ingress as the operations principal and is
//! refused to anyone else; and nothing scraped is a ledger fact.

use std::sync::Mutex;
use std::time::Duration;

use ingress::clock::Clock as _;
use ingress::testkit::call;
use ops::exporter::{Exporter, Fixed, Records, RouterSource};
use ops::kit::{OPS_TOKEN, OpsKit};
use ops::signals::{DEFERRED, SIGNALS, parse};

type MemKit = OpsKit<ingress::store::mem::MemStore, settle::store::mem::MemStore>;

async fn kit(dir: &std::path::Path) -> MemKit {
    OpsKit::start(
        ingress::store::mem::MemStore::new(Duration::from_millis(50)),
        settle::store::mem::MemStore::new(),
        &dir.join("audit"),
        &dir.join("book"),
    )
    .await
}

/// `AC-C4.2`, `NFR-11`: every signal is scraped, with its metrics; the reply of the core's
/// signal query carries counts and ages only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_c4_2_all_six_signals_and_the_three_extras_are_scraped_and_hold_no_ledger_fact() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    k.round().await;
    // A reservation left open: the oldest non-terminal operation has an age.
    let b = k.parties[1].clone();
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/reservations",
            Some(&b.token),
            &[("client-key", "open")],
            Some(serde_json::json!({ "payer": b.account, "asset": "USD", "amount": "5" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    k.d.clock.advance(1_000);
    // The core ticks at the clock's time before anything is read: the ticker's age is then zero.
    k.tick_to_now().await;
    assert!(k.d.quiesce(ops::kit::PATIENCE).await);
    // The records the exporter reads: a backup, a reconciliation run, a drill.
    let backups = tmp.path().join("backups");
    k.backup_core(&backups, "b1").await.unwrap();
    let runs = tmp.path().join("runs");
    ops::reconcile::Run {
        at: k.d.clock.now(),
        axes: (1..=3)
            .map(|axis| ops::reconcile::AxisResult {
                axis,
                ran: axis != 3,
                findings: Vec::new(),
            })
            .collect(),
        suspense: Vec::new(),
        audited: None,
    }
    .record(&runs)
    .unwrap();
    let adapter = k.adapter.signals(k.d.clock.now()).await.unwrap();
    assert!(adapter.serving && adapter.lag() == 0);
    let exporter = Exporter::new(
        RouterSource::new(k.d.router.clone(), OPS_TOKEN),
        Fixed(Mutex::new(Some(adapter))),
        Records {
            reconciliation: Some(runs),
            backups: Some(backups),
            drills: None,
            core_archive: None,
        },
        "settlement-operations",
        ops::kit::PATIENCE,
    );
    let now = k.d.clock.now();
    let text = exporter.scrape(now).await;
    let samples = parse(&text);
    let names: std::collections::BTreeSet<&str> =
        samples.iter().map(|(n, _, _)| n.as_str()).collect();
    for (signal, metrics) in SIGNALS {
        for m in *metrics {
            assert!(names.contains(m), "{signal}: {m} is not scraped:\n{text}");
        }
    }
    for m in DEFERRED {
        assert!(!names.contains(m), "{m} is deferred");
    }
    let value = |name: &str| {
        samples
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, _, v)| *v)
            .unwrap()
    };
    assert_eq!(value("aledgent_signals_up"), 1);
    assert!(value("aledgent_oldest_non_terminal_age_ms") >= 1_000);
    assert!(value("aledgent_operations_open") >= 1);
    assert!(value("aledgent_ticker_age_ms") < 1_000);
    assert_eq!(value("aledgent_observation_cursor_lag"), 0);
    assert!(value("aledgent_position") > 0);
    // The review queue carries its owner.
    assert!(samples.iter().any(|(n, l, _)| n == "aledgent_review_queue"
        && l.get("owner").map(String::as_str) == Some("settlement-operations")));
    // No ledger fact: no identifier, no account, no operation reference, no customer amount.
    assert!(!text.contains("did:"), "{text}");
    for p in &k.parties {
        let acct = p.account.as_deref().unwrap();
        assert!(!text.contains(acct));
        assert!(!text.contains(&p.identity));
    }
    // The signal query itself answers counts and ages, no identifier.
    let (s, body) = call(
        &k.d.router,
        "GET",
        "/v1/ops/signals",
        Some(OPS_TOKEN),
        &[],
        None,
    )
    .await;
    assert_eq!(s.as_u16(), 200);
    assert!(!body.to_string().contains("did:"), "{body}");
}

/// `REQ-PV-5`, `REQ-OS-1`: the signal query is refused to a customer, to the operator's
/// organisation without the `read_signals` kind, and without a credential; granted, it answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_pv_5_the_signal_query_is_refused_to_anyone_without_the_read_signals_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let k = kit(tmp.path()).await;
    let r = &k.d.router;
    let (s, _) = call(r, "GET", "/v1/ops/signals", None, &[], None).await;
    assert_eq!(s.as_u16(), 401);
    let alice = &k.parties[0];
    let (s, v) = call(r, "GET", "/v1/ops/signals", Some(&alice.token), &[], None).await;
    assert_eq!(s.as_u16(), 403, "{v}");
    let op = k.d.operator.token.clone();
    let (s, v) = call(r, "GET", "/v1/ops/signals", Some(&op), &[], None).await;
    assert_eq!(
        s.as_u16(),
        403,
        "the operator holds no read_signals grant: {v}"
    );
    let (s, v) = call(r, "GET", "/v1/ops/signals", Some(OPS_TOKEN), &[], None).await;
    assert_eq!(s.as_u16(), 200, "{v}");
    // A core that does not answer is a source down, not a stale value.
    k.d.core.stop();
    let exporter = Exporter::new(
        RouterSource::new(k.d.router.clone(), OPS_TOKEN),
        Fixed(Mutex::new(None)),
        Records::default(),
        "settlement-operations",
        Duration::from_millis(200),
    );
    let samples = parse(&exporter.scrape(1).await);
    let up: Vec<_> = samples
        .iter()
        .filter(|(n, _, _)| n == "aledgent_signals_up")
        .map(|(_, l, v)| (l["source"].clone(), *v))
        .collect();
    assert_eq!(up, vec![("core".to_owned(), 0), ("adapter".to_owned(), 0)]);
}

/// `T-018-11` (the trigger of `T-036-17`): the exporter writes the book's size as three gauges,
/// the chainspec's records only for a book founded on one, and none for a core that does not
/// count it.
#[test]
fn t_018_11_the_book_size_is_exported_as_gauges() {
    use ingress::query::{BookSizeBody, SignalsBody};
    use ops::signals::{Snapshot, render};
    let with = |book| Snapshot {
        core: Some(SignalsBody {
            book,
            ..SignalsBody::default()
        }),
        ..Snapshot::default()
    };
    let seeded = render(&with(Some(BookSizeBody {
        records: 1_744_213,
        leaves: 7_000_000,
        genesis_records: Some(581_404),
    })));
    for line in [
        "# TYPE aledgent_book_records gauge",
        "aledgent_book_records 1744213",
        "# TYPE aledgent_book_leaves gauge",
        "aledgent_book_leaves 7000000",
        "# TYPE aledgent_book_genesis_records gauge",
        "aledgent_book_genesis_records 581404",
    ] {
        assert!(seeded.lines().any(|l| l == line), "{line}:\n{seeded}");
    }
    let unseeded = render(&with(Some(BookSizeBody {
        records: 3,
        leaves: 12,
        genesis_records: None,
    })));
    assert!(unseeded.lines().any(|l| l == "aledgent_book_records 3"));
    assert!(!unseeded.contains("aledgent_book_genesis_records"));
    let none = render(&with(None));
    assert!(!none.contains("aledgent_book_"), "{none}");
}

/// `T-018-11`: over a running deployment, the core counts the book through ingress, and an
/// operation adds one record to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_018_11_an_operation_adds_one_record_to_the_scraped_book() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    k.round().await;
    assert!(k.d.quiesce(ops::kit::PATIENCE).await);
    let exporter = Exporter::new(
        RouterSource::new(k.d.router.clone(), OPS_TOKEN),
        Fixed(Mutex::new(None)),
        Records::default(),
        "settlement-operations",
        ops::kit::PATIENCE,
    );
    let book = |text: &str| {
        let samples = parse(text);
        let value = |name: &str| {
            samples
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, _, v)| *v)
                .unwrap_or_else(|| panic!("{name} is not scraped:\n{text}"))
        };
        assert!(!text.contains("aledgent_book_genesis_records"), "{text}");
        (
            value("aledgent_book_records"),
            value("aledgent_book_leaves"),
        )
    };
    let (records, leaves) = book(&exporter.scrape(k.d.clock.now()).await);
    assert!(records > 0 && leaves > records, "{records} {leaves}");
    let b = k.parties[1].clone();
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/reservations",
            Some(&b.token),
            &[("client-key", "book-size")],
            Some(serde_json::json!({ "payer": b.account, "asset": "USD", "amount": "5" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert!(k.d.quiesce(ops::kit::PATIENCE).await);
    let (after, grown) = book(&exporter.scrape(k.d.clock.now()).await);
    assert_eq!(after, records + 1);
    assert!(grown > leaves);
}

/// `T-037-06`: the code fetcher's discarded answers are exported as a counter with one label, the
/// place's name, each place listed even at zero; a node with no source of code exports none.
#[test]
fn t037_06_the_code_source_mismatches_are_exported_by_source() {
    use ingress::query::{CodeSourceMismatchBody, SignalsBody};
    use ops::signals::{Snapshot, render};
    let with = |code_source_mismatches| Snapshot {
        core: Some(SignalsBody {
            code_source_mismatches,
            ..SignalsBody::default()
        }),
        ..Snapshot::default()
    };
    let body = |source: &str, count| CodeSourceMismatchBody {
        source: source.into(),
        count,
    };
    let text = render(&with(vec![
        body("cache", 0),
        body("fs:0", 2),
        body("fs:1", 0),
    ]));
    for line in [
        "# TYPE aledgent_code_source_mismatch_total counter",
        r#"aledgent_code_source_mismatch_total{source="cache"} 0"#,
        r#"aledgent_code_source_mismatch_total{source="fs:0"} 2"#,
        r#"aledgent_code_source_mismatch_total{source="fs:1"} 0"#,
    ] {
        assert!(text.lines().any(|l| l == line), "{line}:\n{text}");
    }
    let none = render(&with(Vec::new()));
    assert!(
        !none.contains("aledgent_code_source_mismatch_total"),
        "{none}"
    );
}
