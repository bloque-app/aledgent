// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The deposit backing check, end to end against the fake settlement layer and the in-process
//! core: a final deposit is reported with its movement only if it is matched, one to one, to an
//! unclaimed transfer of its asset and amount from its sender to the contract account, earlier in
//! the same extrinsic. A credit recorded with nothing moved
//! ([`settle::chain::fake::FakeChain::forge_credit`]) is reported without a movement, so the core
//! sets it aside and credits nothing; so is a deposit in a block the history endpoint served, whose
//! events nothing proves. Nothing here stops the sync.

use ingress::testkit::Party;
use ledger_core::id::{AccountId, account_reference};
use ledger_core::settlement::Incident;
use settle::adapter::{UNBACKED_ALERT, UNVERIFIED_ALERT};
use settle::chain::fake::{HistoryFault, position_account};
use settle::chain::{Chain, EventOrigin, InboxEvent, TransferIn};
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
    k.chain.mint(FORGER, &chain_usd(), 1_000_000);
    k
}

/// The contract account of the kit's deployment.
fn contract() -> [u8; 32] {
    position_account(INSTANCE)
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
    assert!(open_alerts(&k, "deposit-").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_credit_is_reported_without_a_movement_and_credits_nothing() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    let held = k.chain.balance_of(&contract(), &chain_usd());
    k.chain
        .forge_credit(FORGER, INSTANCE, reference(&alice), &chain_usd(), 5_000);
    settle_block(&mut k).await;
    // Nothing moved on the layer, and nothing is credited in the book.
    assert_eq!(k.chain.balance_of(&contract(), &chain_usd()), held);
    assert_eq!(k.balances(&alice).await, (0, 0));
    let [(seq, block, 5_000)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    // The core took the record in, set it aside, and the cursor passed it.
    assert!(core_seqs(&k).contains(&seq));
    assert!(unusable(&k, seq), "set aside as unusable");
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), seq + 1);
    assert_eq!(head_position(&k).await, seq + 1);
    // The fault names the entry, its block, the asset, the amount and the sender.
    let alerts = open_alerts(&k, &format!("{UNBACKED_ALERT}:{INSTANCE}:{seq}")).await;
    let [alert] = &alerts[..] else {
        panic!("{alerts:?}");
    };
    for part in [
        format!("position {seq}"),
        format!("block #{block}"),
        chain_usd().0,
        "amount 5000".to_owned(),
        hex::encode(FORGER),
        "not backed".to_owned(),
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

/// Each deposit is matched to its own transfer: a genuine one beside a forgery in the same block is
/// credited, the forgery is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_genuine_deposit_beside_a_forged_credit_is_credited_alone() {
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
    assert_eq!(k.balances(&alice).await, (1_000, 0));
    assert_eq!(k.balances(&mallory).await, (0, 0));
    assert!(unusable(&k, found[1].0));
    assert_eq!(open_alerts(&k, UNBACKED_ALERT).await.len(), 1);
}

/// The first review's finding 1: a plain transfer into the contract account in the same block
/// (another extrinsic) does not cover a forged credit of the same size, whatever their order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transfer_in_the_same_block_does_not_cover_a_forgery() {
    let mut k = serving_kit().await;
    let mallory = k.d.party("mallory", 0).await;
    // The forger's own transfer before the forged credit, and a third party's after it.
    k.chain.transfer(FORGER, contract(), &chain_usd(), 5_000);
    k.chain
        .forge_credit(FORGER, INSTANCE, reference(&mallory), &chain_usd(), 5_000);
    k.chain.transfer(DEPOSITOR, contract(), &chain_usd(), 5_000);
    settle_block(&mut k).await;
    assert_eq!(deposits(&k).len(), 1);
    assert_eq!(k.balances(&mallory).await, (0, 0));
    assert_eq!(open_alerts(&k, UNBACKED_ALERT).await.len(), 1);
}

