// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-027` end to end against the fake settlement layer running the settlement contract: the call
//! outbox, batches of calls in one `execute` transaction signed as the deployment's community PASS
//! account, one outcome per reference read from the contract's inbox at finality, and the
//! guarantees `F-016` carries over (store before broadcast, the signing recorded with the core
//! first, the same bytes after a restart, release only on both proofs). Tests are named after the
//! task, requirement or invariant they verify.

use ingress::testkit::Party;
use ledger_core::id::OperationId;
use ledger_core::settlement::envelope::{anchor_reference, operation_reference};
use ledger_core::settlement::{Incident, NonceFact, SigningRecord};
use parity_scale_codec::Decode;
use serde_json::Value;
use settle::adapter::CrashPoint;
use settle::chain::fake::FakeConfig;
use settle::chain::fake::position_account;
use settle::chain::kreivo::{self, Envelope};
use settle::chain::{Chain, InboxEvent, InboxRead, blake2_256};
use settle::signer::recover;
use settle::store::{ExtrinsicRow, ExtrinsicStatus, InstructionState, NotExecuted, Store};
use settle::testkit::{ADAPTER_TOKEN, INSTANCE, Kit, KitOptions, PAYEE, TEST_PASS_USER, chain_usd};

async fn serving_kit(options: KitOptions) -> Kit {
    let mut k = Kit::start_with(options).await;
    let r = k.step().await.unwrap();
    assert!(r.serving, "{:?}", k.adapter.status());
    k
}

/// Run rounds until every operation of `ops` shows `want`, at most `n`.
async fn until_all(k: &mut Kit, p: &Party, ops: &[String], want: &str, n: usize) {
    for _ in 0..n {
        let mut all = true;
        for op in ops {
            all &= k.state(p, op).await == want;
        }
        if all {
            return;
        }
        k.chain.advance(1);
        k.step().await.unwrap_or_else(|e| panic!("a step: {e}"));
    }
    for op in ops {
        assert_eq!(k.state(p, op).await, want, "{op}");
    }
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

async fn extrinsic(k: &Kit, op: &str) -> ExtrinsicRow {
    k.store
        .extrinsics()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.operation == op)
        .unwrap_or_else(|| panic!("no extrinsic for {op}"))
}

/// The envelopes a stored transaction carries, decoded from its bytes as Kreivo decodes them.
fn envelopes(e: &ExtrinsicRow) -> (kreivo::DecodedTx, [u8; 32], Vec<Envelope>) {
    let tx = kreivo::decode_extrinsic(&e.extrinsic.bytes).expect("a v5 general PASS transaction");
    let (contract, value, _, data) =
        kreivo::decode_contracts_call(&tx.call).expect("a Contracts::call");
    assert_eq!(value, 0);
    let mut args = data
        .strip_prefix(&kreivo::selector::EXECUTE[..])
        .expect("the contract's execute");
    let calls = Vec::<Envelope>::decode(&mut args).expect("the envelopes");
    assert!(args.is_empty());
    (tx, contract, calls)
}

