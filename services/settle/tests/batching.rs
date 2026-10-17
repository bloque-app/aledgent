// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's fixes from the first zombienet runs (`F-027`, issue #96; F-026 NOTES, "Against
//! Kreivo main (0.17), and through aledgent", findings 2 to 6), against the fake settlement layer:
//!
//! - `T-027-08`: every transaction's mortal era is born at the finalized head, whose hash the
//!   adapter holds, so a reorganisation above finality cannot invalidate it and nothing is looked up
//!   by block number.
//! - `T-027-09`: batches are sized by proof size from a dry run, their gas limit is the dry run's
//!   with half again, capped at Kreivo's normal-extrinsic maximum; a batch whose dry run fails is
//!   split, and a call that is no good on its own is left unsubmitted for review, never refused on
//!   chain and never released (the owner's ruling on #96).
//!
//! The fake's weights follow the suite's measures (`settle::chain::fake::EXECUTE_BASE` and after):
//! 32 applied withdrawals need about 2.02 MB of proof size.

use ingress::testkit::Party;
use settle::chain::fake::{
    CALL_REFUSED, ENTRY_DEPOSIT, EXECUTE_BASE, OUT_OF_GAS, WITHDRAWAL_APPLIED,
};
use settle::chain::kreivo::{
    CallLimits, FORMAT_133, MAX_CALLS, NORMAL_EXTRINSIC_MAX_0_16_9, Signing, Weight,
};
use settle::chain::{BatchCall, BlockRef, Chain, ChainError, Mortality};
use settle::store::{ExtrinsicStatus, InstructionState, Store};
use settle::testkit::{INSTANCE, Kit, KitOptions, PAYEE, chain_usd};

/// Run rounds (a block, finality, a step) until `op` shows `want`, at most `n`.
async fn until(k: &mut Kit, p: &Party, op: &str, want: &str, n: usize) -> String {
    for _ in 0..n {
        let last = k.state(p, op).await;
        if last == want {
            return last;
        }
        k.chain.advance(1);
        k.step().await.unwrap_or_else(|e| panic!("a step: {e}"));
    }
    k.state(p, op).await
}

/// What the fake layer's default runtime (Kreivo spec 133) is signed for (`T-027-12`).
fn spec_133() -> Signing {
    Signing {
        spec_version: 133,
        transaction_version: FORMAT_133.transaction_version,
        format: FORMAT_133,
    }
}

async fn serving_kit(options: KitOptions) -> Kit {
    let mut k = Kit::start_with(options).await;
    let r = k.step().await.unwrap();
    assert!(r.serving, "{:?}", k.adapter.status());
    k
}

async fn instruction(k: &Kit, op: &str) -> settle::store::InstructionRow {
    k.store
        .instructions()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.operation == op)
        .unwrap_or_else(|| panic!("no instruction {op}"))
}

async fn open_alert(k: &Kit, key: &str) -> bool {
    k.store
        .alerts()
        .await
        .unwrap()
        .into_iter()
        .any(|a| a.key == key && a.cleared_at.is_none())
}

fn plus(a: Weight, b: Weight, n: u64) -> Weight {
    Weight {
        ref_time: a.ref_time + b.ref_time * n,
        proof_size: a.proof_size + b.proof_size * n,
    }
}

/// `n` withdrawals of 10 from a fresh party: their operation references.
async fn withdrawals(k: &Kit, n: usize) -> (Party, Vec<String>) {
    let alice = k.d.party("alice", 100_000).await;
    let mut ops = Vec::new();
    for i in 0..n {
        ops.push(k.withdrawal(&alice, 10, &format!("w{i}")).await);
    }
    (alice, ops)
}

/// The one transaction signed so far: its hash.
async fn only_transaction(k: &Kit) -> [u8; 32] {
    let exts = k.store.extrinsics().await.unwrap();
    let hash = exts[0].extrinsic.hash;
    assert!(exts.iter().all(|e| e.extrinsic.hash == hash), "one batch");
    hash
}

