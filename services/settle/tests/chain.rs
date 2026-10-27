// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Settle verifies the chain of entries at each sync (`T-043-04`, amendment 0040
//! `[PENDING AMENDMENT 0040]`, `REQ-IN-5`, `REQ-SA-7`, `AC-D2.1`): the head is read at the
//! finalized block, and only when it moved are the events of the blocks since folded from the head
//! last accepted. A fold that does not reach the head stops the adapter at the first new position,
//! with an alert. The re-scan is gone (`T-043-05`).

use serde_json::Value;
use settle::adapter::{AdapterError, CHAIN_ALERT, CrashPoint};
use settle::chain::fake::ReaderFault;
use settle::chain::kreivo::{self, topic};
use settle::chain::{Chain, ChainEvent, ChainHead, InboxEntry, ZERO_DIGEST, fold};
use settle::store::{AcceptedHead, Store};
use settle::testkit::{INSTANCE, Kit, KitOptions, chain_usd};

fn h32(s: &str) -> [u8; 32] {
    hex::decode(s).unwrap().try_into().unwrap()
}

async fn serving_kit() -> Kit {
    let mut k = Kit::start_with(KitOptions::default()).await;
    let r = k.step().await.unwrap();
    assert!(r.serving, "{:?}", k.adapter.status());
    k
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

/// `n` deposits of 1, one a block, not finalized.
fn deposits(k: &Kit, n: usize) {
    let from = [0x77; 32];
    k.chain.mint(from, &chain_usd(), 1_000);
    for _ in 0..n {
        k.chain.deposit(from, INSTANCE, [0x42; 32], &chain_usd(), 1);
        k.chain.produce();
    }
}

async fn head(k: &Kit) -> ChainHead {
    let fin = k.chain.finalized().await.unwrap();
    k.chain.chain_head(&fin, INSTANCE).await.unwrap().unwrap()
}

/// The positions the core took in, final.
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

// ------------------------------------------------------------------------------ the fold

/// The contract's events as its own unit tests emit them (`contracts/settlement`, the scenario of
/// `t_043_02_a_fold_of_the_events_alone_gives_the_head`, then a reset with an anchor in the same
/// block and a deposit after), decoded here as the adapter decodes Kreivo's: every entry kind, a
/// failed frame's refusal and a credit, rebuilt from its event and its block's number alone, folds
/// to the heads the contract held. The signature topics are ink!'s, from the field types.
#[test]
fn t_043_04_the_contracts_own_events_fold_to_its_head() {
    let f: Value =
        serde_json::from_str(include_str!("../fixtures/chain-events.json")).expect("the fixture");
    let me = f["contract"].as_str().unwrap();
    let head_of = |v: &Value| ChainHead {
        position: v["position"].as_u64().unwrap(),
        digest: h32(v["digest"].as_str().unwrap()),
        start: v["start"].as_u64().unwrap(),
    };
    let mut before: Vec<(u64, InboxEntry)> = Vec::new();
    let mut after: Vec<(u64, InboxEntry)> = Vec::new();
    let mut resets = 0;
    for e in f["events"].as_array().unwrap() {
        if e["contract"].as_str().unwrap() != me {
            continue;
        }
        let topics: Vec<[u8; 32]> = e["topics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| h32(t.as_str().unwrap()))
            .collect();
        let data = hex::decode(e["data"].as_str().unwrap()).unwrap();
        let at = e["block"].as_u64().unwrap();
        match kreivo::contract_event(&topics, &data, at, |_| None).unwrap() {
            Some(ChainEvent::Entry { seq, entry }) => {
                assert_eq!(entry.at, at);
                if resets == 0 {
                    before.push((seq, entry));
                } else {
                    after.push((seq, entry));
                }
            }
            Some(ChainEvent::Reset) => resets += 1,
            Some(ChainEvent::Legacy { .. }) => panic!("no earlier event here"),
            None => {}
        }
    }
    assert_eq!((before.len(), after.len(), resets), (9, 2, 1));
    fold(0, &ZERO_DIGEST, &before, &head_of(&f["head_before_reset"])).unwrap();
    fold(0, &ZERO_DIGEST, &after, &head_of(&f["head"])).unwrap();
    // The contract's pinned digest (`t_043_02_the_chain_digest_is_labelled_and_pinned`).
    let e0 = h32("01306698c3faca4813e818fafd0fe09f2977d886b76582e0e1c5fd527195500a");
    assert_eq!(
        settle::chain::chain_digest(&ZERO_DIGEST, 0, &e0),
        h32("8cdf807deac993ffabca639a554c713011a2edb44f3ed1fca81602403b631c15")
    );
    // One entry hidden, repeated or altered: the fold says no.
    let head = head_of(&f["head_before_reset"]);
    let mut hidden = before.clone();
    hidden.remove(4);
    assert!(fold(0, &ZERO_DIGEST, &hidden, &head).is_err());
    let mut extra = before.clone();
    extra.insert(4, before[4].clone());
    assert!(fold(0, &ZERO_DIGEST, &extra, &head).is_err());
    let mut altered = before.clone();
    altered[4].1.raw[0] ^= 1;
    assert!(fold(0, &ZERO_DIGEST, &altered, &head).is_err());
}

/// The signature topics are ink! 5's: `blake2b-256(Name(Type,...))` over the fields as the
/// contract writes them, the earlier code's two shapes included.
#[test]
fn t_043_04_the_event_topics_are_the_contracts() {
    for (topic, signature) in [
        (
            topic::DEPOSITED,
            "Deposited(InboxSeq,AccountId,AccountRef,AssetId,Balance)",
        ),
        (
            topic::WITHDRAWN,
            "Withdrawn(InboxSeq,OperationRef,Destination,AssetId,Balance)",
        ),
        (
            topic::ANCHORED,
            "Anchored(InboxSeq,OperationRef,AnchorNumber,Commitment)",
        ),
        (topic::BOUND, "Bound(InboxSeq,OperationRef,AssetId,Balance)"),
        (
            topic::REFUSED,
            "Refused(InboxSeq,OperationRef,Call,Refusal)",
        ),
        (
            topic::ANCHOR_HEAD_SET,
            "AnchorHeadSet(u32,AnchorNumber,InboxSeq,Commitment,Option<LatestAnchor>)",
        ),
        (
            topic::DEPOSITED_V2,
            "Deposited(InboxSeq,AccountRef,AssetId,Balance)",
        ),
        (topic::REFUSED_V2, "Refused(InboxSeq,OperationRef,Refusal)"),
    ] {
        assert_eq!(topic, kreivo::signature_topic(signature), "{signature}");
    }
    // An event in the earlier shape is recognised, never folded and never skipped silently.
    let mut data = 7u64.to_le_bytes().to_vec();
    data.extend([0u8; 40]);
    assert_eq!(
        kreivo::contract_event(&[topic::DEPOSITED_V2], &data, 1, |_| None).unwrap(),
        Some(ChainEvent::Legacy { seq: 7 })
    );
    // An event that is not about the chain is none; a known one with trailing bytes is an error.
    assert_eq!(
        kreivo::contract_event(&[[9; 32]], &[], 1, |_| None).unwrap(),
        None
    );
    assert!(kreivo::contract_event(&[topic::ANCHORED], &[0; 81], 1, |_| None).is_err());
}

// ------------------------------------------------------------------------------ one read

/// `T-043-04`: an idle sync is one read, the head's; a sync after new entries reads each block
/// since the head it accepted once, folds them, reports each, and accepts the head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_04_an_idle_sync_is_one_read() {
    let mut k = serving_kit().await;
    deposits(&k, 3);
    k.chain.finalize_all();
    let before = k.chain.reads();
    let fin = k.chain.finalized().await.unwrap();
    let accepted = k.store.accepted_head(INSTANCE).await.unwrap().unwrap();
    let r = k.step().await.unwrap();
    assert_eq!(r.observed, 3);
    let after = k.chain.reads();
    assert_eq!(after.heads - before.heads, 1);
    assert_eq!(after.blocks - before.blocks, fin.number - accepted.block);
    assert_eq!(after.history, 0);
    let h = head(&k).await;
    assert_eq!(h.position, 3);
    assert_eq!(
        k.store.accepted_head(INSTANCE).await.unwrap(),
        Some(AcceptedHead {
            position: 3,
            digest: h.digest,
            block: fin.number
        })
    );
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 3);
    assert_eq!(core_seqs(&k), vec![0, 1, 2]);
    // Idle: blocks come and go with nothing new. One read each, no event read.
    for _ in 0..3 {
        k.chain.advance(1);
        let before = k.chain.reads();
        let r = k.step().await.unwrap();
        let after = k.chain.reads();
        assert_eq!(r.observed, 0);
        assert_eq!(
            (after.heads - before.heads, after.blocks - before.blocks),
            (1, 0)
        );
    }
    // The accepted block follows the finalized head while idle, so the next entry's sync reads
    // only the blocks since.
    let fin = k.chain.finalized().await.unwrap();
    assert_eq!(
        k.store
            .accepted_head(INSTANCE)
            .await
            .unwrap()
            .unwrap()
            .block,
        fin.number
    );
    deposits(&k, 1);
    k.chain.finalize_all();
    let before = k.chain.reads();
    assert_eq!(k.step().await.unwrap().observed, 1);
    assert_eq!(k.chain.reads().blocks - before.blocks, 1);
}

