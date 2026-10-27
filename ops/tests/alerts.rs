// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Alert rules, paging routes and dashboards (`T-018-05`, `NFR-9`, `NFR-11`, `REQ-OP-6`,
//! `AC-C4.1`, `RISK-7`): each signal pushed over its line pages the test route, through the
//! committed rules (`ops/prometheus/rules/aledgent.yml`) and routes
//! (`ops/alertmanager/alertmanager.yml`); a healthy deployment pages no one; the dashboards show
//! every signal, and the Needs-Review queue with its owner and service level.
//!
//! There is no staging environment and no pager here: the rules are evaluated by `ops::alerts`
//! and the `pager` receiver is the test route. The staging run is deferred (NOTES.md).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use core_archive::ArchiveStatus;
use core_archive::archiver::CheckpointAt;
use ingress::clock::Clock as _;
use ingress::query::{AuditShipBody, BookSizeBody, IncidentCountBody, SignalsBody};
use ops::alerts::{Evaluator, Routing, Rules, TestRoute};
use ops::backup::StoreKind;
use ops::exporter::{Exporter, Fixed, Records, RouterSource};
use ops::kit::{OPS_TOKEN, OpsKit};
use ops::signals::{ANCHORING, AxisSignal, DEFERRED, RELEASE, SIGNALS, Snapshot, parse, render};
use settle::signals::{AdapterSignals, LaneSignal, PositionSignal};

const NOW: u64 = 1_900_000_000_000;

/// A healthy deployment's snapshot: every signal inside its line.
fn healthy() -> Snapshot {
    Snapshot {
        at: NOW,
        core: Some(SignalsBody {
            now: NOW,
            open: 3,
            oldest_open_age: Some(2_000),
            received: 0,
            review: 0,
            needs_review: 0,
            review_overdue: 0,
            review_most_overdue: 0,
            review_service_level: 3_600_000,
            undelivered: 1,
            oldest_undelivered_age: Some(500),
            tick_age: Some(800),
            oldest_undecided_age: None,
            incidents: vec![IncidentCountBody {
                kind: "audit_gap".into(),
                count: 1,
                latest: NOW - 86_400_000,
            }],
            anchor_lag: Some(1_800_000),
            anchor_in_flight_age: Some(60_000),
            anchor_incidents: Vec::new(),
            settlement_differing: 0,
            settlement_comparison_age: Some(1_800_000),
            completion_p50: Some(400),
            completion_p99: Some(19_000),
            core_halted: false,
            audit: Some(AuditShipBody {
                unshipped: 3,
                oldest_unshipped_age: Some(400),
                shipped: 10_000,
                store_up: true,
                refused: false,
                backpressure: false,
                max_unshipped: 65_536,
            }),
            vos_unacknowledged: Some(0),
            // Fenced, but well inside `fence_alert_after`.
            writer_fenced_age: Some(299_000),
            code_source_mismatches: Vec::new(),
            book: Some(BookSizeBody {
                records: 600_000,
                leaves: 2_400_000,
                genesis_records: Some(581_404),
            }),
            writer_code: Some(Box::new(ingress::query::WriterCodeBody {
                code: "a0".repeat(32),
                generation: 1,
                upgrade: None,
            })),
        }),
        adapter: Some(AdapterSignals {
            at: NOW - 1_000,
            serving: true,
            instance: 1,
            finalized: 120,
            best: 123,
            inbox: 40,
            cursor: 40,
            gaps: 0,
            alerts: BTreeMap::new(),
            lanes: vec![LaneSignal {
                lane: "5e".repeat(32),
                native: "1000000".into(),
                under_floor: false,
            }],
            fee_floor: "100".into(),
            positions: vec![PositionSignal {
                asset: "USD".into(),
                position: "10000000".into(),
            }],
        }),
        finalized_stall: Some(6_000),
        reconciliation: (1..=3)
            .map(|axis| AxisSignal {
                axis,
                last_run: Some(NOW - 3_600_000),
                differences: 0,
                ran: true,
            })
            .collect(),
        suspense: Some(ops::signals::SuspenseSignal {
            nonzero: 1,
            oldest_age: 3_600_000,
        }),
        backups: StoreKind::ALL
            .into_iter()
            .map(|s| (s, NOW - 600_000))
            .collect(),
        drill: Some((NOW - 86_400_000, true)),
        core_archive: Some(Some(ArchiveStatus {
            at: NOW - 500,
            sink: "fs:/archive".into(),
            segments: 40,
            point: None,
            committed_input: 1_000,
            lag_inputs: 3,
            lag_ms: 400,
            pending_bytes: 900,
            failures: 0,
            last_error: None,
            conflict: None,
            checkpoint: Some(CheckpointAt {
                n: 30,
                entries: 700,
                at: NOW - 600_000,
            }),
        })),
        review_owner: "settlement-operations".into(),
    }
}