// ------------------------------------------------------------------------------ T-027-08

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_08_req_sl_4_a_batch_born_at_the_finalized_head_survives_a_reorganisation() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    // Finality lags the best head by two blocks, as it does on Kreivo.
    k.chain.produce_n(2);
    let fin = k.chain.finalized_number();
    let op = k.withdrawal(&alice, 10, "w").await;
    assert_eq!(k.step().await.unwrap().signed, 1);
    let ext = k.store.extrinsics().await.unwrap().remove(0);
    assert_eq!(
        (ext.era.birth, ext.era.checkpoint),
        (fin, fin),
        "born at the finalized head"
    );
    assert!(ext.era.last_valid() > k.chain.best_number());
    // Included, then every block above the finality point is retracted (Kreivo 0.17 reorganises
    // up to six blocks): the transaction goes back to the pool, and the block its era is born at
    // is still the chain's.
    k.chain.produce();
    assert!(k.chain.included(&ext.extrinsic.hash).is_some());
    assert_eq!(k.chain.reorg(3, false), 3);
    assert!(k.chain.included(&ext.extrinsic.hash).is_none());
    k.chain.produce();
    assert!(
        k.chain
            .included(&ext.extrinsic.hash)
            .is_some_and(|i| i.success),
        "the same bytes are included again"
    );
    assert_eq!(until(&mut k, &alice, &op, "Settled", 5).await, "Settled");
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&op)), 1);
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 10);

    // What the adapter did before: born at the best block, the same reorganisation leaves the
    // transaction invalid for good (findings 3 and 6), with no fault of its own.
    k.chain.produce_n(2);
    let best = k.chain.best().await.unwrap();
    let era = Mortality::at(32, best.number);
    let lane = &k.lanes[0];
    let nonce = k.chain.account_nonce(&best, &lane.account()).await.unwrap();
    let call = BatchCall::withdraw(&settle::chain::WithdrawCall {
        instance: INSTANCE,
        operation_ref: [0x55; 32],
        to: ledger_core::settlement::Destination::Account(PAYEE),
        asset: chain_usd(),
        amount: 1,
    });
    let at_best = k
        .chain
        .sign_batch(
            INSTANCE,
            &[call],
            lane.as_ref(),
            nonce,
            &era,
            &best,
            &CallLimits::default(),
            &spec_133(),
        )
        .await
        .unwrap();
    k.chain.submit(&at_best).await.unwrap();
    k.chain.reorg(2, false);
    k.chain.produce_n(3);
    assert!(k.chain.included(&at_best.hash).is_none());
    assert_eq!(k.chain.pool_len(), 0, "dropped: its birth block is gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_08_an_era_is_signed_only_at_the_block_it_is_born_at() {
    let k = serving_kit(KitOptions::default()).await;
    k.chain.advance(3);
    let fin: BlockRef = k.chain.finalized().await.unwrap();
    let lane = &k.lanes[0];
    let call = BatchCall::withdraw(&settle::chain::WithdrawCall {
        instance: INSTANCE,
        operation_ref: [0x56; 32],
        to: ledger_core::settlement::Destination::Account(PAYEE),
        asset: chain_usd(),
        amount: 1,
    });
    // An era born below the context would need its birth block's hash by number: refused.
    let r = k
        .chain
        .sign_batch(
            INSTANCE,
            &[call],
            lane.as_ref(),
            0,
            &Mortality::at(16, fin.number - 1),
            &fin,
            &CallLimits::default(),
            &spec_133(),
        )
        .await;
    assert!(matches!(r, Err(ChainError::Invalid(_))), "{r:?}");
}