/// Every inbox entry of the deployment at the finalized head.
async fn inbox(k: &Kit) -> Vec<(u64, settle::chain::InboxEntry)> {
    let fin = k.chain.finalized().await.unwrap();
    k.chain
        .inbox(&fin, INSTANCE, 0, 1_000)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|(s, r)| match r {
            InboxRead::Entry(e) => Some((s, e)),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------------------ T-027-01

/// `T-027-01`, `REQ-SO-5`, `DEC-18`: every external movement and anchor leaves the core as an
/// envelope — reference, target, call, expiry — in one call outbox, which only the adapter's
/// principal reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_01_every_external_movement_and_anchor_leaves_the_core_as_an_envelope() {
    // Serving, then no step: the calls stay in the outbox, unacknowledged.
    let k = serving_kit(KitOptions {
        anchoring: true,
        ..KitOptions::default()
    })
    .await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 25, "w").await;
    k.pass(ingress::testkit::ANCHOR_INTERVAL_MS).await;
    let (s, v) =
        k.d.call(
            "GET",
            "/v1/settlement/calls",
            Some(ADAPTER_TOKEN),
            &[],
            None,
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let calls = v.as_array().expect("a list of calls");
    let id = OperationId::parse(&op).unwrap();
    let w = calls
        .iter()
        .find(|c| c["call"].get("withdraw").is_some())
        .expect("the withdrawal's envelope");
    assert_eq!(w["reference"], hex::encode(operation_reference(&id)));
    assert_eq!(w["target"], INSTANCE);
    assert_eq!(w["call"]["withdraw"]["amount"], "25");
    assert_eq!(
        w["call"]["withdraw"]["destination"]["account"],
        hex::encode(PAYEE)
    );
    assert!(w["expiry"].as_u64().is_some(), "a withdrawal expires");
    assert_eq!(w["reserved"], true, "INV-13");
    assert_eq!(w["acknowledged"], false);
    // The instruction's identity is the one the core acknowledges it under.
    let digest = k
        .core_settlement(|s| s.instruction(&id).map(|i| i.instruction.digest()))
        .flatten()
        .expect("the instruction");
    assert_eq!(w["instruction"], hex::encode(digest));
    // The anchor: the same form, its own reference, no expiry.
    let a = calls
        .iter()
        .find(|c| c["call"].get("anchor").is_some())
        .expect("the anchor's envelope");
    assert_eq!(a["call"]["anchor"]["number"], 1);
    assert_eq!(a["reference"], hex::encode(anchor_reference(INSTANCE, 1)));
    assert_eq!(a["target"], INSTANCE);
    assert!(a.get("expiry").is_none() || a["expiry"].is_null());
    let keys: Vec<&String> = a["call"]["anchor"]
        .as_object()
        .map(|o| o.keys().collect())
        .unwrap_or_default();
    assert_eq!(
        keys,
        ["commitment", "number"],
        "INV-17: a number and 32 bytes"
    );
    // Nobody else reads it.
    let (s, v) =
        k.d.call("GET", "/v1/settlement/calls", Some(&alice.token), &[], None)
            .await;
    assert_eq!(s.as_u16(), 403, "{v}");
}

// ------------------------------------------------------------------------------ T-027-02

/// `T-027-02`, `REQ-SA-6`, `REQ-SL-4`: the calls ready in one step go out in one `execute`
/// transaction — one nonce, one era, one signed body — and each gets its own outcome in the
/// contract's inbox, observed and settled by its reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_02_the_calls_ready_in_one_step_go_out_in_one_batch_with_one_outcome_each() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 10_000).await;
    let mut ops = Vec::new();
    for (i, amount) in [10u64, 20, 30].iter().enumerate() {
        ops.push(k.withdrawal(&alice, *amount, &format!("b{i}")).await);
    }
    let r = k.step().await.unwrap();
    assert_eq!((r.acknowledged, r.signed, r.broadcast), (3, 3, 1));
    let rows: Vec<ExtrinsicRow> = k.store.extrinsics().await.unwrap();
    assert_eq!(rows.len(), 3, "one row per operation");
    let hash = rows[0].extrinsic.hash;
    let batch = rows[0].batch.clone();
    assert!(rows.iter().all(|e| e.extrinsic.hash == hash
        && e.nonce == rows[0].nonce
        && e.era == rows[0].era
        && e.batch == batch));
    let mut sorted = batch.clone();
    sorted.sort();
    let mut all = ops.clone();
    all.sort();
    assert_eq!(sorted, all, "the batch carries every operation once");
    assert_eq!(k.chain.calls_in(&hash), Some(3));
    // The transaction carries the three envelopes, by reference, to the deployment's contract,
    // in the batch's order.
    let (_, contract, calls) = envelopes(&rows[0]);
    assert_eq!(contract, position_account(INSTANCE));
    let refs: Vec<[u8; 32]> = calls.iter().map(|c| c.operation_ref).collect();
    let expected: Vec<[u8; 32]> = batch.iter().map(|o| Kit::chain_ref(o)).collect();
    assert_eq!(refs, expected);
    // Every operation was recorded with the core before the broadcast, with the batch's nonce.
    for op in &ops {
        let id = OperationId::parse(op).unwrap();
        let signing = k
            .core_settlement(|s| s.instruction(&id).and_then(|i| i.signing))
            .flatten();
        assert!(
            matches!(signing, Some(SigningRecord::Signed(s)) if s.nonce == rows[0].nonce
                && s.last_valid == rows[0].era.last_valid()),
            "{signing:?}"
        );
    }
    until_all(&mut k, &alice, &ops, "Settled", 5).await;
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 60);
    // One outcome per reference, all written by the one transaction.
    let entries = inbox(&k).await;
    let withdrawn: Vec<[u8; 32]> = entries
        .iter()
        .filter_map(|(_, e)| match e.event {
            InboxEvent::Withdrawn { operation_ref, .. } => Some(operation_ref),
            _ => None,
        })
        .collect();
    assert_eq!(withdrawn, expected);
    assert!(entries.windows(2).all(|w| w[0].1.at == w[1].1.at));
    for op in &ops {
        assert_eq!(instruction(&k, op).await.state, InstructionState::Executed);
        assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(op)), 1);
    }
    assert_eq!(k.balances(&alice).await, (9_940, 0));
}

