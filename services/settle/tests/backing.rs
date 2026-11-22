// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The deposit backing check, end to end against the fake settlement layer and the in-process
//! core: a final deposit is reported with its movement only if the block that wrote it raised the
//! deployment's real holding at least as much as the position its contract records (less what the
//! block's bindings recorded of a holding already there). A credit recorded with nothing moved
//! ([`settle::chain::fake::FakeChain::forge_credit`]) is reported without a movement, so the core
//! sets it aside and credits nothing; state the check cannot read stops the sync.

use ingress::testkit::Party;
use ledger_core::id::{AccountId, account_reference};
use ledger_core::settlement::Incident;
use settle::adapter::UNBACKED_ALERT;
use settle::chain::fake::position_account;
use settle::chain::{Chain, InboxEvent};
use settle::store::{Backing, Store};
use settle::testkit::{INSTANCE, Kit, KitOptions, chain_usd};

/// Who pays genuine deposits.
const DEPOSITOR: [u8; 32] = [0x77; 32];
/// The contract that forges credits.
const FORGER: [u8; 32] = [0x66; 32];

async fn serving_kit() -> Kit {
    let mut k = Kit::start_with(KitOptions::default()).await;
    let r = k.step().await.unwrap();
    assert!(r.serving, "{:?}", k.adapter.status());
    k.chain.mint(DEPOSITOR, &chain_usd(), 1_000_000);
    k
}

fn reference(p: &Party) -> [u8; 32] {
    account_reference(&AccountId::parse(p.account.as_deref().unwrap()).unwrap())
}

async fn open_alerts(k: &Kit, prefix: &str) -> Vec<settle::store::AlertRow> {
    k.store
        .alerts()
        .await
        .unwrap()
        .into_iter()
        .filter(|a| a.key.starts_with(prefix) && a.cleared_at.is_none())
        .collect()
}

/// The finalized chain's deposits: (position, block, amount).
fn deposits(k: &Kit) -> Vec<(u64, u64, u128)> {
    k.chain
        .finalized_entries(INSTANCE)
        .into_iter()
        .filter_map(|(seq, e)| match e.event {
            InboxEvent::Deposited { amount, .. } => Some((seq, e.at, amount)),
            _ => None,
        })
        .collect()
}

/// The positions the core took in.
fn core_seqs(k: &Kit) -> Vec<u64> {
    k.core_settlement(|s| {
        s.all_ingested()
            .filter(|i| i.observation.coordinate.instance == INSTANCE)
            .map(|i| i.observation.coordinate.seq)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    })
    .unwrap()
}

/// Whether the core set the record at `seq` aside as unusable.
fn unusable(k: &Kit, seq: u64) -> bool {
    k.core_settlement(|s| {
        s.incidents().iter().any(|i| {
            matches!(i, Incident::Unusable { coordinate }
                if coordinate.instance == INSTANCE && coordinate.seq == seq)
        })
    })
    .unwrap()
}

async fn head_position(k: &Kit) -> u64 {
    let fin = k.chain.finalized().await.unwrap();
    k.chain
        .chain_head(&fin, INSTANCE)
        .await
        .unwrap()
        .unwrap()
        .position
}

/// Produce one block with what is queued, finalize it, and step.
async fn settle_block(k: &mut Kit) {
    k.chain.produce();
    k.chain.finalize_all();
    k.step().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_genuine_deposit_is_backed_and_credited() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    settle_block(&mut k).await;
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    let [(seq, _, 1_200)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), seq + 1);
    assert!(open_alerts(&k, UNBACKED_ALERT).await.is_empty());
}

/// A transport to ingress that can be cut: the core unreachable.
#[derive(Clone)]
struct Cuttable {
    inner: settle::link::RouterTransport,
    down: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl settle::link::Transport for Cuttable {
    fn call<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        client_key: Option<&'a str>,
        body: Option<serde_json::Value>,
    ) -> settle::signer::BoxFuture<'a, Result<(u16, serde_json::Value), settle::link::LinkError>>
    {
        if self.down.load(std::sync::atomic::Ordering::SeqCst) {
            return Box::pin(async { Err(settle::link::LinkError("ingress is down".into())) });
        }
        self.inner.call(method, path, client_key, body)
    }
}

