// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The in-process core with its leaves on disk (`T-031-14`, finding F6 of `F-032`), as
//! `aledgent-ingress serve` runs it: the same answers as over memory, and rebuilt from its journal
//! into a fresh file on every start.

use std::time::Duration;

use axum::http::StatusCode;
use ingress::boundary::Core as _;
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, T0, USD, fast_config, genesis, test_clerk};
use serde_json::{Value, json};

/// `REQ-RT-3`, `INV-18`, `T-031-14`: a deployment over an on-disk core takes transfers; the core
/// restarts (the file is deleted and the journal replayed into a new one) and reads back the same
/// book; a retried command is answered with its first decision.
#[tokio::test(flavor = "multi_thread")]
async fn req_rt_3_an_on_disk_core_rebuilds_from_its_journal() {
    let dir = std::env::temp_dir().join(format!("aledgent-book-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("core.journal.book");
    let journal = MemJournal::new();
    let core = InProcessCore::on_disk(genesis(T0), journal.clone(), &path, 1024 * 1024)
        .with_clerk(test_clerk());
    core.start()
        .unwrap_or_else(|e| panic!("the core starts: {e}"));
    let founded = core
        .founded()
        .unwrap_or_else(|| unreachable!("a started core is founded"));
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        founded,
        journal,
    )
    .await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    assert!(path.exists(), "the leaves are in the file");
    let body = json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "7" }] });
    let (s, first) = d
        .call_decided(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[("client-key", "t1")],
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{first}");
    let balances = |p: &str| format!("/v1/balances?account={p}");
    let bob_account = bob.account.clone().unwrap_or_default();
    let (_, before) = d
        .call("GET", &balances(&bob_account), Some(&bob.token), &[], None)
        .await;
    let inputs = core.inputs();

    core.stop();
    // The replica's ticker keeps logging inputs (every 50 ms in `fast_config`), so the count read
    // before the stop is only a lower bound. What was journaled when the core stopped is what the
    // restart must replay; a tick logged after the restart may add to it.
    let journaled = d.journal.len() as u64;
    assert!(
        Some(journaled) >= inputs,
        "{journaled} journaled, {inputs:?} applied"
    );
    core.start()
        .unwrap_or_else(|e| panic!("the core restarts: {e}"));
    let replayed = core.inputs().unwrap_or_default();
    let now_journaled = d.journal.len() as u64;
    assert!(
        (journaled..=now_journaled).contains(&replayed),
        "the journal replayed: {replayed} applied, {journaled} journaled at the stop, {now_journaled} now"
    );
    let (_, after) = d
        .call("GET", &balances(&bob_account), Some(&bob.token), &[], None)
        .await;
    assert_eq!(before, after, "the same book");
    assert!(before.to_string().contains("\"7\""), "{before}");
    let (s, again) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[("client-key", "t1")],
            Some(body),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    let reference = |v: &Value| v["operation_reference"].clone();
    assert_eq!(reference(&again), reference(&first), "one operation");
    assert!(core.last_tick().await.is_ok());
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