/// The first review's finding 2: funding, a binding and a forged credit in one block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn funding_a_bind_and_a_forgery_in_one_block_credit_nothing() {
    let mut k = serving_kit().await;
    let mallory = k.d.party("mallory", 0).await;
    k.chain.transfer(DEPOSITOR, contract(), &chain_usd(), 400);
    k.chain.bind(INSTANCE, &chain_usd(), 400);
    k.chain
        .forge_credit(FORGER, INSTANCE, reference(&mallory), &chain_usd(), 400);
    settle_block(&mut k).await;
    assert!(
        k.chain
            .finalized_entries(INSTANCE)
            .iter()
            .any(|(_, e)| matches!(e.event, InboxEvent::Bound { amount: 400, .. })),
        "the binding applied"
    );
    assert_eq!(k.balances(&mallory).await, (0, 0));
    assert_eq!(open_alerts(&k, UNBACKED_ALERT).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bind_and_a_deposit_in_one_block_are_backed() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    // A holding the contract does not record yet: what a binding records.
    k.chain.transfer(DEPOSITOR, contract(), &chain_usd(), 400);
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
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    assert!(open_alerts(&k, "deposit-").await.is_empty());
}

/// One transfer backs one entry: a sender that pays the contract account in its own extrinsic
/// backs one credit of that amount (value did arrive), and a second credit against the same
/// transfer is not backed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_deposits_against_one_transfer_back_only_the_first() {
    let mut k = serving_kit().await;
    let mallory = k.d.party("mallory", 0).await;
    let to = reference(&mallory);
    k.chain.in_one_extrinsic(|c| {
        c.transfer(FORGER, contract(), &chain_usd(), 700);
        c.forge_credit(FORGER, INSTANCE, to, &chain_usd(), 700);
        c.forge_credit(FORGER, INSTANCE, to, &chain_usd(), 700);
    });
    settle_block(&mut k).await;
    let found = deposits(&k);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(k.balances(&mallory).await, (700, 0), "what was paid, once");
    let alerts = open_alerts(&k, UNBACKED_ALERT).await;
    assert_eq!(alerts.len(), 1);
    assert_eq!(
        alerts[0].key,
        format!("{UNBACKED_ALERT}:{INSTANCE}:{}", found[1].0)
    );
}

/// A transfer after the entry, in the same extrinsic, does not back it either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transfer_after_the_entry_does_not_back_it() {
    let mut k = serving_kit().await;
    let mallory = k.d.party("mallory", 0).await;
    let to = reference(&mallory);
    k.chain.in_one_extrinsic(|c| {
        c.forge_credit(FORGER, INSTANCE, to, &chain_usd(), 700);
        c.transfer(FORGER, contract(), &chain_usd(), 700);
    });
    settle_block(&mut k).await;
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
    assert!(open_alerts(&k, "deposit-").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_deployments_credit_is_backed() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    // Another deployment of the same contract withdraws to this one: it moves the value first,
    // then has this contract credit it, the entry's sender being that deployment.
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
    assert!(open_alerts(&k, "deposit-").await.is_empty());
}

/// Deposits in blocks only the history endpoint still serves: their events are folded, but
/// nothing proves their transfers, so they are reported without a movement, with their own fault,
/// and the sync goes on. No state is read for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deposits_the_history_endpoint_served_are_held_and_never_stop_the_sync() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.advance(300);
    k.chain.keep_state(Some(0));
    k.chain.serve_history(true);
    k.step().await.unwrap();
    let [(seq, _, 1_200)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    assert!(k.chain.reads().history > 0);
    assert_eq!(k.balances(&alice).await, (0, 0));
    assert!(unusable(&k, seq));
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), seq + 1);
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    let alerts = open_alerts(&k, &format!("{UNVERIFIED_ALERT}:{INSTANCE}:{seq}")).await;
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(
        alerts[0].detail.contains("history endpoint"),
        "{}",
        alerts[0].detail
    );
    // A deposit the adapter's own client serves is credited as ever.
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 300);
    settle_block(&mut k).await;
    assert_eq!(k.balances(&alice).await, (300, 0));
}