// ------------------------------------------------------------------------------ faults

/// `T-043-04`, `REQ-IN-5`, `AC-D2.1`: an entry hidden, added or altered by the reader stops the
/// adapter at the first new position with an alert of kind `chain`, after reading once more;
/// nothing past it is reported, and the head is not accepted. The rest of the step goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_04_hidden_extra_and_altered_entries_stop_the_adapter_at_the_first_new_position() {
    for fault in [ReaderFault::Hide, ReaderFault::Extra, ReaderFault::Alter] {
        let mut k = serving_kit().await;
        deposits(&k, 1);
        k.chain.finalize_all();
        k.step().await.unwrap();
        assert_eq!(core_seqs(&k), vec![0], "{fault:?}");
        let accepted = k.store.accepted_head(INSTANCE).await.unwrap().unwrap();
        // Three more, the middle one misread on every read.
        deposits(&k, 3);
        k.chain.finalize_all();
        k.chain.fault_reads(2, fault, u32::MAX);
        let before = k.chain.reads();
        let r = k.step().await.unwrap();
        assert!(r.serving);
        assert_eq!(r.observed, 0, "{fault:?}");
        // Read twice: the first read and once more.
        assert_eq!(k.chain.reads().blocks - before.blocks, 2 * 3, "{fault:?}");
        let alerts = open_alerts(&k, &format!("{CHAIN_ALERT}:{INSTANCE}:1")).await;
        assert_eq!(alerts.len(), 1, "{fault:?}");
        assert_eq!(alerts[0].key.split(':').next(), Some("chain"));
        assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 1, "{fault:?}");
        assert_eq!(
            k.store.accepted_head(INSTANCE).await.unwrap(),
            Some(accepted)
        );
        assert_eq!(core_seqs(&k), vec![0], "{fault:?}");
        // Every later step stops there too.
        k.chain.advance(1);
        k.step().await.unwrap();
        assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 1, "{fault:?}");
    }
}

