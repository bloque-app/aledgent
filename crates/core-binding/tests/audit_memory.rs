// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The audit outbox no longer grows with the book (`T-013-13`, finding G-02 of `F-032`).
//!
//! The stress suite's two-hour soak saw the core host grow about 1.8 KB an input, for ever:
//! ledger-core's state, the audit outbox among it, which nothing shipped. This test replays one
//! recorded workload (onboarding, transfers, reservations and their captures, ticks and delivery
//! records) into two Agent cores over the native Agent, under a counting allocator: one that ships
//! nothing, as before, and one that ships its trail to an audit store on disk as it goes. The
//! shipping core's outbox stays empty and its heap holds less, by at least half the bytes the
//! other's outbox holds; the store holds the whole trail, each record once, in order.
//!
//! The counting allocator counts the whole process: this binary holds this one test only.

use std::time::Duration;

use audit_store::{AuditAppender, AuditStoreConfig, FileAuditStore, WriterCredential};
use core_binding::core::{AgentCore, Founding, MemLog};
use core_binding::host::{NativePort, ReplicaStores};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::audit_ship::Shipper;
use ingress::boundary::{Core as _, CoreInput};
use ingress::inprocess::Journal as _;
use ingress::testkit::{Deployment, PATIENCE, T0, USD, genesis};
use ledger_core::audit::{AuditQuery, AuditRecord, AuditSink as _, AuditTrail};
use peak_alloc::PeakAlloc;
use serde_json::json;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