/// A history endpoint that names a block as its own parent (the first review's finding 3) is
/// rejected: its header cannot hash to the block, and nothing is reported past the range.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_history_endpoint_with_a_wrong_parent_is_rejected() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    let cursor = k.store.cursor(INSTANCE).await.unwrap();
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.advance(300);
    k.chain.serve_history(true);
    k.chain.history_lies(Some(HistoryFault::WrongParent));
    k.step().await.unwrap();
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), cursor);
    assert!(core_seqs(&k).is_empty());
    let alerts = open_alerts(&k, &format!("unreadable:{INSTANCE}:{cursor}")).await;
    assert_eq!(alerts.len(), 1);
    assert!(
        alerts[0].detail.contains("does not hash"),
        "{}",
        alerts[0].detail
    );
    // An honest endpoint: the range reads, and the deposit is held as unverified.
    k.chain.history_lies(None);
    k.step().await.unwrap();
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), cursor + 1);
    assert_eq!(k.balances(&alice).await, (0, 0));
    assert_eq!(open_alerts(&k, UNVERIFIED_ALERT).await.len(), 1);
}

/// A history endpoint that adds a transfer to back a forged credit gains nothing: a block it served
/// backs no deposit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_history_endpoint_that_forges_a_transfer_credits_nothing() {
    let mut k = serving_kit().await;
    let mallory = k.d.party("mallory", 0).await;
    k.chain
        .forge_credit(FORGER, INSTANCE, reference(&mallory), &chain_usd(), 900);
    k.chain.produce();
    k.chain.advance(300);
    k.chain.serve_history(true);
    k.chain
        .history_lies(Some(HistoryFault::Transfer(TransferIn {
            origin: EventOrigin {
                extrinsic: Some(0),
                index: 0,
            },
            from: FORGER,
            asset: chain_usd(),
            amount: 900,
        })));
    k.step().await.unwrap();
    assert_eq!(deposits(&k).len(), 1);
    assert_eq!(k.balances(&mallory).await, (0, 0));
    assert_eq!(open_alerts(&k, UNVERIFIED_ALERT).await.len(), 1);
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

/// The verdict is reached when the entry is verified and kept with it: a core away for longer than
/// the node keeps events gets each deposit, with or without its movement as found, once it is
/// back, and nothing is read again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verdict_outlives_a_core_away_for_long() {
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
    // Back: the deposits' blocks' events are gone from the client, and the kept verdicts decide.
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

/// A deposit kept before the check existed (no verdict for it) is reported without a movement,
/// with the fault, and the cursor passes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deposit_kept_before_the_upgrade_is_held() {
    let k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
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
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.finalize_all();
    assert!(a.step().await.is_err());
    let [(seq, _, 1_200)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    // As the earlier code kept it: the entry, and no verdict for it (a row for another entry at the
    // position stands in for none).
    k.store
        .put_backing(INSTANCE, seq, &[0; 32], &Backing::Backed)
        .await
        .unwrap();
    down.store(false, std::sync::atomic::Ordering::SeqCst);
    a.step().await.unwrap();
    assert_eq!(k.balances(&alice).await, (0, 0));
    assert!(unusable(&k, seq));
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), seq + 1);
    let alerts = open_alerts(&k, &format!("{UNBACKED_ALERT}:{INSTANCE}:{seq}")).await;
    assert_eq!(alerts.len(), 1);
    assert!(
        alerts[0].detail.contains("never proved"),
        "{}",
        alerts[0].detail
    );
}

/// The verdicts go with the entries on a reset of the chain, so none decides an entry of the chain
/// after it; and a verdict is never taken for another entry at its position.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verdicts_are_cleared_on_a_reset_and_bound_to_their_entry() {
    let k = serving_kit().await;
    let unbacked = Backing::Unbacked {
        detail: "not backed".into(),
    };
    k.store
        .put_backing(INSTANCE, 3, &[3; 32], &unbacked)
        .await
        .unwrap();
    k.store
        .put_backing(INSTANCE + 1, 3, &[3; 32], &Backing::Backed)
        .await
        .unwrap();
    assert_eq!(
        k.store.backing(INSTANCE, 3, &[3; 32]).await.unwrap(),
        Some(unbacked)
    );
    assert_eq!(k.store.backing(INSTANCE, 3, &[4; 32]).await.unwrap(), None);
    k.store.reset_chain(INSTANCE, 0).await.unwrap();
    assert_eq!(k.store.backing(INSTANCE, 3, &[3; 32]).await.unwrap(), None);
    // Another instance's are its own.
    assert_eq!(
        k.store.backing(INSTANCE + 1, 3, &[3; 32]).await.unwrap(),
        Some(Backing::Backed)
    );
}

