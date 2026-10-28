// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter reports a new sequence of the settlement layer's record to the core (amendment
//! 0041, `T-043-15`, `[PENDING AMENDMENT 0041]`, `REQ-SA-10` as proposed): when it finds the reset
//! live with nothing of the chain before it left unreported, and once per sequence it moved to on
//! its own (a first start). It reports nothing of the new chain to a core that refused to follow
//! it; a refusal or a core that cannot take the input yet is reported again every ten minutes.

use ledger_core::settlement::{FinalisedHead, SETTLEMENT_EPOCHS};
use settle::adapter::{EPOCH_ALERT, EPOCH_RETRY, EPOCH_UNSUPPORTED_ALERT};
use settle::chain::Chain;
use settle::store::Store;
use settle::testkit::{INSTANCE, Kit, KitOptions, chain_usd};

/// A kit whose book is named, its core at `generation`, after its first step.
async fn kit(generation: u64) -> Kit {
    let mut k = Kit::start_with(KitOptions {
        named_book: true,
        core_generation: generation,
        ..KitOptions::default()
    })
    .await;
    assert_ne!(k.book(), [0; 32], "the book is named");
    assert!(k.step().await.unwrap().serving, "{:?}", k.adapter.status());
    k
}

async fn open(k: &Kit, prefix: &str) -> usize {
    k.store
        .alerts()
        .await
        .unwrap()
        .into_iter()
        .filter(|a| a.key.starts_with(prefix) && a.cleared_at.is_none())
        .count()
}

async fn sent(k: &Kit, kind: &str) -> usize {
    k.store
        .reports()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == kind)
        .count()
}

/// The core's answer the adapter keeps for the instance's latest sequence.
async fn kept(k: &Kit) -> Option<String> {
    k.store
        .report(&format!("settle/epoch/{INSTANCE}"))
        .await
        .unwrap()
        .and_then(|r| r.verdict)
}

fn deposits(k: &Kit, n: usize, amount: u128) {
    let from = [0x77; 32];
    k.chain.mint(from, &chain_usd(), 1_000);
    for _ in 0..n {
        k.chain
            .deposit(from, INSTANCE, [0x42; 32], &chain_usd(), amount);
        k.chain.produce();
    }
}

/// The amounts of the deposits the core took in.
fn core_amounts(k: &Kit) -> Vec<u64> {
    k.core_settlement(|s| {
        s.all_ingested()
            .filter_map(|i| i.observation.movement.as_ref().map(|m| m.amount.get()))
            .collect()
    })
    .unwrap()
}

/// The block of the instance's last reset.
async fn reset_block(k: &Kit) -> u64 {
    let fin = k.chain.finalized().await.unwrap();
    k.chain
        .chain_epoch(&fin, INSTANCE)
        .await
        .unwrap()
        .unwrap()
        .at
}

/// `REQ-SA-10` (amendment 0041): a reset of the running adapter's instance, for its own book,
/// with nothing of the chain before it unreported, is reported; the core takes it (its head
/// restarts at the reset's block and position 0); the adapter moves to the new chain with no
/// alert, and its entries and heads reach the core from position 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t043_15_a_live_reset_is_followed() {
    let mut k = kit(SETTLEMENT_EPOCHS).await;
    k.chain.advance(1);
    k.step().await.unwrap();
    k.chain.reset_for(INSTANCE, 0, k.book());
    k.chain.advance(1);
    k.step().await.unwrap();
    let at = reset_block(&k).await;
    assert_eq!(kept(&k).await.as_deref(), Some("started"));
    assert_eq!(
        k.core_settlement(|s| s.epoch().map(|e| (e.epoch, e.at)))
            .unwrap(),
        Some((1, at))
    );
    assert!(
        k.core_head()
            .is_some_and(|h| h.height >= at && h.next_seq == 0),
        "{:?}",
        k.core_head()
    );
    assert_eq!(open(&k, "reset:").await, 0);
    assert_eq!(open(&k, EPOCH_ALERT).await, 0);
    deposits(&k, 2, 5);
    k.chain.finalize_all();
    assert_eq!(k.step().await.unwrap().observed, 2);
    assert_eq!(core_amounts(&k), vec![5, 5]);
    let fin = k.chain.finalized().await.unwrap();
    assert_eq!(
        k.core_head(),
        Some(FinalisedHead {
            height: fin.number,
            next_seq: 2
        })
    );
    // Reported once: the confirmation is kept.
    let epochs = sent(&k, "epoch").await;
    for _ in 0..3 {
        k.chain.advance(1);
        k.step().await.unwrap();
    }
    assert_eq!(sent(&k, "epoch").await, epochs);
}