fn rounds() -> usize {
    std::env::var("ALEDGENT_MEMORY_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
}

const HOLDERS: usize = 8;

/// Record a workload: every input the in-process core applied, in order.
async fn record(rounds: usize) -> Vec<CoreInput> {
    let mut d = Deployment::start().await;
    let mut holders = Vec::new();
    for i in 0..HOLDERS {
        holders.push(d.party(&format!("h{i}"), 1_000_000).await);
    }
    for r in 0..rounds {
        for (i, h) in holders.iter().enumerate() {
            let to = holders[(i + 1) % holders.len()].account.clone();
            let (s, v) = d
                .call(
                    "POST",
                    "/v1/transfers",
                    Some(&h.token),
                    &[("client-key", &format!("t-{r}-{i}"))],
                    Some(json!({ "legs": [{ "from": h.account, "to": to, "asset": USD, "amount": "3" }] })),
                )
                .await;
            assert!(s.as_u16() == 202 || s.as_u16() == 200, "transfer: {s} {v}");
        }
        let mut refs = Vec::new();
        for (i, h) in holders.iter().enumerate() {
            let (s, v) = d
                .call(
                    "POST",
                    "/v1/reservations",
                    Some(&h.token),
                    &[("client-key", &format!("r-{r}-{i}"))],
                    Some(json!({ "payer": h.account, "asset": USD, "amount": "5" })),
                )
                .await;
            assert!(s.as_u16() == 202 || s.as_u16() == 200, "reserve: {s} {v}");
            refs.push(
                v["operation_reference"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
        assert!(d.quiesce(PATIENCE).await, "the round is decided");
        for (i, (h, op)) in holders.iter().zip(&refs).enumerate() {
            let to = holders[(i + 2) % holders.len()].account.clone();
            let (s, v) = d
                .call(
                    "POST",
                    &format!("/v1/reservations/{op}/capture"),
                    Some(&h.token),
                    &[],
                    Some(json!({ "destination": to, "cleared": "5" })),
                )
                .await;
            assert_eq!(s.as_u16(), 200, "capture: {v}");
        }
        if r % 3 == 0 {
            d.party(&format!("new{r}"), 100).await;
        }
        assert!(d.all_delivered(PATIENCE).await, "delivered");
    }
    assert!(d.quiesce(PATIENCE).await, "every row is decided");
    let inputs = d
        .journal
        .entries()
        .unwrap_or_else(|e| panic!("the journal reads: {e}"))
        .into_iter()
        .map(|(_, i)| i)
        .collect();
    d.replica.kill();
    inputs
}

fn founding() -> Founding {
    let g = genesis(T0);
    Founding {
        genesis: g.genesis,
        op: g.op,
        at: g.at,
        keys: core_binding::host::HostKeys::local(g.root, None),
        limits: Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    }
}

/// What a vector of these records holds on the heap: the outbox's share of memory, at least.
fn records_bytes(records: &[AuditRecord]) -> usize {
    let before = PEAK.current_usage();
    let copy: Vec<AuditRecord> = records.to_vec();
    let held = PEAK.current_usage().saturating_sub(before);
    drop(copy);
    held
}

/// `T-013-13`, `REQ-AT-2`, `NFR-6`, finding G-02 of `F-032`: a core that ships its trail holds no
/// outbox, however long the book; one that does not holds every record it ever emitted.
#[tokio::test(flavor = "multi_thread")]
async fn t_013_13_the_audit_outbox_does_not_grow_with_the_book() {
    let inputs = record(rounds()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // As before: nothing ships.
    let base = PEAK.current_usage();
    let plain = AgentCore::open_in(
        NativePort::new(),
        founding(),
        MemLog::default(),
        ReplicaStores::Memory,
    )
    .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    for (at, input) in inputs.iter().enumerate() {
        plain
            .apply(input)
            .await
            .unwrap_or_else(|e| panic!("input {at} applies: {e:?}"));
    }
    let plain_held = PEAK.current_usage().saturating_sub(base);
    // Its outbox: every record it emitted, read through a core that ships (below) for the count.
    drop(plain);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Shipping to a store on disk, every few inputs, as the shipper's thread does.
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("state");
    std::fs::create_dir_all(&book).unwrap();
    let config = AuditStoreConfig {
        dir: dir.path().join("audit"),
        book_dir: book,
        segment_records: 4_096,
    };
    let writer = WriterCredential::new(b"test-audit-writer-credential".to_vec());
    let base = PEAK.current_usage();
    let sink = AuditAppender::open(&config, &writer).unwrap();
    let shipping = AgentCore::open_audited_unstarted(
        NativePort::new(),
        founding(),
        MemLog::default(),
        ReplicaStores::Memory,
        sink.head().unwrap(),
        1_000_000,
    )
    .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let audit = shipping.audit().unwrap();
    let mut shipper = Shipper::new(Box::new(sink), audit.clone());
    let mut most_waiting = 0;
    for (at, input) in inputs.iter().enumerate() {
        shipping
            .apply(input)
            .await
            .unwrap_or_else(|e| panic!("input {at} applies: {e:?}"));
        most_waiting = most_waiting.max(
            audit
                .stats
                .unshipped
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        if at % 8 == 7 {
            assert!(matches!(shipping.ship(&mut shipper), Some(Ok(_))));
        }
    }
    assert!(matches!(shipping.ship(&mut shipper), Some(Ok(_))));
    let shipping_held = PEAK.current_usage().saturating_sub(base);
    let unshipped = audit
        .stats
        .unshipped
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(unshipped, 0, "the outbox is empty");

    // The store holds the whole trail, in order, once.
    let trail: Vec<AuditRecord> = FileAuditStore::read(&config.dir)
        .unwrap()
        .query(&AuditQuery::default())
        .into_iter()
        .cloned()
        .collect();
    assert!(AuditTrail::verify_chain(&trail).is_ok());
    let shipped = audit
        .stats
        .shipped
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(trail.len() as u64, shipped);
    let outbox_bytes = records_bytes(&trail);
    let on_disk: u64 = std::fs::read_dir(&config.dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("seg-"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    eprintln!(
        "T-013-13: {} inputs, {} audit records ({} B on the heap, {} B in the store's segments); \
         the core that ships nothing held {} B, the core that ships held {} B; at most {} records \
         waited at once",
        inputs.len(),
        trail.len(),
        outbox_bytes,
        on_disk,
        plain_held,
        shipping_held,
        most_waiting
    );
    assert!(trail.len() > 200, "a workload with a real trail");
    assert!(
        most_waiting <= 8 * 16,
        "the outbox stays within a few inputs' records"
    );
    assert!(
        plain_held.saturating_sub(shipping_held) >= outbox_bytes / 2,
        "shipping returns the outbox's memory: {plain_held} B against {shipping_held} B, the \
         trail {outbox_bytes} B"
    );
}