/// `T-043-04` (ruling a40-q3): a misread the second read does not repeat is no alert; and a
/// mismatch clears as soon as a read shows the range complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_04_a_mismatch_the_re_read_clears_raises_nothing() {
    let mut k = serving_kit().await;
    deposits(&k, 3);
    k.chain.finalize_all();
    k.chain.fault_reads(1, ReaderFault::Alter, 1);
    assert_eq!(k.step().await.unwrap().observed, 3);
    assert!(open_alerts(&k, CHAIN_ALERT).await.is_empty());
    // Two faulty reads in a row: an alert; the next step's clean read clears it.
    deposits(&k, 1);
    k.chain.finalize_all();
    k.chain.fault_reads(3, ReaderFault::Hide, 2);
    assert_eq!(k.step().await.unwrap().observed, 0);
    assert_eq!(open_alerts(&k, CHAIN_ALERT).await.len(), 1);
    assert_eq!(k.step().await.unwrap().observed, 1);
    assert!(open_alerts(&k, CHAIN_ALERT).await.is_empty());
    assert_eq!(core_seqs(&k), vec![0, 1, 2, 3]);
}

// ------------------------------------------------------------------------------ history

/// `T-043-04`: an adapter more than 256 blocks behind cannot read the blocks its client no longer
/// keeps events for: an alert, and no advance. With a history endpoint it catches up, reading
/// there only what its client cannot serve, and folding it against the head its client reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_04_behind_more_than_256_blocks_needs_the_history_endpoint() {
    let mut k = serving_kit().await;
    deposits(&k, 2);
    k.chain.advance(300);
    let r = k.step().await.unwrap();
    assert!(r.serving);
    assert_eq!(r.observed, 0);
    let alerts = open_alerts(&k, &format!("unreadable:{INSTANCE}:0")).await;
    assert_eq!(alerts.len(), 1);
    assert!(
        alerts[0].detail.contains("no history endpoint"),
        "{}",
        alerts[0].detail
    );
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 0);
    assert!(core_seqs(&k).is_empty());
    k.chain.serve_history(true);
    let before = k.chain.reads();
    let r = k.step().await.unwrap();
    assert_eq!(r.observed, 2);
    let after = k.chain.reads();
    assert!(after.history > 0);
    // The client served what it keeps; the history endpoint the rest, each once.
    let fin = k.chain.finalized().await.unwrap();
    assert!(after.blocks - before.blocks <= 258, "{after:?}");
    assert!(after.history - before.history < fin.number);
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    assert_eq!(core_seqs(&k), vec![0, 1]);
    // Caught up: idle again, one read.
    k.chain.serve_history(false);
    k.chain.advance(1);
    let before = k.chain.reads();
    k.step().await.unwrap();
    assert_eq!(k.chain.reads().blocks - before.blocks, 0);
}

