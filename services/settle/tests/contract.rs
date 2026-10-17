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
use settle::adapter::{CrashPoint, Refusing};
use settle::chain::fake::FakeConfig;
use settle::chain::fake::position_account;
use settle::chain::kreivo::{self, Envelope};
use settle::chain::{Chain, InboxEvent, InboxRead, Validity, blake2_256};
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

/// A kit on the fake layer running Kreivo spec `spec_version` in `format`.
fn on_runtime(spec_version: u32, format: &kreivo::TxFormat) -> KitOptions {
    KitOptions {
        fake: FakeConfig::running(spec_version, format),
        ..KitOptions::default()
    }
}

/// The formats whose device message the extrinsic's signature verifies in (`T-027-12`: a
/// format, not a `spec_version`; spec 134 signs in `kreivo-133`).
fn signed_in(e: &ExtrinsicRow) -> Vec<&'static str> {
    let (tx, _, _) = envelopes(e);
    let c = &tx.credential;
    kreivo::FORMATS
        .iter()
        .filter(|f| {
            let digest = blake2_256(&kreivo::signed_message(f, c.context, &c.challenge));
            recover(&digest, &c.signature).is_some_and(|p| blake2_256(&p) == c.device_id)
        })
        .map(|f| f.name)
        .collect()
}

/// `T-027-07`, `REQ-SA-2`: on the runtime live on Kreivo mainnet (spec 132, release `0.16.9`),
/// the device signs `context ‖ authority ‖ challenge`, the order its authenticator verifies, and
/// the batch settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_07_on_spec_132_the_device_signs_in_that_runtimes_order() {
    let mut k = serving_kit(on_runtime(132, &kreivo::FORMAT_132)).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "old").await;
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    assert_eq!(signed_in(&extrinsic(&k, &op).await), vec!["kreivo-132"]);
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 10);
}

/// `T-027-07`, `REQ-SA-2`: a runtime upgrade from spec 132 to spec 133 seen mid-run switches the
/// format for the next batch; the adapter keeps serving, and each batch settles in its runtime's
/// order. Since `T-027-12` the switch follows the surface (133's `transaction_version`,
/// extensions and types), and the layer confirms the new format before the batch goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_07_a_runtime_upgrade_seen_mid_run_switches_the_format_for_the_next_batch() {
    let mut k = serving_kit(on_runtime(132, &kreivo::FORMAT_132)).await;
    let alice = k.d.party("alice", 1_000).await;
    let before = k.withdrawal(&alice, 10, "before").await;
    until_all(&mut k, &alice, std::slice::from_ref(&before), "Settled", 5).await;
    assert_eq!(signed_in(&extrinsic(&k, &before).await), vec!["kreivo-132"]);
    // Kreivo upgrades to the release after mainnet's (spec 133).
    k.chain.upgrade_to(133, &kreivo::FORMAT_133);
    k.chain.advance(1);
    assert!(k.step().await.unwrap().serving, "{:?}", k.adapter.status());
    let after = k.withdrawal(&alice, 20, "after").await;
    until_all(&mut k, &alice, std::slice::from_ref(&after), "Settled", 5).await;
    let e = extrinsic(&k, &after).await;
    assert_eq!(signed_in(&e), vec!["kreivo-133"]);
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
    let mut k = serving_kit(on_runtime(132, &kreivo::FORMAT_132)).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "in-flight").await;
    k.chain.drop_submissions(true);
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (1, 1));
    assert_eq!(signed_in(&extrinsic(&k, &op).await), vec!["kreivo-132"]);
    k.chain.upgrade_to(133, &kreivo::FORMAT_133);
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
    assert_eq!(signed_in(&extrinsic(&k, &next).await), vec!["kreivo-133"]);
}

// ------------------------------------------------------------------------------ T-027-12

/// The runtimes the store records as served: `(spec_version, format)`.
async fn served(k: &Kit) -> Vec<(u32, String)> {
    k.store
        .runtimes()
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.spec_version, r.format))
        .collect()
}

async fn alert_open(k: &Kit, key: &str) -> bool {
    k.store
        .alerts()
        .await
        .unwrap()
        .iter()
        .any(|a| a.key == key && a.cleared_at.is_none())
}