/// A withdrawal of `amount` by `party` to the plain account `to`.
async fn withdrawal_to(k: &Kit, party: &Party, amount: u64, key: &str, to: [u8; 32]) -> String {
    let (s, v) = k.withdraw(party, amount, key).await;
    assert_eq!(s, 200, "{v}");
    let op = v["operation_reference"].as_str().unwrap().to_owned();
    let (s, v) =
        k.d.call(
            "POST",
            &format!("/v1/withdrawals/{op}/submit"),
            Some(&party.token),
            &[],
            Some(serde_json::json!({ "destination": { "account": hex::encode(to) } })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    op
}

/// The refusal the inbox records for `op`: the reason and the amount.
async fn refusal_of(k: &Kit, op: &str) -> Option<(String, u128)> {
    inbox(k).await.into_iter().find_map(|(_, e)| match e.event {
        InboxEvent::WithdrawRefused {
            operation_ref,
            error,
            amount,
            ..
        } if operation_ref == Kit::chain_ref(op) => Some((error, amount)),
        _ => None,
    })
}

/// The payload the adapter reported for the refusal of `op`.
async fn refusal_payload(k: &Kit, op: &str) -> Value {
    k.store
        .reports()
        .await
        .unwrap()
        .into_iter()
        .find(|r| {
            r.kind == "observe"
                && r.body["kind"] == "withdraw_refused"
                && r.body["operation_reference"] == op
        })
        .expect("the refusal was reported")
        .body["payload"]
        .clone()
}

/// `T-027-02`, `REQ-SA-4`, `REQ-SA-3`: one call of a batch refused by the contract — here paid to
/// the deployment's own contract — is recorded under its reference with the contract's reason,
/// and the others apply. The refusal is observed with the layer's own words; final, with its
/// transaction's nonce consumed, it is a proved rejection and its reservation is released at once
/// (amendment 0006 item 3, `REQ-OP-9`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_02_req_sa_4_a_refused_call_inside_a_batch_is_recorded_and_the_rest_apply() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 10_000).await;
    let first = k.withdrawal(&alice, 10, "a").await;
    let refused = withdrawal_to(&k, &alice, 20, "b", position_account(INSTANCE)).await;
    let last = k.withdrawal(&alice, 5, "c").await;
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (3, 1), "one batch");
    assert_eq!(
        extrinsic(&k, &refused).await.extrinsic.hash,
        extrinsic(&k, &first).await.extrinsic.hash
    );
    until_all(&mut k, &alice, &[first.clone(), last.clone()], "Settled", 5).await;
    // The refused one: its own entry, with the contract's reason.
    assert_eq!(
        refusal_of(&k, &refused).await,
        Some(("SelfTransfer".to_owned(), 20))
    );
    assert_eq!(
        instruction(&k, &refused).await.state,
        InstructionState::Refused {
            error: "SelfTransfer".into()
        }
    );
    // Reported with the layer's words as the payload.
    assert_eq!(
        refusal_payload(&k, &refused).await,
        hex::encode("SelfTransfer")
    );
    // It moved nothing on the layer, and its reservation is released: a proved rejection.
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 15);
    until_all(
        &mut k,
        &alice,
        std::slice::from_ref(&refused),
        "Released",
        10,
    )
    .await;
    assert_eq!(k.balances(&alice).await, (9_985, 0));
}