/// The verdict is proved when the entry is verified, while the state is fresh, and kept with it:
/// a core away for longer than the node keeps state gets each deposit, with or without its
/// movement as proved, once it is back, and nothing is read again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verdict_outlives_a_core_away_for_longer_than_state_is_kept() {
    let k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    let mallory = k.d.party("mallory", 0).await;
    let down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut a = settle::Adapter::new(
        k.adapter.config().clone(),
        k.chain.clone(),
        k.store.clone(),
        Cuttable {
            inner: settle::link::RouterTransport::new(
                k.d.router.clone(),
                settle::testkit::ADAPTER_TOKEN,
            ),
            down: down.clone(),
        },
        k.lanes.clone(),
        std::sync::Arc::new(k.d.clock.clone()),
    );
    a.step().await.unwrap();
    down.store(true, std::sync::atomic::Ordering::SeqCst);
    k.chain.keep_state(Some(256));
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain
        .forge_credit(FORGER, INSTANCE, reference(&mallory), &chain_usd(), 900);
    k.chain.produce();
    k.chain.finalize_all();
    // The core is away: each step fails, but verifies and keeps the chain.
    assert!(a.step().await.is_err());
    let [(genuine, _, 1_200), (forged, _, 900)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    let digest = |seq: u64| {
        k.chain
            .finalized_entries(INSTANCE)
            .into_iter()
            .find(|(s, _)| *s == seq)
            .map(|(_, e)| e.digest())
            .unwrap()
    };
    assert_eq!(
        k.store
            .backing(INSTANCE, genuine, &digest(genuine))
            .await
            .unwrap(),
        Some(Backing::Backed)
    );
    assert!(matches!(
        k.store
            .backing(INSTANCE, forged, &digest(forged))
            .await
            .unwrap(),
        Some(Backing::Unbacked { .. })
    ));
    for _ in 0..300 {
        k.chain.advance(1);
        assert!(a.step().await.is_err());
    }
    // Back: the deposits' blocks' state is gone, and the kept verdicts decide.
    down.store(false, std::sync::atomic::Ordering::SeqCst);
    let r = a.step().await.unwrap();
    assert_eq!(r.observed, 2);
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    assert_eq!(k.balances(&mallory).await, (0, 0));
    assert!(unusable(&k, forged));
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), forged + 1);
    // Once taken in, the verdicts go with the entries.
    assert_eq!(
        k.store
            .backing(INSTANCE, genuine, &digest(genuine))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_credit_is_reported_without_a_movement_and_credits_nothing() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    let held = k
        .chain
        .balance_of(&position_account(INSTANCE), &chain_usd());
    k.chain
        .forge_credit(FORGER, INSTANCE, reference(&alice), &chain_usd(), 5_000);
    settle_block(&mut k).await;
    // Nothing moved on the layer, and nothing is credited in the book.
    assert_eq!(
        k.chain
            .balance_of(&position_account(INSTANCE), &chain_usd()),
        held
    );
    assert_eq!(k.balances(&alice).await, (0, 0));
    let [(seq, block, 5_000)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    // The core took the record in, set it aside, and the cursor passed it.
    assert!(core_seqs(&k).contains(&seq));
    assert!(unusable(&k, seq), "set aside as unusable");
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), seq + 1);
    assert_eq!(head_position(&k).await, seq + 1);
    // The fault names the block, the asset, both changes and the sender.
    let alerts = open_alerts(&k, &format!("{UNBACKED_ALERT}:{INSTANCE}:{seq}")).await;
    let [alert] = &alerts[..] else {
        panic!("{alerts:?}");
    };
    for part in [
        format!("block #{block}"),
        chain_usd().0,
        "changed by +0".to_owned(),
        "by +5000".to_owned(),
        hex::encode(FORGER),
    ] {
        assert!(alert.detail.contains(&part), "{part}: {}", alert.detail);
    }
    // The adapter goes on: a genuine deposit after it is credited.
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 300);
    settle_block(&mut k).await;
    assert_eq!(k.balances(&alice).await, (300, 0));
    assert_eq!(open_alerts(&k, UNBACKED_ALERT).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_genuine_deposit_and_a_forged_credit_in_one_block_are_both_held() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    let mallory = k.d.party("mallory", 0).await;
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_000);
    k.chain
        .forge_credit(FORGER, INSTANCE, reference(&mallory), &chain_usd(), 1_000);
    settle_block(&mut k).await;
    let found = deposits(&k);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].1, found[1].1, "one block");
    // Which of the two has nothing behind it cannot be told: neither is credited.
    assert_eq!(k.balances(&alice).await, (0, 0));
    assert_eq!(k.balances(&mallory).await, (0, 0));
    for (seq, _, _) in &found {
        assert!(unusable(&k, *seq));
    }
    assert_eq!(open_alerts(&k, UNBACKED_ALERT).await.len(), 2);
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), found[1].0 + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bind_and_a_deposit_in_one_block_are_backed() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    // A holding the contract does not record yet: what a binding records.
    k.chain
        .transfer(DEPOSITOR, position_account(INSTANCE), &chain_usd(), 400);
    settle_block(&mut k).await;
    k.chain.bind(INSTANCE, &chain_usd(), 400);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    settle_block(&mut k).await;
    let fin = k.chain.finalized().await.unwrap();
    let bound = k
        .chain
        .finalized_entries(INSTANCE)
        .into_iter()
        .filter(|(_, e)| matches!(e.event, InboxEvent::Bound { amount: 400, .. }))
        .map(|(_, e)| e.at)
        .collect::<Vec<_>>();
    assert_eq!(
        bound,
        vec![fin.number],
        "the binding applied in the deposit's block"
    );
    assert_eq!(deposits(&k)[0].1, fin.number);
    // The position rose by 1 600 and the holding by 1 200: the binding's 400 was already held.
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    assert!(open_alerts(&k, UNBACKED_ALERT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bind_does_not_cover_a_forged_credit() {
    let mut k = serving_kit().await;
    let mallory = k.d.party("mallory", 0).await;
    k.chain
        .transfer(DEPOSITOR, position_account(INSTANCE), &chain_usd(), 400);
    settle_block(&mut k).await;
    k.chain.bind(INSTANCE, &chain_usd(), 400);
    k.chain
        .forge_credit(FORGER, INSTANCE, reference(&mallory), &chain_usd(), 400);
    settle_block(&mut k).await;
    assert_eq!(deposits(&k).len(), 1);
    assert_eq!(k.balances(&mallory).await, (0, 0));
    assert_eq!(open_alerts(&k, UNBACKED_ALERT).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_withdrawal_and_a_deposit_in_one_block_are_backed() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 10_000).await;
    let bob = k.d.party("bob", 0).await;
    let op = k.withdrawal(&alice, 250, "w1").await;
    let r = k.step().await.unwrap();
    assert_eq!(r.signed, 1);
    // The withdrawal's transaction and a deposit land in the same block.
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&bob), &chain_usd(), 700);
    settle_block(&mut k).await;
    let entries = k.chain.finalized_entries(INSTANCE);
    let blocks: Vec<u64> = entries.iter().map(|(_, e)| e.at).collect();
    assert_eq!(blocks.len(), 2, "{entries:?}");
    assert_eq!(blocks[0], blocks[1], "one block");
    assert!(
        entries
            .iter()
            .any(|(_, e)| matches!(e.event, InboxEvent::Withdrawn { .. }))
    );
    assert_eq!(k.balances(&bob).await, (700, 0));
    assert_eq!(k.state(&alice, &op).await, "Settled");
    assert!(open_alerts(&k, UNBACKED_ALERT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_deployments_credit_is_backed() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    // Another deployment of the same contract withdraws to this one: it moves the value first,
    // then has this contract credit it.
    k.chain.register_instance(2, &[[0x92; 32]]);
    k.chain.fund_position(2, &chain_usd(), 5_000);
    k.chain
        .peer_withdraw(2, INSTANCE, reference(&alice), &chain_usd(), 800);
    settle_block(&mut k).await;
    let [(_, _, 800)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    assert_eq!(
        k.chain.balance_of(&position_account(2), &chain_usd()),
        4_200
    );
    assert_eq!(k.balances(&alice).await, (800, 0));
    assert!(open_alerts(&k, UNBACKED_ALERT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_parent_state_stops_the_sync_and_leaves_the_cursor() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    let cursor = k.store.cursor(INSTANCE).await.unwrap();
    // The node keeps the finalized block's state only: the deposit's block reads, its parent not.
    k.chain.keep_state(Some(0));
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    settle_block(&mut k).await;
    let [(seq, block, _)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    assert_eq!(seq, cursor);
    // Nothing reported, nothing credited, the cursor in place, and the reason raised.
    assert_eq!(k.balances(&alice).await, (0, 0));
    assert!(!core_seqs(&k).contains(&seq));
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), cursor);
    let alerts = open_alerts(&k, &format!("unreadable:{INSTANCE}:{seq}")).await;
    let [alert] = &alerts[..] else {
        panic!("{alerts:?}");
    };
    assert!(
        alert.detail.contains(&format!("block #{}", block - 1)),
        "{}",
        alert.detail
    );
    assert!(alert.detail.contains("backing"), "{}", alert.detail);
    // Still unreadable: still stopped.
    k.step().await.unwrap();
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), cursor);
    // The state reads again (a node that serves it): the deposit is proved, and credited.
    k.chain.keep_state(None);
    k.step().await.unwrap();
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), seq + 1);
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
}