/// `AC-E4.3`, `T-027-12`, `REQ-SA-9`: Kreivo 133 to 134 (release `0.17.1`, 2026-10-14) changed the
/// `spec_version` and nothing a transaction carries. The adapter keeps serving with no
/// configuration change and no format entry for 134; the next batch is confirmed by the layer on
/// the new runtime, then broadcast, and settles. A runtime served once is never asked again, not
/// even after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_3_t_027_12_an_upgrade_that_changes_nothing_a_transaction_carries_keeps_serving() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let first = k.withdrawal(&alice, 10, "on-133").await;
    until_all(&mut k, &alice, std::slice::from_ref(&first), "Settled", 5).await;
    assert_eq!(
        k.chain.validations(),
        1,
        "the first batch on 133 was confirmed"
    );
    assert_eq!(served(&k).await, vec![(133, "kreivo-133".to_owned())]);
    // Kreivo 0.17.1: spec 134, its surface unchanged. Nothing in the configuration names it.
    let c = k.adapter.config();
    assert!(c.spec_versions.is_empty() && c.formats.is_empty());
    assert!(kreivo::FORMATS.iter().all(|f| f.name != "kreivo-134"));
    k.chain.set_spec_version(134);
    k.chain.advance(1);
    assert!(k.step().await.unwrap().serving, "{:?}", k.adapter.status());
    let second = k.withdrawal(&alice, 20, "on-134").await;
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (1, 1));
    assert_eq!(
        k.chain.validations(),
        2,
        "confirmed on 134 before it went out"
    );
    until_all(&mut k, &alice, std::slice::from_ref(&second), "Settled", 5).await;
    assert_eq!(signed_in(&extrinsic(&k, &second).await), vec!["kreivo-133"]);
    assert_eq!(
        served(&k).await,
        vec![
            (133, "kreivo-133".to_owned()),
            (134, "kreivo-133".to_owned())
        ]
    );
    // Served: neither the next batch nor a restarted adapter asks again.
    k.restart();
    let third = k.withdrawal(&alice, 30, "after-restart").await;
    until_all(&mut k, &alice, std::slice::from_ref(&third), "Settled", 5).await;
    assert_eq!(k.chain.validations(), 2);
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 60);
}

/// `AC-E4.4`, `T-027-12`, `REQ-SA-9`: on a runtime whose surface is unchanged, a transaction the
/// layer finds invalid for what it carries (here `BadProof`) is never stored or broadcast; the
/// adapter refuses to serve on that runtime, raises an alert naming it, and signs nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_4_t_027_12_a_transaction_the_layer_does_not_confirm_stops_service() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    k.chain.set_spec_version(134);
    k.chain
        .answer_validations(Some(Validity::Refused("Invalid(BadProof)".into())));
    k.chain.advance(1);
    assert!(
        k.step().await.unwrap().serving,
        "a known surface, not yet confirmed"
    );
    let op = k.withdrawal(&alice, 10, "refused").await;
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (0, 0));
    assert_eq!(k.chain.validations(), 1);
    assert!(
        k.store.extrinsics().await.unwrap().is_empty(),
        "never stored"
    );
    assert_eq!(k.chain.pool_len(), 0, "never broadcast");
    let refusing = [Refusing::Unconfirmed {
        spec_version: 134,
        format: "kreivo-133".into(),
        error: "Invalid(BadProof)".into(),
    }];
    assert_eq!(k.adapter.status(), (false, &refusing[..]));
    assert!(alert_open(&k, "runtime:134").await);
    // Declared to the core: outbound operations are refused; the call waits, unsigned.
    k.chain.advance(1);
    assert!(!k.step().await.unwrap().serving);
    assert_eq!(k.adapter.status().1, &refusing[..]);
    let (s, v) = k.withdraw(&alice, 5, "after").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (422, Some("ERR-SettlementLayerUnavailable")),
        "{v}"
    );
    assert_eq!(
        instruction(&k, &op).await.state,
        InstructionState::Acknowledged
    );
    assert_eq!(k.chain.validations(), 1, "nothing more was signed");
    assert!(served(&k).await.is_empty());
}