/// Evaluate `s` twice, `for_ms` apart (every rule's `for` is at most ten minutes), and route
/// what fires: what the pager's test route received, and what went to the dashboard.
fn page(s: &Snapshot) -> (TestRoute, TestRoute) {
    page_with(s, &[])
}

/// [`page`], with samples of metrics nothing exports yet (`DEFERRED`) beside the snapshot's.
fn page_with(
    s: &Snapshot,
    extra: &[(String, BTreeMap<String, String>, i128)],
) -> (TestRoute, TestRoute) {
    let rules = Rules::committed().unwrap();
    let routing = Routing::committed().unwrap();
    let mut ev = Evaluator::new(rules);
    let mut samples = parse(&render(s));
    samples.extend(extra.iter().cloned());
    ev.step(s.at, &samples);
    let firing = ev.step(s.at + 600_000, &samples);
    let (mut pager, mut dashboard) = (TestRoute::default(), TestRoute::default());
    pager.deliver(&routing, "pager", &firing);
    dashboard.deliver(&routing, "dashboard", &firing);
    (pager, dashboard)
}

/// A healthy deployment pages no one and warns no one.
#[test]
fn nfr_11_a_healthy_deployment_pages_no_one() {
    let (pager, dashboard) = page(&healthy());
    assert!(pager.delivered.is_empty(), "{:#?}", pager.delivered);
    assert!(dashboard.delivered.is_empty(), "{:#?}", dashboard.delivered);
}

