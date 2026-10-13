// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-018` — Operations: its registered cases (`F-015` §6, §7).
//!
//! Each case runs a whole deployment in process — ingress, the in-process core, the adapter over
//! the fake settlement layer — through `ops::kit`, as `F-009`'s and `F-016`'s cases do; the
//! harness's target is not used. `ops`'s own tests hold the rest of `F-018`'s checks: the drill
//! over PostgreSQL restored from a base backup and its WAL archive, every alert rule pushed over its
//! line, the external source's privileged read, the runbooks.
//!
//! `AC-J3.1` and `AC-J3.2` here are the daily reconciliation's, on all three axes, axis 3 through
//! `F-017`'s settlement comparison; `F-017`'s own cases hold the comparison's.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
use curve25519_dalek::ristretto::CompressedRistretto;
use ingress::clock::Clock as _;
use ops::alerts::{Evaluator, Routing, Rules, TestRoute};
use ops::drill::{Expected, Verdict};
use ops::exporter::{Exporter, Fixed, Records, RouterSource};
use ops::kit::{DrillPlan, OPS_TOKEN, OpsKit, run_drill};
use ops::reconcile::{self, BookSnapshot, NotRun, RouterLink, Sources};
use ops::signals::parse;

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::Harness;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F018,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-018`'s registered cases.
pub const CASES: &[Case] = &[
    service_case!(
        "AC-K5.1",
        ac_k5_1_a_restore_to_an_older_backup_and_replay_yield_the_pre_failure_root__service,
        &["NFR-4", "NFR-10", "INV-5", "INV-6", "REQ-RT-3"]
    ),
    service_case!(
        "AC-C4.2",
        ac_c4_2_the_age_of_the_oldest_non_terminal_operation_is_scraped__service,
        &["REQ-OP-8", "NFR-11", "REQ-PV-5"]
    ),
    service_case!(
        "AC-C4.1",
        ac_c4_1_the_review_queue_pages_its_owner_past_its_service_level__service,
        &["REQ-OP-5", "REQ-OP-6", "NFR-11"]
    ),
    service_case!(
        "AC-J3.2",
        ac_j3_2_one_minor_unit_fails_every_reconciliation_axis__service,
        &["NFR-9", "INV-5", "INV-6", "REQ-PR-5"]
    ),
    service_case!(
        "AC-J3.1",
        ac_j3_1_the_daily_run_has_run_and_is_recorded__service,
        &["NFR-9", "REQ-PR-5", "REQ-LA-2"]
    ),
];

type Result<T = ()> = std::result::Result<T, String>;
type MemKit = OpsKit<ingress::store::mem::MemStore, settle::store::mem::MemStore>;

fn check(cond: bool, why: impl FnOnce() -> String) -> Result {
    if cond { Ok(()) } else { Err(why()) }
}

/// Run an async case body on a runtime of its own, with a directory of its own.
fn run<F: Future<Output = Result>>(body: impl FnOnce(std::path::PathBuf) -> F) -> Outcome {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return Outcome::Fail(format!("no runtime: {e}")),
    };
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => return Outcome::Fail(format!("no directory: {e}")),
    };
    match rt.block_on(body(tmp.path().to_path_buf())) {
        Ok(()) => Outcome::Pass,
        Err(why) => Outcome::Fail(why),
    }
}

async fn kit(dir: &std::path::Path, rounds: usize) -> MemKit {
    let mut k = OpsKit::start(
        ingress::store::mem::MemStore::new(Duration::from_millis(50)),
        settle::store::mem::MemStore::new(),
        &dir.join("audit"),
        &dir.join("book"),
    )
    .await;
    for _ in 0..rounds {
        k.round().await;
    }
    k
}

fn ac_k5_1_a_restore_to_an_older_backup_and_replay_yield_the_pre_failure_root__service(
    _: &mut dyn Harness,
) -> Outcome {
    run(|dir| async move {
        let mut k = kit(&dir, 1).await;
        let root = dir.join("backups");
        let (core_backup, _) = k
            .backup_core(&root, "b1")
            .await
            .map_err(|e| e.to_string())?;
        let (audit_backup, _, _) =
            ops::backup::audit::take(&k.audit_dir, &root, "a1", 1).map_err(|e| e.to_string())?;
        k.round().await;
        k.anchor_round(12).await;
        k.quiet().await;
        let r = k.roots().ok_or("the core runs")?;
        k.fail();
        let outside = k.outside();
        let drilled = run_drill(
            &DrillPlan {
                id: "conformance",
                host: "in-process",
                workdir: &dir.join("isolated"),
                core_backup: &core_backup,
                audit_backup: &audit_backup,
                expected: Some(Expected {
                    kernel_root: r.kernel,
                    book_root: r.book,
                    inputs: r.inputs,
                }),
                wal_window_s: None,
            },
            &outside,
            k.d.store.clone(),
            k.adapter_store.clone(),
        )
        .await
        .map_err(|e| e.to_string())?;
        for c in &drilled.report.checks {
            check(c.verdict == Verdict::Passed, || {
                format!("{}: {}", c.name, c.detail)
            })?;
        }
        check(drilled.report.replay.replayed > 0, || {
            "nothing replayed".into()
        })
    })
}

async fn exporter_of(
    k: &MemKit,
) -> Result<Exporter<RouterSource, Fixed<settle::signals::AdapterSignals>>> {
    let adapter = k
        .adapter
        .signals(k.d.clock.now())
        .await
        .map_err(|e| e.to_string())?;
    Ok(Exporter::new(
        RouterSource::new(k.d.router.clone(), OPS_TOKEN),
        Fixed(Mutex::new(Some(adapter))),
        Records::default(),
        "settlement-operations",
        ops::kit::PATIENCE,
    ))
}

fn ac_c4_2_the_age_of_the_oldest_non_terminal_operation_is_scraped__service(
    _: &mut dyn Harness,
) -> Outcome {
    run(|dir| async move {
        let k = kit(&dir, 0).await;
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
        check(s.as_u16() == 200, || format!("reserve: {v}"))?;
        k.d.clock.advance(7_000);
        let e = exporter_of(&k).await?;
        let text = e.scrape(k.d.clock.now()).await;
        let age = parse(&text)
            .into_iter()
            .find(|(n, _, _)| n == "aledgent_oldest_non_terminal_age_ms")
            .map(|(_, _, v)| v);
        check(age.is_some_and(|a| a >= 7_000), || {
            format!("{age:?} in {text}")
        })?;
        check(!text.contains("did:"), || {
            "a ledger fact was scraped".into()
        })
    })
}

fn ac_c4_1_the_review_queue_pages_its_owner_past_its_service_level__service(
    _: &mut dyn Harness,
) -> Outcome {
    run(|dir| async move {
        let k = kit(&dir, 0).await;
        let b = k.parties[1].clone();
        k.d.call_decided(
            "POST",
            "/v1/reservations",
            Some(&b.token),
            &[("client-key", "left")],
            Some(serde_json::json!({ "payer": b.account, "asset": "USD", "amount": "5" })),
        )
        .await;
        k.d.clock.advance(600_000 + 3_600_000 + 1_000);
        k.tick_to_now().await;
        check(k.d.quiesce(ops::kit::PATIENCE).await, || "undecided".into())?;
        let e = exporter_of(&k).await?;
        let now = k.d.clock.now();
        let samples = parse(&e.scrape(now).await);
        let rules = Rules::committed().map_err(|e| e.to_string())?;
        let routing = Routing::committed().map_err(|e| e.to_string())?;
        let mut ev = Evaluator::new(rules);
        ev.step(now, &samples);
        let firing = ev.step(now + 600_000, &samples);
        let mut pager = TestRoute::default();
        pager.deliver(&routing, "pager", &firing);
        let owner = pager
            .delivered
            .iter()
            .find(|a| a.alert == "AledgentReviewOverdue")
            .and_then(|a| a.labels.get("owner").cloned());
        check(owner.as_deref() == Some("settlement-operations"), || {
            format!("{:?}", pager.delivered)
        })
    })
}

fn link(k: &MemKit) -> RouterLink {
    RouterLink {
        router: k.d.router.clone(),
        token: OPS_TOKEN.to_owned(),
    }
}

fn ac_j3_1_the_daily_run_has_run_and_is_recorded__service(_: &mut dyn Harness) -> Outcome {
    run(|dir| async move {
        let mut k = kit(&dir, 1).await;
        k.anchor_round(12).await;
        let records = dir.join("runs");
        let run = reconcile::daily(&k.d.core, &link(&k), None, k.d.clock.now(), &records)
            .await
            .map_err(|e| e.to_string())?;
        check(run.clean(), || format!("{run:?}"))?;
        check(run.audited.is_some(), || "no audit record".into())?;
        let recorded = reconcile::latest_run(&records).map_err(|e| e.to_string())?;
        check(recorded.as_ref() == Some(&run), || "no JSON record".into())
    })
}

fn ac_j3_2_one_minor_unit_fails_every_reconciliation_axis__service(_: &mut dyn Harness) -> Outcome {
    run(|dir| async move {
        let mut k = kit(&dir, 1).await;
        k.anchor_round(12).await;
        check(k.d.quiesce(ops::kit::PATIENCE).await, || "undecided".into())?;
        let run_on = |snap: Option<BookSnapshot>| {
            k.d.core
                .checkpoint(|l, _| {
                    reconcile::run(
                        l,
                        snap,
                        Sources {
                            external: None,
                            settlement: &NotRun::default(),
                        },
                        1,
                    )
                })
                .ok_or("the core runs")?
                .map_err(|e| e.to_string())
        };
        let clean = run_on(None)?;
        check(
            clean.axes.iter().take(2).all(|a| a.findings.is_empty()),
            || format!("{clean:?}"),
        )?;
        let mut snap =
            k.d.core
                .with_ledger(|l| BookSnapshot::read(l.book().store()))
                .ok_or("the core runs")?
                .map_err(|e| e.to_string())?;
        let leaf = snap
            .stored
            .values_mut()
            .flatten()
            .find(|a| a.balances[0].cr.0 != [0; 32])
            .ok_or("an account with credits")?;
        let p = CompressedRistretto(leaf.balances[0].cr.0)
            .decompress()
            .ok_or("a commitment")?;
        leaf.balances[0].cr.0 = (p + RISTRETTO_BASEPOINT_POINT).compress().to_bytes();
        let drifted = run_on(Some(snap))?;
        check(!drifted.axes[0].findings.is_empty(), || {
            "axis 1 missed one minor unit".into()
        })?;
        check(!drifted.axes[1].findings.is_empty(), || {
            "axis 2 missed one minor unit".into()
        })?;
        // Axis 3: one minor unit sent to the position outside the pallet's calls.
        k.chain
            .top_up_position(settle::testkit::INSTANCE, &settle::testkit::chain_usd(), 1);
        k.settle(3).await;
        let run = reconcile::daily(
            &k.d.core,
            &link(&k),
            None,
            k.d.clock.now(),
            &dir.join("runs"),
        )
        .await
        .map_err(|e| e.to_string())?;
        let three = run.axis(3).ok_or("no axis 3")?;
        check(
            three.findings.iter().any(|f| {
                matches!(f, reconcile::Finding::Settlement { difference, .. } if difference == "1")
            }),
            || format!("axis 3 missed one minor unit: {three:?}"),
        )
    })
}