/// `T-027-02`, `REQ-SA-3`, `F-026` decision 19: a withdrawal Kreivo's assets API refuses (the
/// contract account no longer holds the amount) is recorded with Kreivo's reason, and reported in
/// those words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_02_req_sa_3_a_refusal_carries_kreivos_reason() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 30, "k").await;
    k.step().await.unwrap();
    // Drained outside the contract after the batch was signed.
    k.chain.set_position(INSTANCE, &chain_usd(), 0);
    k.rounds(2).await.unwrap();
    assert_eq!(
        refusal_of(&k, &op).await,
        Some(("Kreivo(Assets(CannotTransfer))".to_owned(), 30))
    );
    assert_eq!(
        refusal_payload(&k, &op).await,
        hex::encode("Kreivo(Assets(CannotTransfer))")
    );
    // A proved rejection (amendment 0006 item 3): released once final with its nonce consumed.
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Released", 10).await;
    assert_eq!(k.balances(&alice).await, (1_000, 0), "released");
}

/// `T-027-02`, `REQ-SA-6`, `REQ-SO-7`: a crash after the batch is stored and before it is
/// broadcast; the restarted adapter records it and broadcasts the same bytes, never a second
/// transaction for any of its operations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_02_req_so_7_a_restart_replays_the_same_batch_bytes() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 10_000).await;
    let ops = vec![
        k.withdrawal(&alice, 10, "r1").await,
        k.withdrawal(&alice, 20, "r2").await,
    ];
    k.adapter.crash_at(CrashPoint::AfterPersist);
    assert!(k.step().await.is_err());
    let stored: Vec<ExtrinsicRow> = k.store.extrinsics().await.unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(|e| e.status == ExtrinsicStatus::Signed));
    assert_eq!(k.chain.pool_len(), 0, "nothing was broadcast");
    k.restart();
    let r = k.step().await.unwrap();
    assert_eq!(
        (r.signed, r.broadcast),
        (0, 1),
        "the stored batch, not a new one"
    );
    let again = k.store.extrinsics().await.unwrap();
    for e in &again {
        let before = stored.iter().find(|s| s.operation == e.operation).unwrap();
        assert_eq!(e.extrinsic, before.extrinsic, "the same bytes");
    }
    until_all(&mut k, &alice, &ops, "Settled", 5).await;
    for op in &ops {
        assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(op)), 1);
    }
    assert!(k.chain.duplicated_references().is_empty());
}

/// `T-027-02`, `INV-11`, the owner's ruling of 2026-09-24 (#89): a batch executes, but the
/// reader loses one entry's reference. That operation's range is complete and final with no
/// record naming it; its key's nonce shows the transaction was included: review, never release.
/// The other call of the batch settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_02_inv_11_a_reader_fault_inside_a_batch_goes_to_review() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let lost = k.withdrawal(&alice, 40, "lost").await;
    let read = k.withdrawal(&alice, 15, "read").await;
    k.chain.misread_reference_of(Kit::chain_ref(&lost));
    until_all(&mut k, &alice, std::slice::from_ref(&read), "Settled", 10).await;
    until_all(
        &mut k,
        &alice,
        std::slice::from_ref(&lost),
        "Needs-Review",
        80,
    )
    .await;
    // Both executed, in one transaction.
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 55);
    assert_eq!(
        extrinsic(&k, &lost).await.extrinsic.hash,
        extrinsic(&k, &read).await.extrinsic.hash
    );
    let id = OperationId::parse(&lost).unwrap();
    let lane = k.lanes[0].account();
    let (fact, incident) = k
        .core_settlement(|s| {
            let i = s.instruction(&id).unwrap();
            let incident = s.incidents().iter().any(|x| {
                matches!(x, Incident::IncludedWithoutRecord { op, key, .. }
                    if *op == id && *key == lane)
            });
            (i.nonce, incident)
        })
        .unwrap();
    assert!(matches!(fact, Some(NonceFact::Consumed { .. })), "{fact:?}");
    assert!(incident);
    k.rounds(10).await.unwrap();
    assert_eq!(k.state(&alice, &lost).await, "Needs-Review");
    assert_eq!(
        k.balances(&alice).await,
        (945, 40),
        "reserved, never released"
    );
}