/// `NFR-11`, `NFR-9`, `REQ-OP-6`, `RISK-7`: each signal, pushed over its line, pages the test
/// route with its signal and owner.
#[test]
fn nfr_11_each_signal_over_its_line_pages_the_test_route() {
    type Push = fn(&mut Snapshot);
    let cases: &[(&str, &str, Push)] = &[
        (
            "AledgentOldestNonTerminalOverServiceLevel",
            "oldest-non-terminal",
            |s| s.core.as_mut().unwrap().oldest_open_age = Some(3_600_001),
        ),
        (
            "AledgentReceivedOverDecisionBound",
            "oldest-non-terminal",
            |s| s.core.as_mut().unwrap().oldest_undecided_age = Some(60_001),
        ),
        ("AledgentObservationGap", "observation-completeness", |s| {
            s.adapter.as_mut().unwrap().alerts.insert("gap".into(), 1);
        }),
        (
            "AledgentSettlementChainMismatch",
            "observation-completeness",
            |s| {
                s.adapter.as_mut().unwrap().alerts.insert("chain".into(), 1);
            },
        ),
        (
            "AledgentObservationStopped",
            "observation-completeness",
            |s| {
                s.adapter
                    .as_mut()
                    .unwrap()
                    .alerts
                    .insert("unreadable".into(), 1);
            },
        ),
        (
            "AledgentObservationStopped",
            "observation-completeness",
            |s| {
                s.adapter.as_mut().unwrap().alerts.insert("reset".into(), 1);
            },
        ),
        ("AledgentObservationLag", "observation-completeness", |s| {
            s.adapter.as_mut().unwrap().cursor = 0;
            s.adapter.as_mut().unwrap().inbox = 101;
        }),
        (
            "AledgentUndeliveredNotification",
            "undelivered-notifications",
            |s| s.core.as_mut().unwrap().oldest_undelivered_age = Some(900_001),
        ),
        ("AledgentReconciliationDifference", "reconciliation", |s| {
            s.reconciliation[0].differences = 1
        }),
        ("AledgentReconciliationNotRun", "reconciliation", |s| {
            s.reconciliation[2].last_run = None
        }),
        ("AledgentLiquidityShortfall", "liquidity", |s| {
            s.adapter
                .as_mut()
                .unwrap()
                .alerts
                .insert("position".into(), 1);
        }),
        ("AledgentLaneUnderFeeFloor", "liquidity", |s| {
            s.adapter.as_mut().unwrap().lanes[0].under_floor = true
        }),
        ("AledgentTickerStale", "ticker", |s| {
            s.core.as_mut().unwrap().tick_age = Some(30_001)
        }),
        // REQ-RT-20, T-037-15: a writer fenced past `fence_alert_after` (five minutes).
        ("AledgentWriterFenced", "writer-fence", |s| {
            s.core.as_mut().unwrap().writer_fenced_age = Some(301_000)
        }),
        ("AledgentReviewOverdue", "needs-review", |s| {
            let c = s.core.as_mut().unwrap();
            c.review = 1;
            c.needs_review = 1;
            c.review_overdue = 1;
            c.review_most_overdue = 1;
        }),
        ("AledgentBlockProductionStalled", "block-production", |s| {
            s.finalized_stall = Some(120_001)
        }),
        ("AledgentSignalsDown", "exporter", |s| s.core = None),
        ("AledgentAdapterSignalsStale", "exporter", |s| {
            s.adapter.as_mut().unwrap().at = NOW - 60_001
        }),
        ("AledgentAuditGap", "audit", |s| {
            s.core.as_mut().unwrap().incidents[0].latest = NOW - 1
        }),
        // NFR-15 (amendment 0007): the 30 s ceiling pages; the 19 s target of `healthy` does not.
        ("AledgentCompletionOverCeiling", "completion", |s| {
            s.core.as_mut().unwrap().completion_p99 = Some(30_001)
        }),
        // REQ-ER-2 (amendment 0006 item 8): an internal fault pages.
        ("AledgentInternalFault", "audit", |s| {
            s.core.as_mut().unwrap().incidents.push(IncidentCountBody {
                kind: "internal_fault".into(),
                count: 1,
                latest: NOW - 1,
            })
        }),
        ("AledgentCoreBackupLate", "backup", |s| {
            s.backups.insert(StoreKind::Core, NOW - 7_200_001);
        }),
        ("AledgentAuditBackupLate", "backup", |s| {
            s.backups.remove(&StoreKind::Audit);
        }),
        ("AledgentIngressBackupLate", "backup", |s| {
            s.backups.insert(StoreKind::Ingress, NOW - 172_800_001);
        }),
        ("AledgentAdapterBackupLate", "backup", |s| {
            s.backups.insert(StoreKind::Adapter, NOW - 172_800_001);
        }),
        ("AledgentAnchorLag", "anchor-lag", |s| {
            s.core.as_mut().unwrap().anchor_lag = Some(7_200_001)
        }),
        ("AledgentAnchorStuck", "anchor-lag", |s| {
            s.core.as_mut().unwrap().anchor_in_flight_age = Some(3_600_001)
        }),
        ("AledgentAnchorCommitmentMismatch", "anchoring", |s| {
            s.core.as_mut().unwrap().anchor_incidents = vec![IncidentCountBody {
                kind: "commitment_mismatch".into(),
                count: 1,
                latest: 0,
            }]
        }),
        ("AledgentUnknownAnchor", "anchoring", |s| {
            s.core.as_mut().unwrap().anchor_incidents = vec![IncidentCountBody {
                kind: "unknown_anchor".into(),
                count: 1,
                latest: 0,
            }]
        }),
        ("AledgentSettlementDifference", "reconciliation", |s| {
            s.core.as_mut().unwrap().settlement_differing = 1
        }),
        ("AledgentSuspenseNotReconciled", "suspense", |s| {
            s.suspense = Some(ops::signals::SuspenseSignal {
                nonzero: 1,
                oldest_age: 86_400_001,
            })
        }),
        ("AledgentDrillFailed", "drill", |s| {
            s.drill = Some((NOW, false))
        }),
        // T-018-10: the release's blockers.
        ("AledgentCoreHalted", "core-halt", |s| {
            s.core.as_mut().unwrap().core_halted = true
        }),
        ("AledgentAuditNotShipped", "audit-shipping", |s| {
            s.core.as_mut().unwrap().audit = None
        }),
        ("AledgentAuditShipLag", "audit-shipping", |s| {
            s.core
                .as_mut()
                .unwrap()
                .audit
                .as_mut()
                .unwrap()
                .oldest_unshipped_age = Some(300_001)
        }),
        ("AledgentAuditStoreDown", "audit-shipping", |s| {
            s.core.as_mut().unwrap().audit.as_mut().unwrap().store_up = false
        }),
        ("AledgentAuditStoreRefused", "audit-shipping", |s| {
            s.core.as_mut().unwrap().audit.as_mut().unwrap().refused = true
        }),
        ("AledgentAuditBackpressure", "audit-shipping", |s| {
            s.core
                .as_mut()
                .unwrap()
                .audit
                .as_mut()
                .unwrap()
                .backpressure = true
        }),
        ("AledgentVosAuthorityBacklog", "vos-authority", |s| {
            s.core.as_mut().unwrap().vos_unacknowledged = Some(192)
        }),
        ("AledgentAdapterNotServing", "settlement-adapter", |s| {
            s.adapter.as_mut().unwrap().serving = false
        }),
        // F-027 NOTES decision 52: a runtime the layer has not confirmed.
        (
            "AledgentAdapterRuntimeUnconfirmed",
            "settlement-adapter",
            |s| {
                s.adapter
                    .as_mut()
                    .unwrap()
                    .alerts
                    .insert("runtime".into(), 1);
            },
        ),
        // F-045: a signing key's engine (Vault Transit) not ready.
        ("AledgentAdapterSignerNotReady", "settlement-adapter", |s| {
            s.adapter
                .as_mut()
                .unwrap()
                .alerts
                .insert("signer".into(), 1);
        }),
        ("AledgentFinalityStalled", "settlement-adapter", |s| {
            s.adapter.as_mut().unwrap().best = 141
        }),
        // T-005-15: the Agent core's off-node archive.
        ("AledgentCoreArchiveLag", "backup", |s| {
            if let Some(Some(a)) = s.core_archive.as_mut() {
                a.lag_ms = 300_001;
            }
        }),
        ("AledgentCoreArchiveStale", "backup", |s| {
            s.core_archive = Some(None)
        }),
        ("AledgentCoreArchiveConflict", "backup", |s| {
            if let Some(Some(a)) = s.core_archive.as_mut() {
                a.conflict = Some("not this host's history".into());
            }
        }),
    ];
    // The rules over metrics nothing exports yet, each pushed over its line as a raw sample.
    let deferred: &[(&str, &str, &str, i128)] = &[
        (
            "AledgentCreditLag",
            "partition-lag",
            "aledgent_credit_lag_p99_ms",
            2_001,
        ),
        (
            "AledgentRevocationLag",
            "partition-lag",
            "aledgent_revocation_lag_p99_ms",
            1_001,
        ),
        (
            "AledgentStreamLag",
            "partition-lag",
            "aledgent_stream_lag_ms",
            2_001,
        ),
        (
            "AledgentReplayFenced",
            "partition-lag",
            "aledgent_replay_fenced",
            1,
        ),
        // R-03 (rq-01): a message between Agents its sender did not commit; messages no
        // certificate has covered for over a minute.
        (
            "AledgentR03CheckFailed",
            "message-provenance",
            "aledgent_r03_failing_streams",
            1,
        ),
        (
            "AledgentR03Unverified",
            "message-provenance",
            "aledgent_r03_unverified_age_ms",
            60_001,
        ),
    ];
    for (alert, signal, metric, value) in deferred {
        assert!(DEFERRED.contains(metric), "{metric} is deferred");
        let (pager, _) = page_with(&healthy(), &[((*metric).into(), BTreeMap::new(), *value)]);
        let got: Vec<_> = pager
            .delivered
            .iter()
            .filter(|a| a.alert == *alert)
            .collect();
        assert!(
            !got.is_empty(),
            "{alert} did not page: {:#?}",
            pager.delivered
        );
        for a in got {
            assert_eq!(a.labels.get("signal").map(String::as_str), Some(*signal));
            assert!(a.labels.contains_key("owner"), "{alert} has no owner");
        }
        // Healthy at the line: nothing pages.
        let (pager, _) = page_with(&healthy(), &[((*metric).into(), BTreeMap::new(), 0)]);
        assert!(pager.delivered.is_empty(), "{:#?}", pager.delivered);
    }
    for (alert, signal, push) in cases {
        let mut s = healthy();
        push(&mut s);
        let (pager, _) = page(&s);
        let got: Vec<_> = pager
            .delivered
            .iter()
            .filter(|a| a.alert == *alert)
            .collect();
        assert!(
            !got.is_empty(),
            "{alert} did not page: {:#?}",
            pager.delivered
        );
        for a in got {
            assert_eq!(a.labels.get("signal").map(String::as_str), Some(*signal));
            assert!(a.labels.contains_key("owner"), "{alert} has no owner");
            assert_eq!(a.labels.get("severity").map(String::as_str), Some("page"));
        }
    }
    // Every rule that pages is exercised above.
    let exercised: BTreeSet<&str> = cases
        .iter()
        .map(|(a, _, _)| *a)
        .chain(deferred.iter().map(|(a, _, _, _)| *a))
        .collect();
    for r in &Rules::committed().unwrap().0 {
        let pages = r.labels.get("severity").map(String::as_str) == Some("page");
        if pages {
            assert!(
                exercised.contains(r.alert.as_str()),
                "{} is untested",
                r.alert
            );
        }
    }
    // A condition that clears before its `for` does not page.
    let mut s = healthy();
    s.core.as_mut().unwrap().oldest_open_age = Some(3_600_001);
    let mut ev = Evaluator::new(Rules::committed().unwrap());
    let oldest = |a: &Vec<ops::alerts::Alert>| {
        a.iter()
            .any(|a| a.alert == "AledgentOldestNonTerminalOverServiceLevel")
    };
    assert!(!oldest(&ev.step(NOW, &parse(&render(&s)))));
    assert!(!oldest(
        &ev.step(NOW + 600_000, &parse(&render(&healthy())))
    ));
}

