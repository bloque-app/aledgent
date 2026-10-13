// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The in-process core ships its audit trail to the audit store (`T-013-13`, `REQ-AT-2`, `NFR-6`):
//! from the shipper's own thread, each record once and in order; and a restart replays the journal
//! without holding again what the store already has.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use audit_store::{AuditAppender, AuditStoreConfig, FileAuditStore, WriterCredential};
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, PATIENCE, T0, fast_config, genesis, test_clerk};
use ledger_core::audit::{AuditQuery, AuditTrail};

/// Wait until `f` holds, or `PATIENCE` passes.
async fn until(f: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < PATIENCE {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f()
}

/// `REQ-AT-1`, `REQ-AT-2`, `NFR-6`, `T-013-13`: over a running deployment, the in-process core's
/// shipper empties the audit outbox into the store on its own; the store holds the chain once;
/// after a restart the replayed book holds nothing the store has, and new records continue it.
#[tokio::test(flavor = "multi_thread")]
async fn req_at_2_the_in_process_core_ships_its_trail_and_restarts_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("journal");
    std::fs::create_dir_all(&book).unwrap();
    let config = AuditStoreConfig {
        dir: dir.path().join("audit"),
        book_dir: book,
        segment_records: 8,
    };
    let sink = AuditAppender::open(
        &config,
        &WriterCredential::new(b"test-audit-writer-credential".to_vec()),
    )
    .unwrap();
    let journal = MemJournal::new();
    let core = InProcessCore::in_memory(genesis(T0), journal.clone())
        .with_clerk(test_clerk())
        .with_audit(Box::new(sink), 10_000)
        .unwrap();
    core.start().unwrap();
    let founded = core.founded().unwrap();
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        founded,
        journal,
    )
    .await;
    d.party("alice", 100).await;
    d.party("bob", 0).await;
    assert!(d.quiesce(PATIENCE).await);
    let a = core.audit().unwrap().clone();
    let shipped = || a.stats.shipped.load(Ordering::Relaxed);
    let unshipped = || a.stats.unshipped.load(Ordering::Relaxed);
    assert!(
        until(|| unshipped() == 0 && shipped() > 0).await,
        "the shipper empties the outbox: {} waiting",
        unshipped()
    );
    let held = |c: &AuditStoreConfig| {
        FileAuditStore::read(&c.dir)
            .unwrap()
            .query(&AuditQuery::default())
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    };
    let first = held(&config);
    assert_eq!(first.len() as u64, shipped());
    assert!(AuditTrail::verify_chain(&first).is_ok());

    // A restart replays the journal: nothing the store holds comes back into the outbox.
    core.stop();
    core.start().unwrap();
    assert_eq!(unshipped(), 0, "the replay drops what the store holds");
    d.party("carol", 5).await;
    assert!(d.quiesce(PATIENCE).await);
    assert!(until(|| unshipped() == 0 && shipped() > first.len() as u64).await);
    let second = held(&config);
    assert_eq!(
        second[..first.len()],
        first[..],
        "continued, never rewritten"
    );
    assert!(AuditTrail::verify_chain(&second).is_ok());
    assert!(a.stats.store_up.load(Ordering::Relaxed));
}