/// `AC-E4.4`, `T-027-12`: the case the surface cannot see. A runtime whose metadata is spec 133's
/// to the byte but whose authenticator verifies the device message in the other order (as Kreivo
/// `cf69b67`, spec 133 with the old order, would: `F-027` NOTES, decision 23). The surface selects
/// `kreivo-133`; the layer itself refuses the signature, and nothing goes out. The operator then
/// names `kreivo-132` for that `spec_version`: the adapter serves again, and broadcasts only once
/// the layer confirms that format.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_4_t_027_12_an_order_flipped_under_an_unchanged_surface_is_caught_by_the_layer() {
    let mut k = serving_kit(KitOptions {
        fake: FakeConfig {
            spec_version: 135,
            order: kreivo::MessageOrder::AuthorityFirst,
            ..FakeConfig::default()
        },
        ..KitOptions::default()
    })
    .await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "flipped").await;
    k.step().await.unwrap();
    let (serving, why) = k.adapter.status();
    assert!(!serving);
    assert!(
        matches!(
            why,
            [Refusing::Unconfirmed { spec_version: 135, format, error }]
                if format == "kreivo-133" && error.contains("BadProof")
        ),
        "{why:?}"
    );
    assert!(k.store.extrinsics().await.unwrap().is_empty());
    assert_eq!(k.chain.pool_len(), 0);
    // The operator names spec 135's format (amendment 0031: still confirmed by the layer).
    k.reconfigure(|c| {
        c.formats.insert(135, "kreivo-132".into());
    });
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    assert_eq!(signed_in(&extrinsic(&k, &op).await), vec!["kreivo-132"]);
    assert_eq!(k.chain.validations(), 2);
    assert_eq!(served(&k).await, vec![(135, "kreivo-132".to_owned())]);
}

/// `AC-E4.4`, `T-027-12`, `REQ-SA-9`, `F-027` NOTES decision 51: a validity error that says
/// nothing of the format (a nonce, fees, the era, the block's resources) neither confirms the
/// runtime nor refuses it, and **nothing goes out on a runtime the layer has not confirmed**: the
/// batch is neither stored nor broadcast, no nonce moves, and its calls stay acknowledged and
/// unsigned. Each step rebuilds the transaction and asks again; once the layer confirms, it goes
/// out, and later transactions on the runtime are not validated again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_4_t_027_12_a_transient_validity_error_broadcasts_nothing_until_confirmed() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    k.chain
        .answer_validations(Some(Validity::NotNow("Invalid(Future)".into())));
    let op = k.withdrawal(&alice, 10, "transient").await;
    let lane = hex::encode(k.lanes[0].account());
    for asked in 1..=2 {
        let r = k.step().await.unwrap();
        assert_eq!(
            (r.signed, r.broadcast),
            (0, 0),
            "nothing goes out before the layer confirms"
        );
        assert_eq!(k.chain.validations(), asked, "rebuilt and asked again");
        assert!(k.adapter.status().0, "{:?}", k.adapter.status());
        assert!(!alert_open(&k, "runtime:133").await, "not a refusal");
        assert!(served(&k).await.is_empty(), "not confirmed");
        assert!(
            k.store.extrinsics().await.unwrap().is_empty(),
            "never stored"
        );
        assert_eq!(k.chain.pool_len(), 0, "never broadcast");
        assert_eq!(
            k.store.lane(&lane).await.unwrap().map(|l| l.next_nonce),
            None,
            "no nonce moved"
        );
        assert_eq!(
            instruction(&k, &op).await.state,
            InstructionState::Acknowledged
        );
        k.chain.advance(1);
    }
    k.chain.answer_validations(None);
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    assert_eq!(k.chain.validations(), 3, "validated again, and confirmed");
    assert_eq!(served(&k).await, vec![(133, "kreivo-133".to_owned())]);
    // Confirmed: the next transaction on the runtime goes out unvalidated, as before.
    let next = k.withdrawal(&alice, 20, "next").await;
    until_all(&mut k, &alice, std::slice::from_ref(&next), "Settled", 5).await;
    assert_eq!(k.chain.validations(), 3);
}

