// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Anchoring end to end (`F-017` T-017-08): ingress, the in-process core with its clerk key, the
//! adapter with its anchor key and its store, and the fake settlement layer running the settlement
//! contract's anchor calls (`F-027`), in one process (`settle::testkit`). Then a recipient checks a
//! disclosure with the verifier alone, from what the layer holds in the venue the anchor lives in,
//! once every aledgent process is stopped.
//!
//! Against a local Kreivo the same flow needs the binaries `features/F-016-settlement-adapter/
//! NOTES.md` lists; `crates/verify`'s ignored `reads_a_local_node` is the verifier's half of it.

use ingress::testkit::{ANCHOR_INTERVAL_MS, Party, USD};
use ledger_core::anchor::{AnchorStore, CheckpointStatus};
use serde_json::{Value, json};
use settle::store::{ExtrinsicStatus, Store};
use settle::testkit::{INSTANCE, Kit, KitOptions};

async fn anchoring_kit() -> Kit {
    let mut k = Kit::start_with(KitOptions {
        anchoring: true,
        ..KitOptions::default()
    })
    .await;
    let r = k.step().await.unwrap();
    assert!(r.serving, "{:?}", k.adapter.status());
    k
}

fn anchoring<T>(k: &Kit, f: impl FnOnce(&AnchorStore) -> T) -> T {
    k.d.core
        .with_ledger(|l| f(l.state().anchoring()))
        .expect("the core runs")
}

/// Rounds until checkpoint `number` is final and none is in flight, at most `n`.
async fn until_final(k: &mut Kit, number: u64, n: usize) {
    for _ in 0..n {
        if anchoring(k, |s| {
            s.in_flight().is_none() && s.latest_final().is_some_and(|c| c.number >= number)
        }) {
            return;
        }
        k.chain.advance(1);
        k.step().await.unwrap();
    }
    panic!(
        "checkpoint {number} is not final: {:?}",
        anchoring(k, |s| s.checkpoints().copied().collect::<Vec<_>>())
    );
}

async fn disclose(k: &Kit, who: &Party, body: Value) -> (u16, Value) {
    let (s, v) =
        k.d.call("POST", "/v1/disclosures", Some(&who.token), &[], Some(body))
            .await;
    (s.as_u16(), v)
}