/// `T-043-04` (ruling a40-q3): when no reader serves the range, the operator has the adapter
/// record it as a gap through its own source; the adapter accepts the layer's head as it stands,
/// and observation goes on from there. A range other than the one it stopped at is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_04_the_operators_gap_goes_through_the_adapter() {
    let mut k = serving_kit().await;
    deposits(&k, 3);
    k.chain.advance(300);
    k.step().await.unwrap();
    assert_eq!(open_alerts(&k, "unreadable:").await.len(), 1);
    assert!(matches!(
        k.adapter.record_operator_gap(0, 2).await,
        Err(AdapterError::Refused(_))
    ));
    assert!(k.store.gaps().await.unwrap().is_empty());
    k.adapter.record_operator_gap(0, 3).await.unwrap();
    let gaps = k.store.gaps().await.unwrap();
    assert_eq!(
        (gaps[0].from, gaps[0].to, gaps[0].reason.as_str()),
        (0, 3, "unreadable")
    );
    let core = k.core_settlement(|s| s.gaps().to_vec()).unwrap();
    assert_eq!((core[0].from, core[0].to), (0, 3));
    assert!(open_alerts(&k, "unreadable:").await.is_empty());
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 3);
    assert_eq!(
        k.store
            .accepted_head(INSTANCE)
            .await
            .unwrap()
            .unwrap()
            .position,
        3
    );
    // From there on, as ever.
    deposits(&k, 1);
    k.chain.finalize_all();
    assert_eq!(k.step().await.unwrap().observed, 1);
    assert_eq!(core_seqs(&k), vec![3]);
}

// ------------------------------------------------------------------------------ resets

/// `T-043-04`: the adapter's first start on a reset instance folds from the reset: an entry of the
/// chain before it is never reported, the one in the reset's own block after it is, and a cursor
/// kept from before the reset is reset to the chain's start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_04_after_a_reset_no_entry_of_the_old_chain_is_reported() {
    let mut k = Kit::start_with(KitOptions::default()).await;
    let from = [0x77; 32];
    k.chain.mint(from, &chain_usd(), 1_000);
    // The chain before: three deposits; then, in one block, a deposit, the reset and a deposit.
    for _ in 0..3 {
        k.chain.deposit(from, INSTANCE, [0x41; 32], &chain_usd(), 1);
        k.chain.produce();
    }
    k.chain.deposit(from, INSTANCE, [0x41; 32], &chain_usd(), 1);
    k.chain.reset(INSTANCE, 0);
    k.chain.deposit(from, INSTANCE, [0x42; 32], &chain_usd(), 2);
    k.chain.produce();
    k.chain.deposit(from, INSTANCE, [0x42; 32], &chain_usd(), 3);
    k.chain.advance(1);
    let h = head(&k).await;
    assert_eq!((h.position, h.start), (2, 0));
    // A cursor kept from the chain before, as test's at 13.
    k.store.advance_cursor(INSTANCE, 13).await.unwrap();
    let r = k.step().await.unwrap();
    assert!(r.serving);
    assert_eq!(r.observed, 2);
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 2);
    let amounts: Vec<u64> = k
        .core_settlement(|s| {
            s.all_ingested()
                .filter_map(|i| i.observation.movement.as_ref().map(|m| m.amount.get()))
                .collect()
        })
        .unwrap();
    assert_eq!(amounts, vec![2, 3]);
    assert_eq!(
        k.store
            .accepted_head(INSTANCE)
            .await
            .unwrap()
            .unwrap()
            .position,
        2
    );
}