/// The second review's finding 1: one failed read of the adapter's own client (a timeout, a light
/// client not yet synced), for a block within what it keeps, is a blip. The step stops at the first
/// new position and reads again at the next; the block is never taken from the history endpoint,
/// whose blocks back no deposit, so the deposit is credited once the client answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blip_of_the_client_delays_a_deposit_and_never_holds_it() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    let cursor = k.store.cursor(INSTANCE).await.unwrap();
    k.chain.serve_history(true);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.finalize_all();
    k.chain.blip(1);
    let before = k.chain.reads();
    k.step().await.unwrap();
    assert_eq!(k.chain.reads().history, before.history, "no history read");
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), cursor);
    let alerts = open_alerts(&k, &format!("unreadable:{INSTANCE}:{cursor}")).await;
    assert_eq!(alerts.len(), 1);
    assert!(
        alerts[0].detail.contains("read again"),
        "{}",
        alerts[0].detail
    );
    k.step().await.unwrap();
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    assert!(open_alerts(&k, "deposit-").await.is_empty());
}

/// The second review's finding 2: a transfer to the contract whose asset id the adapter cannot
/// decode backs nothing, and stops nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transfer_of_an_undecodable_asset_backs_nothing_and_stops_nothing() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain.undecodable_transfers(true);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    settle_block(&mut k).await;
    let [(seq, _, 1_200)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    assert_eq!(k.balances(&alice).await, (0, 0));
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), seq + 1);
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    assert_eq!(open_alerts(&k, UNBACKED_ALERT).await.len(), 1);
    // What decodes again backs as ever.
    k.chain.undecodable_transfers(false);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 300);
    settle_block(&mut k).await;
    assert_eq!(k.balances(&alice).await, (300, 0));
}

/// The final review: a node that keeps 64 blocks, after an outage of 100. It says the block is
/// gone (`State already discarded`), so the walk reads it, and every older one, from the history
/// endpoint at once, though it is far shallower than 256: the adapter catches up, the deposit held
/// as unverified, and nothing stalls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_keeping_64_blocks_recovers_through_history_at_once() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain.keep_events(64);
    k.chain.serve_history(true);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.advance(100);
    k.step().await.unwrap();
    let [(seq, _, 1_200)] = deposits(&k)[..] else {
        panic!("{:?}", deposits(&k));
    };
    assert!(k.chain.reads().history > 0);
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), seq + 1);
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    assert_eq!(open_alerts(&k, UNVERIFIED_ALERT).await.len(), 1);
    assert_eq!(k.balances(&alice).await, (0, 0));
}

/// The same through the light client, whose answer for a pruned block (`RemoteCouldntAnswer`)
/// does not say the block is gone: each step stops and retries, and after
/// `own_client_retries` (10) walks in a row the walk reads the block from the history endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_light_client_whose_peers_keep_64_blocks_recovers_after_its_retries() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain.keep_events(64);
    k.chain.light_client_errors(true);
    k.chain.serve_history(true);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.advance(100);
    let mut steps = 0;
    while k.store.cursor(INSTANCE).await.unwrap() == 0 {
        steps += 1;
        assert!(steps <= 10, "still stalled after {steps} steps");
        k.step().await.unwrap();
        if steps < 10 {
            assert_eq!(
                k.chain.reads().history,
                0,
                "retried, not taken from history"
            );
            k.chain.advance(1);
        }
    }
    assert_eq!(steps, 10);
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    assert_eq!(open_alerts(&k, UNVERIFIED_ALERT).await.len(), 1);
    assert_eq!(k.balances(&alice).await, (0, 0));
}

/// A blip that lasts several steps, fewer than `own_client_retries`, still never holds a deposit:
/// once the client answers, it is credited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blip_of_several_steps_is_still_credited() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain.serve_history(true);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.finalize_all();
    for _ in 0..9 {
        k.chain.blip(1);
        k.step().await.unwrap();
    }
    assert_eq!(k.chain.reads().history, 0);
    assert_eq!(k.balances(&alice).await, (0, 0));
    k.step().await.unwrap();
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    assert!(open_alerts(&k, "deposit-").await.is_empty());
}