/// `AC-E4.4`, `T-027-12`, `REQ-SA-9`, `F-027` NOTES decision 52: "not now" answers that persist
/// raise the runtime's alert, which pages. `not_now_alert_after` − 1 answers in a row raise
/// nothing; the next one raises `runtime:<spec_version>`, naming the last answer; the runtime's
/// confirmation clears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_4_t_027_12_repeated_not_now_answers_raise_the_runtime_alert_until_confirmed() {
    let mut k = serving_kit(KitOptions::default()).await;
    let after = k.adapter.config().not_now_alert_after as usize;
    assert_eq!(after, 10, "the default");
    let alice = k.d.party("alice", 1_000).await;
    k.chain
        .answer_validations(Some(Validity::NotNow("Invalid(Stale)".into())));
    let op = k.withdrawal(&alice, 10, "held").await;
    for asked in 1..after {
        k.step().await.unwrap();
        assert_eq!(k.chain.validations(), asked, "one answer a step");
        assert!(
            !alert_open(&k, "runtime:133").await,
            "{asked} answers raise nothing"
        );
        k.chain.advance(1);
    }
    k.chain
        .answer_validations(Some(Validity::NotNow("Invalid(Payment)".into())));
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (0, 0), "still nothing goes out");
    assert_eq!(k.chain.validations(), after);
    let alerts = k.store.alerts().await.unwrap();
    let raised = alerts
        .iter()
        .find(|a| a.key == "runtime:133" && a.cleared_at.is_none())
        .expect("raised at the threshold");
    assert!(
        raised.detail.contains("Invalid(Payment)") && raised.detail.contains("10 times"),
        "names the last answer: {}",
        raised.detail
    );
    assert!(
        k.adapter.status().0,
        "an alert, not a refusal: {:?}",
        k.adapter.status()
    );
    assert!(served(&k).await.is_empty(), "not confirmed");
    // The layer confirms: the withdrawal goes out and the alert clears.
    k.chain.advance(1);
    k.chain.answer_validations(None);
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    assert_eq!(served(&k).await, vec![(133, "kreivo-133".to_owned())]);
    assert!(
        !alert_open(&k, "runtime:133").await,
        "cleared by the confirmation"
    );
}

/// `F-027` NOTES decision 52: a runtime held by "not now" answers and then upgraded away from is
/// never confirmed itself; the next runtime's confirmation clears its alert, since nothing waits on
/// it any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_12_a_runtime_upgraded_away_from_has_its_not_now_alert_cleared() {
    let mut k = serving_kit(KitOptions::default()).await;
    let after = k.adapter.config().not_now_alert_after as usize;
    let alice = k.d.party("alice", 1_000).await;
    k.chain
        .answer_validations(Some(Validity::NotNow("Invalid(Future)".into())));
    let op = k.withdrawal(&alice, 10, "upgraded").await;
    for _ in 0..after {
        k.step().await.unwrap();
        k.chain.advance(1);
    }
    assert!(alert_open(&k, "runtime:133").await);
    k.chain.set_spec_version(134);
    k.chain.answer_validations(None);
    k.chain.advance(1);
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    assert_eq!(served(&k).await, vec![(134, "kreivo-133".to_owned())]);
    assert!(!alert_open(&k, "runtime:133").await);
    assert!(!alert_open(&k, "runtime:134").await);
}

/// `F-027` NOTES decision 52: a confirmation stored before its alert was cleared (a store error or
/// a crash in between) leaves `runtime:<spec_version>` open; every later step finds the runtime
/// confirmed and returns early. The next process clears it once, on its first transaction for the
/// runtime, instead of paging until another runtime is confirmed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_12_an_alert_left_open_by_a_stored_confirmation_is_cleared() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let first = k.withdrawal(&alice, 10, "confirms").await;
    until_all(&mut k, &alice, std::slice::from_ref(&first), "Settled", 5).await;
    assert_eq!(served(&k).await, vec![(133, "kreivo-133".to_owned())]);
    // The crash between storing the confirmation and clearing the alert, recreated.
    let now = k.now();
    assert!(
        k.store
            .raise("runtime:133", "left open", now)
            .await
            .unwrap()
    );
    k.restart();
    let second = k.withdrawal(&alice, 10, "after the restart").await;
    until_all(&mut k, &alice, std::slice::from_ref(&second), "Settled", 5).await;
    assert!(
        !alert_open(&k, "runtime:133").await,
        "cleared on the next transaction for the confirmed runtime"
    );
}

/// `AC-E4.3`, `T-027-12`, `REQ-SA-9`, `F-027` NOTES decision 51: Kreivo upgrades (133 to 134, its
/// surface unchanged) in a block produced after the step read its best head and before it read the
/// runtime it signs for. The batch is signed for 134 and validated at the block 134 was read at,
/// by 134: never by 133 at the head the step started from, which would refuse it (`BadProof`: the
/// `spec_version` is in its challenge) and stop the adapter with `Unconfirmed` until a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_3_t_027_12_an_upgrade_between_a_steps_start_and_its_signing_is_not_refused() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "mid-step").await;
    let before = k.chain.best_number();
    // The step's assertions read the runtime first (still 133), then the step reads its heads;
    // the upgrade lands before the next read, the one the batch is signed for.
    k.chain.before_next_runtime_read(|c| {
        c.before_next_runtime_read(|c| {
            c.produce();
            c.set_spec_version(134);
        });
    });
    let r = k.step().await.unwrap();
    assert_eq!(r.best, before, "the step started on 133's head");
    assert_eq!(
        k.chain.best_number(),
        before + 1,
        "134 was enacted mid-step"
    );
    let (serving, why) = k.adapter.status();
    assert!(serving, "{why:?}");
    assert!(
        !why.iter()
            .any(|w| matches!(w, Refusing::Unconfirmed { .. })),
        "{why:?}"
    );
    assert!(!alert_open(&k, "runtime:134").await);
    // Confirmed by 134 and broadcast at once.
    assert_eq!((r.signed, r.broadcast), (1, 1));
    assert_eq!(k.chain.validations(), 1);
    assert_eq!(served(&k).await, vec![(134, "kreivo-133".to_owned())]);
    // The next steps carry it to settlement, and keep serving.
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    assert!(k.adapter.status().0);
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 10);
}