/// `T-027-02`, `REQ-SL-4`, `REQ-OP-9`, the owner's ruling of 2026-09-24: a batch that never
/// reaches the layer. Its era ends at the finalized head with the PASS account's nonce unconsumed,
/// and the inbox shows no record of any of its calls: every one is released, on both proofs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_02_req_sl_4_a_batch_whose_era_ends_unincluded_releases_every_call() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let ops = vec![
        k.withdrawal(&alice, 10, "e1").await,
        k.withdrawal(&alice, 20, "e2").await,
    ];
    k.chain.drop_submissions(true);
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (2, 1));
    let signed = extrinsic(&k, &ops[0]).await;
    until_all(&mut k, &alice, &ops, "Released", 120).await;
    for op in &ops {
        let row = instruction(&k, op).await;
        assert!(
            matches!(
                row.state,
                InstructionState::NotExecuted {
                    proof: NotExecuted::EraEnded { .. }
                }
            ),
            "{row:?}"
        );
        let id = OperationId::parse(op).unwrap();
        let fact = k
            .core_settlement(|s| s.instruction(&id).and_then(|i| i.nonce))
            .flatten();
        assert!(
            matches!(fact, Some(NonceFact::Unconsumed { height }) if height >= signed.era.last_valid()),
            "{fact:?}"
        );
        assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(op)), 1);
    }
    let pass = k.lanes[0].account();
    let fin = k.chain.finalized().await.unwrap();
    assert_eq!(k.chain.account_nonce(&fin, &pass).await.unwrap(), 0);
    assert_eq!(k.balances(&alice).await, (1_000, 0));
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 0);
}

// ------------------------------------------------------------------------------ T-027-03

/// `T-027-03`, `NFR-8`: a batch is a v5 general transaction authorised by `PassAuthenticate`, as
/// the deployment's community PASS account. Its device signs; the PASS account's ordinary nonce
/// is what moves; the device's never does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_03_a_batch_is_sent_as_the_community_pass_account() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "p").await;
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    let e = extrinsic(&k, &op).await;
    let (tx, _, calls) = envelopes(&e);
    assert_eq!(calls.len(), 1);
    assert_eq!(tx.credential.user_id, TEST_PASS_USER);
    let lane = &k.lanes[0];
    assert_eq!(tx.credential.device_id, lane.device());
    assert_eq!(lane.account(), kreivo::pass_account(&TEST_PASS_USER));
    assert_eq!(u64::from(tx.nonce), e.nonce);
    // The challenge's context is a finalized block no later than the era's birth.
    assert!(u64::from(tx.credential.context) <= e.era.birth);
    let fin = k.chain.finalized().await.unwrap();
    assert_eq!(
        k.chain.account_nonce(&fin, &lane.account()).await.unwrap(),
        1
    );
    assert_eq!(
        k.chain.account_nonce(&fin, &lane.device()).await.unwrap(),
        0
    );
    // The key is never in what the adapter stored or reported.
    let stored = serde_json::to_string(&k.store.extrinsics().await.unwrap()).unwrap();
    assert!(!stored.contains(&hex::encode(settle::testkit::TEST_LANE_SECRET)));
    let reports: Vec<Value> = k
        .store
        .reports()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.body)
        .collect();
    assert!(
        !serde_json::to_string(&reports)
            .unwrap()
            .contains(&hex::encode(settle::testkit::TEST_LANE_SECRET))
    );
}

/// `T-027-03`: a transaction whose challenge does not bind its call is not a PASS account's:
/// the layer refuses it before it spends anything, and no nonce moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_03_a_tampered_pass_transaction_is_never_included() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "t").await;
    k.chain.drop_submissions(true);
    k.step().await.unwrap();
    k.chain.drop_submissions(false);
    let e = extrinsic(&k, &op).await;
    // The same transaction, one byte of its call changed.
    let mut bytes = e.extrinsic.bytes.clone();
    let n = bytes.len();
    bytes[n - 2] ^= 1;
    let tampered = settle::chain::SignedExtrinsic {
        hash: settle::chain::blake2_256(&bytes),
        bytes,
    };
    k.chain.submit(&tampered).await.unwrap();
    k.chain.advance(1);
    let fin = k.chain.finalized().await.unwrap();
    assert!(
        k.chain
            .find_extrinsic(&fin, 0, &tampered.hash)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        k.chain
            .account_nonce(&fin, &k.lanes[0].account())
            .await
            .unwrap(),
        0
    );
    // The genuine bytes still go in, from the adapter's store.
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
}

// ------------------------------------------------------------------------------ T-027-07

/// A kit on the fake layer running Kreivo spec `spec_version`.
fn on_runtime(spec_version: u32, transaction_version: u32) -> KitOptions {
    KitOptions {
        fake: FakeConfig {
            spec_version,
            transaction_version,
            ..FakeConfig::default()
        },
        ..KitOptions::default()
    }
}