/// The review of bcd834a: ten failed steps of the client at the newest block (peer churn, an
/// eclipse of the light client's peers) never send it to the history endpoint: above
/// `own_client_min_fallback_depth` the walk stays on the client, and once it answers the deposit is
/// credited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_blip_at_a_shallow_block_never_holds_its_deposit() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain.serve_history(true);
    k.chain.light_client_errors(true);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.finalize_all();
    for _ in 0..12 {
        k.chain.blip(1);
        k.step().await.unwrap();
    }
    assert_eq!(k.chain.reads().history, 0);
    assert_eq!(k.balances(&alice).await, (0, 0));
    k.step().await.unwrap();
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    assert!(open_alerts(&k, "deposit-").await.is_empty());
}

/// Peers that keep fewer blocks than `own_client_min_fallback_depth` (16 < 32), after an outage
/// longer than they keep: neither a node's `State already discarded` nor the light client's
/// retries are taken above that depth, so the adapter stalls at the first new position with
/// `unreadable:` open (it pages) until the operator acts. Lowering the depth below what the peers
/// keep is one way out: the node's answer then sends the block to the history endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_keeping_fewer_blocks_than_the_min_depth_stall_until_the_operator_acts() {
    for light in [false, true] {
        let mut k = serving_kit().await;
        let alice = k.d.party("alice", 0).await;
        k.chain.keep_events(16);
        k.chain.light_client_errors(light);
        k.chain.serve_history(true);
        k.chain
            .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
        k.chain.produce();
        k.chain.advance(100);
        for _ in 0..30 {
            k.step().await.unwrap();
            k.chain.advance(1);
        }
        assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 0, "stalled");
        assert_eq!(k.chain.reads().history, 0);
        assert_eq!(open_alerts(&k, "unreadable:").await.len(), 1);
        if !light {
            // The operator lowers the depth below what the node keeps.
            k.reconfigure(|c| c.own_client_min_fallback_depth = 8);
            k.step().await.unwrap();
            assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 1);
            assert_eq!(open_alerts(&k, UNVERIFIED_ALERT).await.len(), 1);
        }
    }
}

/// The review of f9eccfd, finding 1: a lagging backend of a load-balanced RPC pool that calls the
/// newest block unknown (`UnknownBlock`) is not taken at its word above
/// `own_client_min_fallback_depth`: the step retries, and once the block is known the deposit is
/// credited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_block_at_depth_1_is_retried_not_held() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain.serve_history(true);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    let b = k.chain.produce().unwrap();
    k.chain.produce();
    k.chain.finalize_all();
    k.chain.unknown_to_client(Some(b.number));
    for _ in 0..3 {
        k.step().await.unwrap();
    }
    assert_eq!(k.chain.reads().history, 0);
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 0);
    k.chain.unknown_to_client(None);
    k.step().await.unwrap();
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    assert!(open_alerts(&k, "deposit-").await.is_empty());
}

/// The review of f9eccfd, finding 2: shallow blips between the deep failures of a light client
/// whose peers keep 64 blocks, after an outage of 100, do not start the count again: the walk
/// still reaches the history endpoint after its retries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shallow_blips_do_not_starve_the_deep_fallback() {
    let mut k = serving_kit().await;
    let alice = k.d.party("alice", 0).await;
    k.chain.keep_events(64);
    k.chain.light_client_errors(true);
    k.chain.serve_history(true);
    k.chain
        .deposit(DEPOSITOR, INSTANCE, reference(&alice), &chain_usd(), 1_200);
    k.chain.produce();
    k.chain.advance(100);
    let mut steps = 0;
    while k.store.cursor(INSTANCE).await.unwrap() == 0 {
        steps += 1;
        assert!(steps <= 20, "still stalled after {steps} steps");
        if steps % 5 == 0 {
            k.chain.blip(1);
        }
        k.step().await.unwrap();
        k.chain.advance(1);
    }
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    assert_eq!(open_alerts(&k, UNVERIFIED_ALERT).await.len(), 1);
}