/// `REQ-SA-10`: a core that refuses the start (it holds final records of the chain before at the
/// positions the new one reuses) gets nothing of the new chain: `epoch:` (pages) and `reset:`.
/// The start is reported again every ten minutes, not every step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t043_15_a_refused_start_stops_the_adapter() {
    let mut k = kit(SETTLEMENT_EPOCHS).await;
    deposits(&k, 2, 1);
    k.chain.finalize_all();
    assert_eq!(k.step().await.unwrap().observed, 2);
    k.chain.reset_for(INSTANCE, 0, k.book());
    k.chain.produce();
    deposits(&k, 1, 7);
    k.chain.finalize_all();
    assert_eq!(k.step().await.unwrap().observed, 0);
    assert_eq!(kept(&k).await.as_deref(), Some("refused:earlier_final"));
    assert_eq!(open(&k, &format!("{EPOCH_ALERT}:{INSTANCE}:1")).await, 1);
    assert_eq!(open(&k, &format!("reset:{INSTANCE}")).await, 1);
    let attempts = k
        .store
        .report(&format!("settle/epoch/{INSTANCE}"))
        .await
        .unwrap()
        .unwrap()
        .sent_at;
    for _ in 0..3 {
        k.chain.advance(1);
        assert_eq!(k.step().await.unwrap().observed, 0);
    }
    assert_eq!(core_amounts(&k), vec![1, 1], "nothing of the new chain");
    let r = k.store.report(&format!("settle/epoch/{INSTANCE}")).await;
    assert_eq!(r.unwrap().unwrap().sent_at, attempts, "not asked again yet");
    k.d.clock.advance(EPOCH_RETRY);
    k.chain.advance(1);
    k.step().await.unwrap();
    let r = k.store.report(&format!("settle/epoch/{INSTANCE}")).await;
    assert!(r.unwrap().unwrap().sent_at > attempts, "asked again");
    assert_eq!(open(&k, &format!("reset:{INSTANCE}")).await, 1);
}

/// A core before amendment 0041's generation cannot take the start: on a live reset the adapter
/// stops as before (`reset:`), with a non-paging `epoch-unsupported:`; once the core is upgraded,
/// the start is reported again when due, taken, the stop lifted, and the new chain reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t043_15_a_core_upgraded_later_follows_the_reset() {
    let mut k = kit(0).await;
    k.chain.advance(1);
    k.step().await.unwrap();
    k.chain.reset_for(INSTANCE, 0, k.book());
    k.chain.produce();
    deposits(&k, 1, 3);
    k.chain.finalize_all();
    assert_eq!(k.step().await.unwrap().observed, 0);
    assert_eq!(
        kept(&k).await.as_deref(),
        Some("refused:ERR-MalformedCommand")
    );
    assert_eq!(open(&k, EPOCH_UNSUPPORTED_ALERT).await, 1);
    assert_eq!(open(&k, &format!("{EPOCH_ALERT}:")).await, 0);
    assert_eq!(open(&k, &format!("reset:{INSTANCE}")).await, 1);
    // The core's in-place upgrade, then the next attempt.
    k.d.core
        .checkpoint(|l, _| l.set_generation(SETTLEMENT_EPOCHS, false))
        .unwrap();
    k.d.clock.advance(EPOCH_RETRY);
    k.chain.advance(1);
    k.step().await.unwrap();
    assert_eq!(kept(&k).await.as_deref(), Some("started"));
    assert_eq!(open(&k, "reset:").await, 0);
    assert_eq!(open(&k, EPOCH_UNSUPPORTED_ALERT).await, 0);
    k.chain.advance(1);
    assert_eq!(k.step().await.unwrap().observed, 1);
    assert_eq!(core_amounts(&k), vec![3]);
}