/// The runtimes whose format the extrinsic's device signature verifies in: 133's signature
/// verifies as 134's too, since 134 signs exactly as 133 does.
fn signed_in(e: &ExtrinsicRow) -> Vec<u32> {
    let (tx, _, _) = envelopes(e);
    let c = &tx.credential;
    kreivo::FORMATS
        .iter()
        .filter(|f| {
            let digest = blake2_256(&kreivo::signed_message(f, c.context, &c.challenge));
            recover(&digest, &c.signature).is_some_and(|p| blake2_256(&p) == c.device_id)
        })
        .map(|f| f.spec_version)
        .collect()
}

/// `T-027-07`, `REQ-SA-2`: on the runtime live on Kreivo mainnet (spec 132, release `0.16.9`),
/// the device signs `context ‖ authority ‖ challenge`, the order its authenticator verifies, and
/// the batch settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_07_on_spec_132_the_device_signs_in_that_runtimes_order() {
    let mut k = serving_kit(on_runtime(132, 12)).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "old").await;
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    assert_eq!(signed_in(&extrinsic(&k, &op).await), vec![132]);
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 10);
}

/// `T-027-07`, `REQ-SA-2`: a runtime upgrade from spec 132 to spec 133 seen mid-run switches the
/// format for the next batch; the adapter keeps serving, and each batch settles in its runtime's
/// order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_07_a_runtime_upgrade_seen_mid_run_switches_the_format_for_the_next_batch() {
    let mut k = serving_kit(on_runtime(132, 12)).await;
    let alice = k.d.party("alice", 1_000).await;
    let before = k.withdrawal(&alice, 10, "before").await;
    until_all(&mut k, &alice, std::slice::from_ref(&before), "Settled", 5).await;
    assert_eq!(signed_in(&extrinsic(&k, &before).await), vec![132]);
    // Kreivo upgrades to the release after mainnet's (spec 133).
    k.chain.set_spec_version(133);
    k.chain.advance(1);
    assert!(k.step().await.unwrap().serving, "{:?}", k.adapter.status());
    let after = k.withdrawal(&alice, 20, "after").await;
    until_all(&mut k, &alice, std::slice::from_ref(&after), "Settled", 5).await;
    let e = extrinsic(&k, &after).await;
    assert_eq!(signed_in(&e), vec![133, 134]);
    assert_ne!(
        e.extrinsic.hash,
        extrinsic(&k, &before).await.extrinsic.hash
    );
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 30);
    assert_eq!(k.balances(&alice).await, (970, 0));
}

/// `T-027-07`, `REQ-SL-4`, `REQ-OP-9`: a batch signed for spec 132 and still in flight when the
/// runtime upgrades never verifies on spec 133 (its `spec_version` is in its challenge). It is
/// not re-signed: its era ends with the PASS account's nonce unconsumed and no inbox record, and
/// its call is released on both proofs; the next batch goes out in spec 133's format.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_07_a_batch_in_flight_across_the_upgrade_is_released_on_both_proofs() {
    let mut k = serving_kit(on_runtime(132, 12)).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "in-flight").await;
    k.chain.drop_submissions(true);
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (1, 1));
    assert_eq!(signed_in(&extrinsic(&k, &op).await), vec![132]);
    k.chain.set_spec_version(133);
    k.chain.drop_submissions(false);
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Released", 120).await;
    assert!(matches!(
        instruction(&k, &op).await.state,
        InstructionState::NotExecuted {
            proof: NotExecuted::EraEnded { .. }
        }
    ));
    assert_eq!(
        k.chain.extrinsics_for(&Kit::chain_ref(&op)),
        1,
        "never re-signed"
    );
    let fin = k.chain.finalized().await.unwrap();
    assert_eq!(
        k.chain
            .account_nonce(&fin, &k.lanes[0].account())
            .await
            .unwrap(),
        0
    );
    assert_eq!(k.balances(&alice).await, (1_000, 0));
    let next = k.withdrawal(&alice, 10, "next").await;
    until_all(&mut k, &alice, std::slice::from_ref(&next), "Settled", 5).await;
    assert_eq!(signed_in(&extrinsic(&k, &next).await), vec![133, 134]);
}