/// `AC-E4.4`, `T-027-12`, `F-027` NOTES decision 51: before a refusal is taken as one of the
/// format, the runtime at the block the transaction was validated at must be the one it was signed
/// for. Here the layer's runtime at that block changes after the signing (a layer whose answer for
/// the block is not what it reported when the runtime was read): the transaction, signed for 133,
/// is refused by 134. Neither confirmed nor refused: nothing goes out, the adapter keeps serving
/// with no alert, and the next step signs for 134, which the layer confirms, and broadcasts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_4_t_027_12_a_refusal_by_another_runtime_than_the_one_signed_for_refuses_nothing() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "validated-by-134").await;
    k.chain.before_next_validation(|c| c.set_spec_version(134));
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (0, 0));
    assert_eq!(k.chain.validations(), 1);
    let (serving, why) = k.adapter.status();
    assert!(serving && why.is_empty(), "{why:?}");
    assert!(!alert_open(&k, "runtime:133").await);
    assert!(!alert_open(&k, "runtime:134").await);
    assert!(served(&k).await.is_empty());
    assert!(k.store.extrinsics().await.unwrap().is_empty());
    assert_eq!(k.chain.pool_len(), 0);
    assert_eq!(
        instruction(&k, &op).await.state,
        InstructionState::Acknowledged
    );
    // The next step signs for 134, which the layer confirms, and broadcasts.
    k.chain.advance(1);
    let r = k.step().await.unwrap();
    assert_eq!((r.signed, r.broadcast), (1, 1));
    assert_eq!(k.chain.validations(), 2);
    assert_eq!(served(&k).await, vec![(134, "kreivo-133".to_owned())]);
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
}

/// `T-027-12`, `REQ-SA-9` ("an operator MAY name in configuration the format to use for a set of
/// rules; the layer's confirmation is still required"): a runtime whose types match no format
/// (as an extension's type gaining a variant would) is refused until the operator names one; then
/// it serves, but nothing is broadcast until the layer confirms the named format, and a layer that
/// refuses it stops service again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_12_a_runtime_the_operator_maps_is_served_only_after_the_layer_confirms_it() {
    let mut k = Kit::start_with(KitOptions {
        fake: FakeConfig {
            spec_version: 140,
            types: Some([7; 32]),
            ..FakeConfig::default()
        },
        ..KitOptions::default()
    })
    .await;
    assert!(!k.step().await.unwrap().serving);
    assert!(matches!(
        k.adapter.status().1,
        [Refusing::UnknownSurface {
            spec_version: 140,
            ..
        }]
    ));
    k.reconfigure(|c| {
        c.formats.insert(140, "kreivo-133".into());
    });
    // Named, but the layer refuses it: nothing goes out.
    k.chain
        .answer_validations(Some(Validity::Refused("Invalid(Custom(3))".into())));
    assert!(k.step().await.unwrap().serving);
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "mapped").await;
    k.step().await.unwrap();
    assert_eq!(
        k.adapter.status().1,
        &[Refusing::Unconfirmed {
            spec_version: 140,
            format: "kreivo-133".into(),
            error: "Invalid(Custom(3))".into(),
        }][..]
    );
    assert!(k.store.extrinsics().await.unwrap().is_empty());
    assert_eq!(k.chain.pool_len(), 0);
    // A restart asks again; the layer now confirms it.
    k.chain.answer_validations(None);
    k.restart();
    until_all(&mut k, &alice, std::slice::from_ref(&op), "Settled", 5).await;
    assert_eq!(k.chain.validations(), 2);
    assert_eq!(served(&k).await, vec![(140, "kreivo-133".to_owned())]);
    assert!(!alert_open(&k, "runtime:140").await);
}