// ------------------------------------------------------------------------------ T-027-09

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_09_req_so_3_a_full_batch_goes_out_whole_with_its_gas_from_one_dry_run() {
    let mut k = serving_kit(KitOptions::default()).await;
    let (alice, ops) = withdrawals(&k, MAX_CALLS).await;
    let before = k.chain.dry_runs();
    assert_eq!(k.step().await.unwrap().signed, MAX_CALLS);
    assert_eq!(k.chain.dry_runs() - before, 1, "one dry run");
    let hash = only_transaction(&k).await;
    assert_eq!(k.chain.calls_in(&hash), Some(MAX_CALLS));
    k.chain.advance(1);
    let (limit, cost) = k.chain.gas_of(&hash).expect("included");
    assert_eq!(cost, plus(EXECUTE_BASE, WITHDRAWAL_APPLIED, 32));
    // More than the 1 MB the adapter used to give every batch (finding 2)...
    assert!(cost.proof_size > 1_000_000);
    // ...and half again of it, under the most a normal extrinsic may use.
    assert_eq!(limit, cost.with_margin(150));
    assert!(limit.fits(NORMAL_EXTRINSIC_MAX_0_16_9));
    // Its storage deposit limit is the dry run's deposit, one entry per call, with the margin.
    let (deposit_limit, deposit) = k.chain.deposit_of(&hash).expect("included");
    assert_eq!(deposit, 32 * ENTRY_DEPOSIT);
    assert_eq!(deposit_limit, Some(32 * ENTRY_DEPOSIT * 3 / 2));
    assert!(k.chain.included(&hash).is_some_and(|i| i.success));
    for op in &ops {
        assert_eq!(until(&mut k, &alice, op, "Settled", 3).await, "Settled");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_09_req_so_3_a_batch_holds_only_what_its_proof_size_allows() {
    // The adapter's old gas limit, 1 MB of proof size, as the most a batch is given: at about
    // 60 KB a withdrawal and half again, nine fit.
    let mut k = serving_kit(KitOptions {
        batch_gas_cap: Some(Weight {
            ref_time: 50_000_000_000,
            proof_size: 1_000_000,
        }),
        ..KitOptions::default()
    })
    .await;
    let (alice, ops) = withdrawals(&k, MAX_CALLS).await;
    assert_eq!(k.step().await.unwrap().signed, 9);
    let hash = only_transaction(&k).await;
    assert_eq!(k.chain.calls_in(&hash), Some(9));
    // The rest wait for the lane, and go in batches of the same size; none runs out of gas.
    for op in &ops {
        assert_eq!(until(&mut k, &alice, op, "Settled", 30).await, "Settled");
    }
    let exts = k.store.extrinsics().await.unwrap();
    let mut batches: Vec<[u8; 32]> = exts.iter().map(|e| e.extrinsic.hash).collect();
    batches.dedup();
    assert_eq!(batches.len(), 4, "9 + 9 + 9 + 5");
    for e in &exts {
        assert!(
            matches!(&e.status, ExtrinsicStatus::Included(i) if i.success),
            "{:?}",
            e.status
        );
    }
    assert_eq!(
        k.chain.balance_of(&PAYEE, &chain_usd()),
        10 * MAX_CALLS as u128
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_09_req_so_3_a_batch_is_sized_by_the_runtimes_extrinsic_limit_at_its_block() {
    // Kreivo 0.17.0's normal-extrinsic maximum moves with the cores the parachain has: the adapter
    // reads it at the block it sizes a batch against, never a constant. At 1 MB of proof size,
    // nine withdrawals fit (about 60 KB each, and half again).
    let mut k = serving_kit(KitOptions::default()).await;
    let few_cores = Weight {
        ref_time: 1_399_891_843_000,
        proof_size: 1_000_000,
    };
    k.chain.set_extrinsic_limit(few_cores);
    let (alice, ops) = withdrawals(&k, MAX_CALLS).await;
    assert_eq!(k.step().await.unwrap().signed, 9);
    let first = only_transaction(&k).await;
    // More cores by the next batch (the suite read 7 340 032 B in one run): the other 23 go in one.
    k.chain.set_extrinsic_limit(Weight {
        ref_time: 1_399_891_843_000,
        proof_size: 7_340_032,
    });
    for op in &ops {
        assert_eq!(until(&mut k, &alice, op, "Settled", 30).await, "Settled");
    }
    let mut batches: Vec<[u8; 32]> = k
        .store
        .extrinsics()
        .await
        .unwrap()
        .iter()
        .map(|e| e.extrinsic.hash)
        .collect();
    batches.sort();
    batches.dedup();
    assert_eq!(batches.len(), 2, "9, then 23");
    let (limit, cost) = k.chain.gas_of(&first).unwrap();
    assert!(limit.fits(few_cores), "{limit:?} over {few_cores:?}");
    assert!(cost.fits(limit));
    let second = batches.into_iter().find(|h| *h != first).unwrap();
    assert_eq!(k.chain.calls_in(&second), Some(MAX_CALLS - 9));
    assert!(
        !k.chain.gas_of(&second).unwrap().0.fits(few_cores),
        "sized by the new limit, past what the first allowed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_09_req_sa_4_a_call_no_good_alone_is_left_unsubmitted_for_review_and_the_others_go() {
    let mut k = serving_kit(KitOptions::default()).await;
    let (alice, ops) = withdrawals(&k, MAX_CALLS).await;
    // One call traps the contract: every batch carrying it fails whole. The adapter takes its
    // calls in (received, reference) order, so make it the sixth in that order.
    let mut ordered = ops.clone();
    ordered.sort();
    let bad = ordered[5].clone();
    k.chain.poison(Kit::chain_ref(&bad));
    let before = k.chain.dry_runs();
    // Split: the five before it go now; the rest go back.
    assert_eq!(k.step().await.unwrap().signed, 5);
    assert!(k.chain.dry_runs() - before <= 8, "split by bisection");
    let hash = only_transaction(&k).await;
    assert_eq!(k.chain.calls_in(&hash), Some(5));
    for op in &ordered[..5] {
        assert_eq!(until(&mut k, &alice, op, "Settled", 3).await, "Settled");
    }
    // Fitted again with the lane free: alone, the bad call's dry run fails. It is not refused on
    // chain (nothing moved): it is left unsubmitted, and the other 26 go in one batch.
    for op in ordered.iter().filter(|o| **o != bad) {
        assert_eq!(until(&mut k, &alice, op, "Settled", 5).await, "Settled");
    }
    let mut batches: Vec<[u8; 32]> = k
        .store
        .extrinsics()
        .await
        .unwrap()
        .iter()
        .map(|e| e.extrinsic.hash)
        .collect();
    batches.sort();
    batches.dedup();
    assert_eq!(batches.len(), 2, "5, then 26");
    let row = instruction(&k, &bad).await;
    assert!(
        matches!(&row.state, InstructionState::Held { error, .. } if error.contains("alone")),
        "{:?}",
        row.state
    );
    assert!(open_alert(&k, &format!("review:{bad}")).await);
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&bad)), 0);
    // No signing is recorded for it, so nothing proves it absent: past its expiry it is not
    // released, and at its deadline it goes to review, still reserved.
    k.rounds(60).await.unwrap();
    assert_eq!(k.state(&alice, &bad).await, "Expired");
    k.pass(600_000).await;
    let id = ledger_core::id::OperationId::parse(&bad).unwrap();
    let now = ingress::clock::Clock::now(&k.d.clock);
    let queued =
        k.d.core
            .with_ledger(|l| l.review_queue(now).iter().any(|e| e.op == id));
    assert_eq!(queued, Some(true), "in the review queue");
    // Amendment 0006: an operation in the review queue reads `Needs-Review`.
    assert_eq!(k.state(&alice, &bad).await, "Needs-Review");
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&bad)), 0);
    assert_eq!(k.balances(&alice).await, (100_000 - 320, 10));
}