/// `T-043-04`: a reset under a running adapter is its book's re-founding: an alert, and nothing of
/// the new chain is reported to the book it serves, until the adapter is reset with the
/// re-founding (`refound`), after which it starts on the new chain from its start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_04_a_reset_under_a_running_adapter_stops_it_until_the_re_founding() {
    let mut k = serving_kit().await;
    deposits(&k, 2);
    k.chain.finalize_all();
    k.step().await.unwrap();
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 2);
    k.chain.reset(INSTANCE, 0);
    k.chain.produce();
    deposits(&k, 1);
    k.chain.finalize_all();
    assert_eq!(k.step().await.unwrap().observed, 0);
    assert_eq!(open_alerts(&k, &format!("reset:{INSTANCE}")).await.len(), 1);
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 2);
    k.adapter.refound().await.unwrap();
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 0);
    assert_eq!(k.store.accepted_head(INSTANCE).await.unwrap(), None);
    assert!(open_alerts(&k, "reset:").await.is_empty());
    k.step().await.unwrap();
    // The new chain read from its start (here the core is the same one, which keeps what it took
    // in under the chain before: in a re-founding it is the new book's).
    let h = head(&k).await;
    assert_eq!(
        k.store
            .accepted_head(INSTANCE)
            .await
            .unwrap()
            .map(|a| a.position),
        Some(h.position)
    );
}

// ------------------------------------------------------------------------------ restarts

/// `T-043-04`: the head is accepted only once the core has taken in every entry up to it; after a
/// crash in between, the next sync folds from the accepted head again and reports only what the
/// cursor has not passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_04_a_head_is_accepted_only_after_the_core_took_in_every_entry() {
    let mut k = serving_kit().await;
    let accepted = k.store.accepted_head(INSTANCE).await.unwrap();
    deposits(&k, 3);
    k.chain.finalize_all();
    k.adapter.crash_at(CrashPoint::AfterVerdict);
    assert!(k.step().await.is_err());
    assert_eq!(k.store.accepted_head(INSTANCE).await.unwrap(), accepted);
    k.restart();
    let r = k.step().await.unwrap();
    assert_eq!(r.observed, 3);
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 3);
    assert_eq!(
        k.store
            .accepted_head(INSTANCE)
            .await
            .unwrap()
            .unwrap()
            .position,
        3
    );
    // A crash after the second entry's cursor moved: only the third is reported again.
    deposits(&k, 2);
    k.chain.finalize_all();
    k.adapter.crash_at(CrashPoint::AfterObserve);
    assert!(k.step().await.is_err());
    k.restart();
    assert_eq!(k.step().await.unwrap().observed, 2);
    assert_eq!(core_seqs(&k), vec![0, 1, 2, 3, 4]);
}

/// `T-043-05`: no code path re-reads a range on a timer. Many idle steps read nothing but the
/// head; a configuration that still names the re-scan is accepted and the fields are retired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_043_05_nothing_re_reads_a_range_on_a_timer() {
    let mut k = serving_kit().await;
    deposits(&k, 2);
    k.chain.finalize_all();
    k.step().await.unwrap();
    k.reconfigure(|c| {
        c.rescan_every = Some(1);
        c.rescan_window = Some(1_024);
    });
    assert_eq!(
        k.adapter.config().retired(),
        vec!["rescan_every", "rescan_window"]
    );
    let before = k.chain.reads();
    for _ in 0..20 {
        k.chain.advance(1);
        k.step().await.unwrap();
    }
    let after = k.chain.reads();
    assert_eq!(
        (after.heads - before.heads, after.blocks - before.blocks),
        (20, 0)
    );
    let rescans = k
        .store
        .reports()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "rescan")
        .count();
    assert_eq!(rescans, 0);
}
