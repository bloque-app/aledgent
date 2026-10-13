// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The ticker logs a tick only when the core has something due (`F-031`, stress finding F2;
//! `DEC-6`, `REQ-RT-5`).

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::http::StatusCode;
use ingress::boundary::Core;
use ingress::clock::Clock;
use ingress::testkit::{Deployment, USD, fast_config};
use serde_json::json;

/// `REQ-RT-5`: with nothing due, the ticker skips its ticks (each would be a full core input);
/// a reservation's deadline makes the core report it due, and the tick that comes once the clock
/// passes it fires the deadline.
#[tokio::test]
async fn req_rt_5_idle_ticks_are_skipped_and_a_due_deadline_still_fires() {
    let mut config = fast_config("ingress-0");
    config.tick_interval = Duration::from_millis(10);
    config.tick_heartbeat = Duration::from_secs(3_600);
    let d = Deployment::start_with(config).await;
    let alice = d.party("alice", 1_000).await;
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let metrics = &d.replica.shared.metrics;
    // Idle: whatever the setup left due is fired by the first ticks; then nothing is due.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ticks = metrics.ticks.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        metrics.ticks.load(Ordering::Relaxed),
        ticks,
        "an idle book is not ticked"
    );
    assert!(metrics.ticks_skipped.load(Ordering::Relaxed) > 0);
    // A reservation with a deadline one second away.
    let (s, v) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[("client-key", "r1")],
            Some(json!({ "payer": alice.account, "asset": USD, "amount": "7", "deadline": d.clock.now() + 1_000 })),
        )
        .await;
    assert!(s == StatusCode::ACCEPTED || s == StatusCode::OK, "{v}");
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let due = d.core.next_due().await.unwrap();
    assert!(due.is_some_and(|t| t > d.clock.now()), "{due:?}");
    // Past it, a tick is logged and the deadline fires: nothing is due any more.
    d.clock.advance(1_500);
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < until {
        if d.quiesce(Duration::ZERO).await
            && d.core
                .next_due()
                .await
                .unwrap()
                .is_none_or(|t| t > d.clock.now())
            && metrics.ticks.load(Ordering::Relaxed) > ticks
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the due deadline was not ticked");
}