/// A withdrawal whose dry run takes the cheaper path. The contract bounds withdrawals to 10 a
/// window of 8 blocks, and a first withdrawal spends the window. At the finalized head, where the
/// batch is dry-run, the window is still spent, so the second withdrawal is refused there
/// (`BoundExceeded`): the cheap path. By the block it lands in, the window has started again, and
/// it applies: the costlier path. (The suite met the same with a deposit landing first in the
/// block.) Returns the transaction's inclusion and `(gas limit, cost)`.
const CHEAP_PATH_CALLS: usize = 16;

async fn priced_on_the_cheap_path(margin: u64) -> (settle::chain::Included, Weight, Weight) {
    let mut k = serving_kit(KitOptions {
        batch_gas_margin_percent: margin,
        ..KitOptions::default()
    })
    .await;
    let alice = k.d.party("alice", 10_000).await;
    k.chain.set_withdrawal_bound(
        INSTANCE,
        &chain_usd(),
        Some((10 * CHEAP_PATH_CALLS as u128, 8)),
    );
    let started = k.chain.best_number();
    let first = k
        .withdrawal(&alice, 10 * CHEAP_PATH_CALLS as u64, "first")
        .await;
    assert_eq!(until(&mut k, &alice, &first, "Settled", 4).await, "Settled");
    assert!(
        k.chain.finalized_number() < started + 8,
        "inside the window"
    );
    // Blocks past the window's end, above the finality point.
    while k.chain.best_number() < started + 8 {
        k.chain.produce();
    }
    for i in 0..CHEAP_PATH_CALLS {
        k.withdrawal(&alice, 10, &format!("second {i}")).await;
    }
    let before: Vec<[u8; 32]> = k
        .store
        .extrinsics()
        .await
        .unwrap()
        .iter()
        .map(|e| e.extrinsic.hash)
        .collect();
    assert_eq!(k.step().await.unwrap().signed, CHEAP_PATH_CALLS);
    let hash = k
        .store
        .extrinsics()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.extrinsic.hash)
        .find(|h| !before.contains(h))
        .expect("the second batch");
    k.chain.produce();
    let included = k.chain.included(&hash).expect("included");
    let (limit, cost) = k.chain.gas_of(&hash).expect("gas");
    (included, limit, cost)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_09_req_so_3_with_no_dry_run_to_be_had_nothing_is_signed() {
    let mut k = serving_kit(KitOptions::default()).await;
    // The light client answered the suite's dry runs of `execute` with "Error trying to access
    // the storage required for the runtime call".
    k.chain.refuse_dry_runs(true);
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    assert_eq!(k.step().await.unwrap().signed, 0);
    assert!(open_alert(&k, "dry-run").await);
    assert_eq!(
        instruction(&k, &op).await.state,
        InstructionState::Acknowledged
    );
    // Not asked for again for a minute (each failed one gets the light client's peer banned),
    // even with dry runs back.
    k.chain.refuse_dry_runs(false);
    let before = k.chain.dry_runs();
    k.rounds(3).await.unwrap();
    assert_eq!(k.chain.dry_runs(), before);
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&op)), 0);
    // Past the pause: dry-run, signed, settled, and the alert clears.
    k.pass(60_000).await;
    assert_eq!(until(&mut k, &alice, &op, "Settled", 4).await, "Settled");
    assert!(k.chain.dry_runs() > before);
    assert!(!open_alert(&k, "dry-run").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_09_req_so_3_half_again_covers_a_batch_its_dry_run_priced_on_the_cheap_path() {
    let n = CHEAP_PATH_CALLS as u64;
    let (included, limit, cost) = priced_on_the_cheap_path(150).await;
    // Priced as refused, run as applied.
    let priced = plus(EXECUTE_BASE, CALL_REFUSED, n);
    assert_eq!(limit, priced.with_margin(150));
    assert_eq!(cost, plus(EXECUTE_BASE, WITHDRAWAL_APPLIED, n));
    assert!(!cost.fits(priced), "the dry run priced the cheaper path");
    assert!(included.success, "{included:?}");

    // Without the percentage, only the fixed floor above the dry run (2 G of ref time, 92 KiB of
    // proof size): the same batch runs out of gas and fails whole.
    let (included, limit, cost) = priced_on_the_cheap_path(100).await;
    assert_eq!(
        limit,
        plus(
            priced,
            Weight {
                ref_time: 2_000_000_000,
                proof_size: 92 * 1024
            },
            1
        )
    );
    assert!(!cost.fits(limit));
    assert!(!included.success);
    assert_eq!(included.error.as_deref(), Some(OUT_OF_GAS));
}