/// What a recipient reads of the settlement layer at its finalized head: the anchors and the
/// key the deployment publishes, in its venue — the settlement contract (`F-027` T-027-04).
/// Nothing of aledgent's.
fn public_read(k: &Kit, numbers: &[u64]) -> verify::MemAnchors {
    let venue = settle::chain::Chain::venue(&k.chain, INSTANCE);
    let mut m = verify::MemAnchors::new();
    let read_at = k.chain.finalized_number();
    for n in numbers {
        if let Some((commitment, stored_at)) = k.chain.anchor_at_finalized(INSTANCE, *n) {
            m.put_in(
                venue,
                *n,
                verify::PublicAnchor {
                    commitment,
                    stored_at,
                    read_at,
                },
            );
        }
    }
    if let Some(key) = k.chain.attestation_key_at_finalized(INSTANCE) {
        m.publish_key_in(venue, key);
    }
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_i2_1_ac_i3_1_a_disclosure_verifies_with_every_aledgent_process_stopped() {
    let mut k = anchoring_kit().await;
    let alice = k.d.party("alice", 1_000).await;
    let bob = k.d.party("bob", 0).await;
    k.pass(ANCHOR_INTERVAL_MS).await;
    until_final(&mut k, 1, 12).await;
    // The adapter published it with its own key, stored before it was broadcast.
    let rows = k.store.anchors().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].extrinsic.as_ref().map(|e| &e.status),
        Some(ExtrinsicStatus::Included(i)) if i.success
    ));
    let (s, v) = disclose(
        &k,
        &alice,
        json!({
            "subject": alice.account,
            "recipient_identity": bob.identity,
            "assertion": { "shape": "holds_at_least", "asset": USD, "amount": "750", "kind": "available" },
        }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let hex = v["result"]["anchoring"]["disclosure"]
        .as_str()
        .expect("the disclosure");
    let bytes = hex::decode(hex).unwrap();
    // The core's own verify agrees.
    let (s, v) =
        k.d.call(
            "GET",
            &format!("/v1/disclosures/verify?disclosure={hex}"),
            Some(&bob.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(v["passed"], true, "{v}");
    // Every aledgent process stops: the core, and the adapter with it.
    let read = public_read(&k, &[1]);
    k.d.core.stop();
    drop(k);
    let report = verify::verify(&bytes, None, &read).unwrap();
    assert!(report.passed(), "{}", report.render());
    assert_eq!(report.key_source, verify::check::KeySource::Published);
    // AC-I2.3, AC-I3.3: the label and the assurance level, printed.
    let text = report.render();
    assert!(text.contains(verify::SIGNED_ASSERTION_LABEL), "{text}");
    assert!(text.contains("Test deployment"), "{text}");
    assert!(text.contains("signed assertion"), "{text}");
    // A tampered disclosure fails.
    let mut tampered = bytes.clone();
    let n = tampered.len();
    tampered[n / 2] ^= 1;
    assert!(verify::verify(&tampered, None, &read).map_or(true, |r| !r.passed()));
    // And an anchor the layer does not hold establishes nothing.
    assert!(!verify::verify(&bytes, None, &verify::MemAnchors::new()).is_ok_and(|r| r.passed()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_i2_2_ac_i3_2_the_recipient_still_cannot_read_the_subjects_account() {
    let mut k = anchoring_kit().await;
    let alice = k.d.party("alice", 1_000).await;
    let bob = k.d.party("bob", 0).await;
    k.pass(ANCHOR_INTERVAL_MS).await;
    until_final(&mut k, 1, 12).await;
    let path = format!(
        "/v1/balances?account={}&asset={USD}",
        alice.account.clone().unwrap_or_default()
    );
    let (before, _) = k.d.call("GET", &path, Some(&bob.token), &[], None).await;
    let (s, v) = disclose(
        &k,
        &alice,
        json!({
            "subject": alice.account,
            "recipient_identity": bob.identity,
            "assertion": { "shape": "holds_at_least", "asset": USD, "amount": "1", "kind": "available" },
        }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (after, v) = k.d.call("GET", &path, Some(&bob.token), &[], None).await;
    assert!(!before.is_success() && !after.is_success(), "{v}");
    assert_eq!(before, after);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_017_03_an_anchor_that_provably_did_not_execute_is_emitted_and_signed_again() {
    let mut k = anchoring_kit().await;
    k.d.party("alice", 1_000).await;
    // The layer loses what it is given: the first extrinsic never lands.
    k.chain.drop_submissions(true);
    k.pass(ANCHOR_INTERVAL_MS).await;
    k.step().await.unwrap();
    assert_eq!(
        anchoring(&k, |s| s.checkpoint(1).map(|c| c.status)),
        Some(CheckpointStatus::Submitted)
    );
    // Its era ends unincluded, with its nonce unspent: provably not executed.
    for _ in 0..80 {
        k.chain.advance(1);
        k.step().await.unwrap();
        if k.store
            .anchors()
            .await
            .unwrap()
            .first()
            .is_some_and(|r| !r.previous.is_empty())
        {
            break;
        }
    }
    let row = k.store.anchors().await.unwrap().remove(0);
    assert_eq!(row.previous.len(), 1, "{row:?}");
    assert!(matches!(
        row.previous[0].status,
        ExtrinsicStatus::Dead(settle::store::NotExecuted::EraEnded { .. })
    ));
    // The core emitted it again, the adapter acknowledged it again, and a new extrinsic carries
    // the same number and commitment.
    k.chain.drop_submissions(false);
    until_final(&mut k, 1, 20).await;
    let c = anchoring(&k, |s| s.checkpoint(1).copied()).unwrap();
    assert_eq!(
        k.chain.anchor_at_finalized(INSTANCE, 1).map(|a| a.0),
        Some(c.commitment)
    );
    assert!(k.chain.anchor_submissions() >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_6_a_restarted_adapter_broadcasts_the_same_anchor_bytes() {
    let mut k = anchoring_kit().await;
    k.d.party("alice", 1_000).await;
    k.chain.drop_submissions(true);
    k.pass(ANCHOR_INTERVAL_MS).await;
    k.step().await.unwrap();
    let signed = k.store.anchors().await.unwrap()[0]
        .extrinsic
        .clone()
        .expect("signed and stored");
    k.restart();
    k.chain.drop_submissions(false);
    until_final(&mut k, 1, 12).await;
    let row = k.store.anchors().await.unwrap().remove(0);
    assert!(row.previous.is_empty(), "{row:?}");
    assert_eq!(
        row.extrinsic.map(|e| e.extrinsic.hash),
        Some(signed.extrinsic.hash)
    );
}

/// `T-027-04`, `INV-19`, `REQ-PR-1`: a checkpoint records the venue its anchor was read in — the
/// settlement contract — and a disclosure resting on it names that venue, so the verifier reads
/// the anchor there and nowhere else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_04_a_checkpoint_records_its_venue_and_the_disclosure_names_it() {
    let mut k = anchoring_kit().await;
    let alice = k.d.party("alice", 1_000).await;
    let bob = k.d.party("bob", 0).await;
    k.pass(ANCHOR_INTERVAL_MS).await;
    until_final(&mut k, 1, 12).await;
    let venue = verify::Venue::Contract {
        account: settle::chain::fake::position_account(INSTANCE),
    };
    let c = anchoring(&k, |s| s.checkpoint(1).copied()).expect("checkpoint 1");
    assert_eq!(c.venue, Some(venue));
    assert_eq!(c.anchored_in(), venue);
    let (s, v) = disclose(
        &k,
        &alice,
        json!({
            "subject": alice.account,
            "recipient_identity": bob.identity,
            "assertion": { "shape": "holds_at_least", "asset": USD, "amount": "1", "kind": "available" },
        }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let bytes = hex::decode(v["result"]["anchoring"]["disclosure"].as_str().unwrap()).unwrap();
    let d = verify::Disclosure::from_bytes(&bytes).unwrap();
    assert_eq!(d.venue, venue);
    assert_eq!(d.version, verify::envelope::VERSION);
    // Found in its venue; the same anchor read as the pallet's is not its anchor.
    let read = public_read(&k, &[1]);
    assert!(verify::verify(&bytes, None, &read).unwrap().passed());
    let mut pallet = verify::MemAnchors::new();
    let (commitment, stored_at) = k.chain.anchor_at_finalized(INSTANCE, 1).unwrap();
    pallet.put(
        INSTANCE,
        1,
        verify::PublicAnchor {
            commitment,
            stored_at,
            read_at: k.chain.finalized_number(),
        },
    );
    let r = verify::verify(
        &bytes,
        Some(ingress::testkit::test_clerk().public_key()),
        &pallet,
    )
    .unwrap();
    assert_eq!(r.anchor, verify::AnchorCheck::Missing);
}

/// `T-027-02`, `REQ-SA-3`: the contract refuses an anchor call — another key stored the number
/// with another commitment between signing and inclusion — and records the refusal. The adapter
/// reports it as the layer names it, with the contract's reason, and the core ingests it with no
/// operation; the checkpoint never becomes final on the other commitment (`REQ-PR-8`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_02_a_refused_anchor_call_is_observed_with_the_contracts_reason() {
    let mut k = anchoring_kit().await;
    k.d.party("alice", 1_000).await;
    k.pass(ANCHOR_INTERVAL_MS).await;
    k.step().await.unwrap();
    assert!(
        k.store.anchors().await.unwrap()[0].extrinsic.is_some(),
        "signed and broadcast"
    );
    // Before the batch lands, another key stores anchor 1 with another commitment.
    k.chain.inject_anchor(INSTANCE, 1, [0xEE; 32]);
    for _ in 0..4 {
        k.chain.advance(1);
        k.step().await.unwrap();
    }
    let refused = k
        .store
        .reports()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.kind == "observe" && r.body["kind"] == "anchor_refused")
        .expect("the refusal was reported");
    assert_eq!(refused.body["payload"], hex::encode("AnchorConflict"));
    assert!(refused.body.get("operation_reference").is_none());
    assert_eq!(refused.verdict.as_deref(), Some("ignored"));
    let ingested = k
        .core_settlement(|s| {
            s.all_ingested()
                .filter(|i| i.observation.kind == ledger_core::settlement::EventKind::AnchorRefused)
                .count()
        })
        .unwrap();
    assert_eq!(ingested, 1);
    assert_ne!(
        anchoring(&k, |s| s.checkpoint(1).map(|c| c.status)),
        Some(CheckpointStatus::Final)
    );
    assert!(anchoring(&k, |s| s.incidents().iter().any(|i| matches!(
        i,
        ledger_core::anchor::AnchorIncident::CommitmentMismatch { number: 1, .. }
    ))));
}

/// `T-027-12`, `AC-E4.4`, `REQ-SA-9`: the anchor key's first transaction on a runtime not served
/// before is confirmed by the layer too. Refused, it is neither stored nor broadcast, and the
/// adapter refuses to serve on that runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_12_an_anchor_the_layer_does_not_confirm_is_never_stored_or_broadcast() {
    let mut k = anchoring_kit().await;
    k.d.party("alice", 1_000).await;
    k.chain.set_spec_version(134);
    k.chain
        .answer_validations(Some(settle::chain::Validity::Refused(
            "Invalid(BadSigner)".into(),
        )));
    k.pass(ANCHOR_INTERVAL_MS).await;
    k.step().await.unwrap();
    assert_eq!(k.chain.validations(), 1);
    let (serving, why) = k.adapter.status();
    assert!(!serving);
    assert!(
        matches!(
            why,
            [settle::adapter::Refusing::Unconfirmed {
                spec_version: 134,
                ..
            }]
        ),
        "{why:?}"
    );
    assert!(
        k.store
            .anchors()
            .await
            .unwrap()
            .iter()
            .all(|r| r.extrinsic.is_none())
    );
    assert_eq!(k.chain.anchor_submissions(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adapter_without_an_anchor_key_does_not_anchor() {
    let mut k = Kit::start().await;
    k.step().await.unwrap();
    k.d.party("alice", 1_000).await;
    k.pass(ANCHOR_INTERVAL_MS).await;
    for _ in 0..4 {
        k.chain.advance(1);
        k.step().await.unwrap();
    }
    assert_eq!(k.chain.anchor_submissions(), 0);
    assert!(k.store.anchors().await.unwrap().is_empty());
    // The checkpoint waits, pending; holders are not affected (REQ-PR-9).
    assert_eq!(
        anchoring(&k, |s| s.in_flight().map(|c| c.status)),
        Some(CheckpointStatus::Pending)
    );
}
