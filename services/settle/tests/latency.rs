// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! When the adapter steps (`NFR-15`, `F-032` T-032-12): a withdrawal's completion ends at its
//! leg's inclusion (amendment 0018) and its hand-off follows the layer's finality; the adapter must
//! not add an interval's wait before inclusion, nor on either side of that finality. [`settle::adapter::Adapter::news`] says when a step is due before the interval ends;
//! the process steps on it (`wake_poll`). Against the fake settlement layer (`settle::testkit`).

use settle::testkit::{Kit, KitOptions};

/// A kit whose adapter has taken its first step and serves, and whose core ticks only when the
/// test passes time ([`Kit::drive_ticks`]). With the replica's own ticker running, the first
/// checkpoint is taken at whichever 50 ms tick follows the first entry: its anchor is a call the
/// outbox did not hold at the last step, and so news, at a moment no test chooses. These tests
/// say what is news; nothing else may add to the outbox behind them.
async fn serving_kit() -> Kit {
    let mut k = Kit::start_with(KitOptions::default()).await;
    // The adapter has not stepped yet: the restart onto the new replica's surface loses nothing.
    k.drive_ticks();
    let r = k.step().await.unwrap();
    assert!(r.serving, "{:?}", k.adapter.status());
    k
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nfr_15_a_withdrawal_new_in_the_outbox_is_news_and_its_step_signs_it() {
    let mut k = serving_kit().await;
    assert!(!k.adapter.news().await, "nothing changed since the step");
    let alice = k.d.party("alice", 10_000).await;
    let op = k.withdrawal(&alice, 250, "w1").await;
    assert!(k.adapter.news().await, "a call waits for intake");
    let r = k.step().await.unwrap();
    assert_eq!((r.acknowledged, r.signed), (1, 1));
    assert_eq!(k.state(&alice, &op).await, "Submitted");
    assert!(
        !k.adapter.news().await,
        "taken in and signed: nothing new until the layer moves"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nfr_15_inv_10_a_moved_head_is_news_and_settlement_still_waits_for_finality() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 10_000).await;
    let op = k.withdrawal(&alice, 250, "w1").await;
    k.step().await.unwrap();
    // Included above the finality point: news, observed provisionally, nothing settled.
    k.chain.produce();
    assert!(k.adapter.news().await, "the best head moved");
    k.step().await.unwrap();
    assert_eq!(k.state(&alice, &op).await, "Observed");
    assert!(!k.adapter.news().await);
    // Final: news, and its step settles (INV-10: only now).
    k.chain.finalize_all();
    assert!(k.adapter.news().await, "the finalized head moved");
    k.step().await.unwrap();
    assert_eq!(k.state(&alice, &op).await, "Settled");
    assert!(!k.adapter.news().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nfr_15_an_adapter_that_does_not_serve_has_no_news() {
    let mut k = Kit::start_with(KitOptions {
        registered: false,
        ..KitOptions::default()
    })
    .await;
    let r = k.step().await.unwrap();
    assert!(!r.serving);
    k.chain.advance(1);
    assert!(
        !k.adapter.news().await,
        "while refusing, steps come at the interval only"
    );
}