/// `AC-C4.1`, `REQ-OP-6`: over a running deployment, a reservation left past its deadline and its
/// service level joins the review queue, and the exporter's scrape pages its owner through the
/// test route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_c4_1_a_reservation_past_its_deadline_pages_the_queue_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let k: OpsKit<ingress::store::mem::MemStore, settle::store::mem::MemStore> = OpsKit::start(
        ingress::store::mem::MemStore::new(Duration::from_millis(50)),
        settle::store::mem::MemStore::new(),
        &tmp.path().join("audit"),
        &tmp.path().join("book"),
    )
    .await;
    let b = k.parties[1].clone();
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/reservations",
            Some(&b.token),
            &[("client-key", "left")],
            Some(serde_json::json!({ "payer": b.account, "asset": "USD", "amount": "5" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    // Past its deadline (ten minutes) and its service level (one hour).
    k.d.clock.advance(600_000 + 3_600_000 + 1_000);
    k.tick_to_now().await;
    assert!(k.d.quiesce(ops::kit::PATIENCE).await);
    let adapter = k.adapter.signals(k.d.clock.now()).await.unwrap();
    let exporter = Exporter::new(
        RouterSource::new(k.d.router.clone(), OPS_TOKEN),
        Fixed(Mutex::new(Some(adapter))),
        Records::default(),
        "settlement-operations",
        ops::kit::PATIENCE,
    );
    let now = k.d.clock.now();
    let samples = parse(&exporter.scrape(now).await);
    let mut ev = Evaluator::new(Rules::committed().unwrap());
    ev.step(now, &samples);
    let firing = ev.step(now + 600_000, &samples);
    let mut pager = TestRoute::default();
    pager.deliver(&Routing::committed().unwrap(), "pager", &firing);
    let review: Vec<_> = pager
        .delivered
        .iter()
        .filter(|a| a.alert == "AledgentReviewOverdue")
        .collect();
    assert_eq!(review.len(), 1, "{:#?}", pager.delivered);
    assert_eq!(
        review[0].labels.get("owner").map(String::as_str),
        Some("settlement-operations")
    );
    assert!(pager.got("AledgentOldestNonTerminalOverServiceLevel"));
    let sl = samples
        .iter()
        .find(|(n, _, _)| n == "aledgent_review_service_level_ms")
        .map(|(_, _, v)| *v);
    assert_eq!(sl, Some(3_600_000));
}

/// The dashboards show every signal, and name no metric nothing exports (but the deferred anchor
/// lag); the Needs-Review queue has its panel, with its owner and service level.
#[test]
fn req_op_6_the_dashboards_show_every_signal_and_the_review_queue() {
    let dash: serde_json::Value = serde_json::from_str(include_str!(
        "../grafana/dashboards/aledgent-operations.json"
    ))
    .unwrap();
    let mut exprs = Vec::new();
    for p in dash["panels"].as_array().unwrap() {
        for t in p["targets"].as_array().unwrap() {
            exprs.push((
                p["title"].as_str().unwrap().to_owned(),
                t["expr"].as_str().unwrap().to_owned(),
            ));
        }
    }
    let metric = |e: &str| e.split(['{', ' ']).next().unwrap_or_default().to_owned();
    let shown: BTreeSet<String> = exprs.iter().map(|(_, e)| metric(e)).collect();
    for (signal, metrics) in SIGNALS.iter().chain(ANCHORING).chain(RELEASE) {
        for m in *metrics {
            assert!(shown.contains(*m), "{signal}: {m} has no panel");
        }
    }
    let exported: BTreeSet<String> = parse(&render(&healthy()))
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    for m in &shown {
        assert!(
            exported.contains(m) || DEFERRED.contains(&m.as_str()),
            "{m} is on a dashboard and not exported"
        );
    }
    for r in &Rules::committed().unwrap().0 {
        let m = &r.expr.metric;
        assert!(
            exported.contains(m) || DEFERRED.contains(&m.as_str()),
            "{}: {m} is not exported",
            r.alert
        );
    }
    let queue = dash["panels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["title"].as_str().unwrap().contains("Needs-Review"))
        .unwrap();
    let text = queue.to_string();
    assert!(text.contains("owner") && text.contains("aledgent_review_service_level_ms"));
}

/// R-03 (owner ruling `rq-01`, 2026-10-05): a failed check of a message between the book's Agents
/// pages security at once, through its own route; so do messages left unverified, after five
/// minutes over a minute. Report mode never refuses a message, so the page is the control.
#[test]
fn req_rt_13_a_failed_r03_check_pages_security_at_once() {
    let rules = Rules::committed().unwrap();
    for alert in ["AledgentR03CheckFailed", "AledgentR03Unverified"] {
        let r = rules
            .get(alert)
            .unwrap_or_else(|| panic!("{alert} is committed"));
        assert_eq!(r.labels.get("severity").map(String::as_str), Some("page"));
        assert_eq!(r.labels.get("owner").map(String::as_str), Some("security"));
    }
    let failed = rules.get("AledgentR03CheckFailed").unwrap();
    assert_eq!(failed.expr.metric, "aledgent_r03_failing_streams");
    assert_eq!(failed.for_ms, 0, "at once");
    let routing = Routing::committed().unwrap();
    let mut labels = BTreeMap::new();
    labels.insert("severity".to_owned(), "page".to_owned());
    labels.insert("signal".to_owned(), "message-provenance".to_owned());
    labels.insert("owner".to_owned(), "security".to_owned());
    assert_eq!(routing.receiver(&labels), "pager");
}

/// `T-018-10`: the release's alerts each link a runbook under `ops/runbooks/release/` that
/// exists; the book's halt and the audit trail's shipping reach the pager at once, through their
/// own routes; a warning of the Authority backlog reaches the dashboard only.
#[test]
fn t_018_10_release_alerts_link_their_runbooks_and_route_to_the_pager() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let signals: BTreeSet<&str> = RELEASE.iter().map(|(s, _)| *s).collect();
    let rules = Rules::committed().unwrap();
    let mut release = 0;
    for r in &rules.0 {
        let Some(signal) = r.labels.get("signal") else {
            continue;
        };
        if !signals.contains(signal.as_str()) {
            continue;
        }
        release += 1;
        let runbook = r
            .annotations
            .get("runbook")
            .unwrap_or_else(|| panic!("{} has no runbook", r.alert));
        assert!(
            runbook.starts_with("ops/runbooks/release/") && root.join(runbook).is_file(),
            "{}: {runbook}",
            r.alert
        );
    }
    assert_eq!(release, 18, "the release alerts");
    let routing = Routing::committed().unwrap();
    let mut labels = BTreeMap::new();
    labels.insert("severity".to_owned(), "page".to_owned());
    for signal in [
        "core-halt",
        "audit-shipping",
        "partition-lag",
        "message-provenance",
    ] {
        labels.insert("signal".to_owned(), signal.to_owned());
        assert_eq!(routing.receiver(&labels), "pager", "{signal}");
    }
    labels.insert("severity".to_owned(), "warning".to_owned());
    labels.insert("signal".to_owned(), "vos-authority".to_owned());
    assert_eq!(routing.receiver(&labels), "dashboard");
    // A warning over its line reaches the dashboard and pages no one.
    let mut s = healthy();
    s.core.as_mut().unwrap().vos_unacknowledged = Some(1);
    let (pager, dashboard) = page(&s);
    assert!(pager.delivered.is_empty(), "{:#?}", pager.delivered);
    assert!(dashboard.got("AledgentVosAuthorityUnacknowledged"));
}

/// `T-005-15`, `NFR-4`: the exporter reads the Agent core's archiver status; the core's backup is
/// the archive, as old as its lag; an unreadable status pages as stale; a checkpoint two hours old
/// warns on the dashboard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_005_15_the_archive_is_the_agent_cores_backup_and_its_lag_is_exported() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("archive.status.json");
    let mut status = match healthy().core_archive {
        Some(Some(a)) => a,
        _ => unreachable!(),
    };
    status.point = Some(core_archive::record::Point {
        inputs: 997,
        entries: 900,
        log_head: "00".into(),
        kernel_root: String::new(),
        service_root: String::new(),
        audit: String::new(),
        at: NOW - 900,
        archive_bytes: 1,
        replies_bytes: 1,
    });
    std::fs::write(&path, serde_json::to_vec(&status).unwrap()).unwrap();
    let exporter = |records: Records| {
        Exporter::new(
            Fixed(Mutex::new(None::<SignalsBody>)),
            Fixed(Mutex::new(None::<AdapterSignals>)),
            records,
            "settlement-operations",
            Duration::from_millis(100),
        )
    };
    let e = exporter(Records {
        core_archive: Some(path.clone()),
        ..Records::default()
    });
    let s = e.collect(NOW).await;
    assert_eq!(s.backups.get(&StoreKind::Core), Some(&(NOW - 500 - 400)));
    let metric = |s: &Snapshot, name: &str| {
        parse(&render(s))
            .into_iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, _, v)| v)
    };
    assert_eq!(metric(&s, "aledgent_archive_lag_ms"), Some(400));
    assert_eq!(metric(&s, "aledgent_archive_lag_inputs"), Some(3));
    assert_eq!(metric(&s, "aledgent_archive_conflict"), Some(0));
    assert_eq!(metric(&s, "aledgent_archive_status_age_ms"), Some(500));
    // Configured and unreadable: as old as the epoch.
    let e = exporter(Records {
        core_archive: Some(tmp.path().join("absent.json")),
        ..Records::default()
    });
    let s = e.collect(NOW).await;
    assert_eq!(s.core_archive, Some(None));
    assert_eq!(
        metric(&s, "aledgent_archive_status_age_ms"),
        Some(i128::from(NOW))
    );
    assert_eq!(metric(&s, "aledgent_archive_lag_ms"), None);
    // An old checkpoint warns, and pages no one.
    let mut old = healthy();
    if let Some(Some(a)) = old.core_archive.as_mut() {
        a.checkpoint = None;
    }
    let (pager, dashboard) = page(&old);
    assert!(pager.delivered.is_empty(), "{:#?}", pager.delivered);
    assert!(dashboard.got("AledgentCoreCheckpointLate"));
}