/// A reset that names another book: the core refuses it, and the adapter stops until `refound`
/// (the book was re-founded), with no `epoch:` alert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t043_15_another_book_s_reset_stops_until_refound() {
    let mut k = kit(SETTLEMENT_EPOCHS).await;
    k.chain.advance(1);
    k.step().await.unwrap();
    k.chain.reset_for(INSTANCE, 0, [0xEE; 32]);
    k.chain.advance(1);
    k.step().await.unwrap();
    assert_eq!(kept(&k).await.as_deref(), Some("refused:other_book"));
    assert_eq!(open(&k, &format!("reset:{INSTANCE}")).await, 1);
    assert_eq!(open(&k, &format!("{EPOCH_ALERT}:")).await, 0);
    assert_eq!(k.core_settlement(|s| s.epoch()).unwrap(), None);
    // While it is open, nothing is retried.
    let r = k.store.report(&format!("settle/epoch/{INSTANCE}")).await;
    let at = r.unwrap().unwrap().sent_at;
    k.d.clock.advance(EPOCH_RETRY);
    k.chain.advance(1);
    k.step().await.unwrap();
    let r = k.store.report(&format!("settle/epoch/{INSTANCE}")).await;
    assert_eq!(r.unwrap().unwrap().sent_at, at);
}

/// Amendment 0041: entries of the chain before the reset past the verified head, never reported,
/// keep the reset from being followed (`reset:`, as before); the start is not reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t043_15_a_reset_with_unreported_entries_is_not_followed() {
    let mut k = kit(SETTLEMENT_EPOCHS).await;
    k.chain.advance(1);
    k.step().await.unwrap();
    deposits(&k, 1, 4);
    k.chain.reset_for(INSTANCE, 0, k.book());
    k.chain.produce();
    k.chain.finalize_all();
    assert_eq!(k.step().await.unwrap().observed, 0);
    assert_eq!(open(&k, &format!("reset:{INSTANCE}")).await, 1);
    assert_eq!(sent(&k, "epoch").await, 0);
    assert_eq!(k.core_settlement(|s| s.epoch()).unwrap(), None);
}

/// Test's case on 28 Oct (amendment 0041): the instance was reset before the adapter's first start,
/// which moved itself to the new chain (a cursor of 13 back to 0) while the core, before the
/// amendment's generation, could not take the start (`epoch-unsupported:`, not paging) and the
/// adapter went on. Once the core is upgraded the start is reported when due and taken; the core's
/// head restarts on the new chain and takes the adapter's next head; it is not reported again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t043_15_a_first_start_reports_the_sequence_once() {
    let mut k = Kit::start_with(KitOptions {
        named_book: true,
        anchoring: true,
        ..KitOptions::default()
    })
    .await;
    k.chain.reset_for(INSTANCE, 0, k.book());
    k.chain.advance(1);
    k.store.advance_cursor(INSTANCE, 13).await.unwrap();
    assert!(k.step().await.unwrap().serving);
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 0);
    assert_eq!(
        kept(&k).await.as_deref(),
        Some("refused:ERR-MalformedCommand")
    );
    assert_eq!(open(&k, EPOCH_UNSUPPORTED_ALERT).await, 1);
    assert_eq!(open(&k, "reset:").await, 0);
    // It goes on: the new chain's entries reach the core.
    deposits(&k, 1, 9);
    k.chain.finalize_all();
    assert_eq!(k.step().await.unwrap().observed, 1);
    // The core's in-place upgrade.
    k.d.core
        .checkpoint(|l, _| l.set_generation(SETTLEMENT_EPOCHS, false))
        .unwrap();
    k.d.clock.advance(EPOCH_RETRY);
    k.chain.advance(1);
    k.step().await.unwrap();
    assert_eq!(kept(&k).await.as_deref(), Some("started"));
    assert_eq!(open(&k, EPOCH_UNSUPPORTED_ALERT).await, 0);
    let fin = k.chain.finalized().await.unwrap();
    assert_eq!(
        k.core_head(),
        Some(FinalisedHead {
            height: fin.number,
            next_seq: 1
        })
    );
    let epochs = sent(&k, "epoch").await;
    let reads = k.chain.reads();
    k.d.clock.advance(EPOCH_RETRY);
    k.chain.advance(1);
    k.step().await.unwrap();
    assert_eq!(sent(&k, "epoch").await, epochs);
    assert_eq!(
        k.chain.reads().epochs,
        reads.epochs,
        "no epoch read once confirmed"
    );
}
