// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter end to end against the fake settlement layer: ingress, the in-process core, the
//! adapter and its store, in one process (`settle::testkit`). Tests are named after the task,
//! requirement, invariant or acceptance criterion they verify.

use ingress::testkit::{Party, USD};
use ledger_core::id::{AccountId, OperationId, account_reference};
use ledger_core::settlement::{Incident, NonceFact, SigningRecord};
use serde_json::json;
use settle::adapter::{CrashPoint, Refusing};
use settle::chain::fake::FakeConfig;
use settle::store::{ExtrinsicStatus, InstructionState, NotExecuted, ParkReason, Store};
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

async fn open_alerts(k: &Kit, prefix: &str) -> Vec<settle::store::AlertRow> {
    k.store
        .alerts()
        .await
        .unwrap()
        .into_iter()
        .filter(|a| a.key.starts_with(prefix) && a.cleared_at.is_none())
        .collect()
}

// ------------------------------------------------------------------------------ the happy path

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_so_6_a_withdrawal_is_submitted_observed_and_settled_only_at_finality() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 10_000).await;
    let op = k.withdrawal(&alice, 250, "w1").await;
    assert_eq!(k.state(&alice, &op).await, "Reserved");
    let r = k.step().await.unwrap();
    assert_eq!((r.acknowledged, r.signed), (1, 1));
    assert_eq!(k.state(&alice, &op).await, "Submitted");
    // Included above the finality point: observed provisionally, nothing posted.
    k.chain.produce();
    k.step().await.unwrap();
    assert_eq!(k.state(&alice, &op).await, "Observed");
    assert_eq!(k.balances(&alice).await, (9_750, 250));
    // Final: settled.
    k.chain.finalize_all();
    k.step().await.unwrap();
    assert_eq!(k.state(&alice, &op).await, "Settled");
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 250);
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&op)), 1);
    assert_eq!(k.balances(&alice).await, (9_750, 0));
    assert_eq!(instruction(&k, &op).await.state, InstructionState::Executed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_d1_4_a_deposit_to_an_account_reference_is_issued_at_finality() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 0).await;
    let account = AccountId::parse(alice.account.as_deref().unwrap()).unwrap();
    let from = [0x77; 32];
    k.chain.mint(from, &chain_usd(), 5_000);
    k.chain.deposit(
        from,
        INSTANCE,
        account_reference(&account),
        &chain_usd(),
        1_200,
    );
    k.chain.produce();
    k.step().await.unwrap();
    // Not final, not read: nothing yet.
    assert_eq!(k.balances(&alice).await, (0, 0));
    k.chain.finalize_all();
    k.step().await.unwrap();
    assert_eq!(k.balances(&alice).await, (1_200, 0));
    // A deposit addressed to nothing the book holds: recorded, not associated, nothing issued.
    k.chain
        .deposit(from, INSTANCE, [0x42; 32], &chain_usd(), 10);
    k.chain.advance(1);
    k.step().await.unwrap();
    let verdicts: Vec<String> = k
        .store
        .reports()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "observe")
        .filter_map(|r| r.verdict)
        .collect();
    assert!(
        verdicts.contains(&"not_associated".to_owned()),
        "{verdicts:?}"
    );
}

// ------------------------------------------------------------------------------ T-016-01

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_016_01_a_second_instance_cannot_take_the_lease() {
    let mut k = serving_kit(KitOptions::default()).await;
    let mut second = k.adapter_on(k.store.clone(), "settle-1");
    assert_eq!(
        second.step().await.unwrap_err(),
        settle::AdapterError::LeaseLost
    );
    // The first keeps it by stepping; once it stops renewing and the lease runs out, the
    // second takes it over, and the first is fenced.
    k.step().await.unwrap();
    k.d.clock.advance(k.adapter.config().lease_ttl + 1);
    second.step().await.unwrap();
    assert_eq!(k.step().await.unwrap_err(), settle::AdapterError::LeaseLost);
    // A stale epoch cannot sign into a lane.
    let lane = settle::store::LaneRow {
        lane: "x".into(),
        next_nonce: 0,
        in_flight: None,
    };
    assert_eq!(
        k.store.put_lane(1, &lane).await,
        Err(settle::store::StoreError::Fenced)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_8_the_core_refuses_the_adapter_principal_outside_its_three_capabilities() {
    let k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let adapter = Some(k.adapter_party.token.as_str());
    let forbidden = |s: axum::http::StatusCode, v: &serde_json::Value| {
        s.as_u16() == 403 && v["error"] == "ERR-NotAuthorised"
    };
    // No reservation, no issuance, no operator command, no read of the book.
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/reservations",
            adapter,
            &[],
            Some(json!({ "payer": alice.account, "asset": USD, "amount": "1" })),
        )
        .await;
    assert!(forbidden(s, &v), "{s} {v}");
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/issuances",
            adapter,
            &[],
            Some(json!({ "asset": USD, "destination": alice.account, "amount": "1" })),
        )
        .await;
    assert!(forbidden(s, &v), "{s} {v}");
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/ops/adapter-support",
            adapter,
            &[],
            Some(json!({ "legs": [], "justification": "x" })),
        )
        .await;
    assert!(forbidden(s, &v), "{s} {v}");
    let (s, v) =
        k.d.call(
            "GET",
            &format!("/v1/balances?account={}", alice.account.as_deref().unwrap()),
            adapter,
            &[],
            None,
        )
        .await;
    assert!(forbidden(s, &v), "{s} {v}");
    // And nobody else holds the adapter's capabilities.
    let (s, v) =
        k.d.call(
            "GET",
            "/v1/settlement/instructions",
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert!(forbidden(s, &v), "{s} {v}");
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/settlement/heads",
            Some(&k.d.operator.token),
            &[],
            Some(json!({ "height": 1, "next_seq": 0 })),
        )
        .await;
    assert!(forbidden(s, &v), "{s} {v}");
}

// ------------------------------------------------------------------------------ T-016-02

async fn refuses_with(
    options: KitOptions,
    reconfig: impl FnOnce(&mut Kit),
) -> (Kit, Vec<Refusing>) {
    let mut k = Kit::start_with(options).await;
    reconfig(&mut k);
    let r = k.step().await.unwrap();
    assert!(!r.serving);
    let why = k.adapter.status().1.to_vec();
    (k, why)
}

async fn unavailable(k: &Kit) {
    let alice = k.d.party("zed", 1_000).await;
    let (s, v) = k.withdraw(&alice, 10, "refused").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (422, Some("ERR-SettlementLayerUnavailable")),
        "{v}"
    );
    assert_eq!(k.balances(&alice).await, (1_000, 0), "nothing reserved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_2_a_wrong_genesis_stops_service() {
    let options = KitOptions {
        fake: FakeConfig {
            genesis: [0x01; 32],
            ..FakeConfig::default()
        },
        ..KitOptions::default()
    };
    let (k, why) = refuses_with(options, |_| {}).await;
    assert!(matches!(why[..], [Refusing::Genesis { .. }]), "{why:?}");
    unavailable(&k).await;
}

/// `AC-E4.4`, `REQ-SA-9`: a runtime whose `transaction_version` is none the adapter has a format
/// for is refused, whatever its `spec_version`: nothing is signed for it rather than guessed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_4_a_runtime_with_an_unknown_transaction_version_stops_service() {
    let options = KitOptions {
        fake: FakeConfig {
            spec_version: 135,
            transaction_version: 14,
            ..FakeConfig::default()
        },
        ..KitOptions::default()
    };
    let (k, why) = refuses_with(options, |_| {}).await;
    let f = settle::chain::kreivo::FORMAT_133;
    assert_eq!(
        why,
        vec![Refusing::UnknownSurface {
            spec_version: 135,
            transaction_version: 14,
            extensions: f.extensions.iter().map(|e| (*e).to_owned()).collect(),
            types: hex::encode(f.types),
        }]
    );
    unavailable(&k).await;
    assert!(k.store.extrinsics().await.unwrap().is_empty());
    assert_eq!(k.chain.validations(), 0, "nothing was signed to validate");
}

/// `T-027-12`, `REQ-SA-9`: a `spec_versions` list from an older configuration gates nothing,
/// either way. A runtime it does not name is served when its surface is known (the next tests),
/// and one it names is refused when its surface is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_12_spec_versions_listed_in_the_configuration_gate_nothing() {
    let options = KitOptions {
        fake: FakeConfig {
            spec_version: 135,
            types: Some([7; 32]),
            ..FakeConfig::default()
        },
        ..KitOptions::default()
    };
    let (k, why) = refuses_with(options, |k| k.reconfigure(|c| c.spec_versions.push(135))).await;
    assert!(
        matches!(
            why[..],
            [Refusing::UnknownSurface {
                spec_version: 135,
                ..
            }]
        ),
        "{why:?}"
    );
    unavailable(&k).await;
    let mut k = serving_kit(KitOptions::default()).await;
    k.reconfigure(|c| c.spec_versions = vec![1, 2]);
    assert!(k.step().await.unwrap().serving, "{:?}", k.adapter.status());
}

/// `T-027-07`, `T-027-12`, `AC-E4.4`: a runtime whose transaction extensions are none a format
/// was pinned with is refused (here, spec 133's `transaction_version` and types with spec 132's
/// extensions, without `DynamicMaxBlockWeight`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_027_07_ac_e4_4_a_runtime_with_other_extensions_stops_service() {
    let found: Vec<String> = settle::chain::kreivo::EXTENSIONS_132
        .iter()
        .map(|e| (*e).to_owned())
        .collect();
    let options = KitOptions {
        fake: FakeConfig {
            spec_version: 133,
            extensions: found.clone(),
            ..FakeConfig::default()
        },
        ..KitOptions::default()
    };
    let (k, why) = refuses_with(options, |_| {}).await;
    assert_eq!(
        why,
        vec![Refusing::UnknownSurface {
            spec_version: 133,
            transaction_version: 13,
            extensions: found,
            types: hex::encode(settle::chain::kreivo::TYPES_133),
        }]
    );
    unavailable(&k).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_2_a_missing_instance_or_pallet_stops_service() {
    let (k, why) = refuses_with(
        KitOptions {
            registered: false,
            ..KitOptions::default()
        },
        |_| {},
    )
    .await;
    assert_eq!(why, vec![Refusing::NoInstance]);
    unavailable(&k).await;
    let (k, why) = refuses_with(
        KitOptions {
            fake: FakeConfig {
                pallet: false,
                ..FakeConfig::default()
            },
            ..KitOptions::default()
        },
        |_| {},
    )
    .await;
    assert_eq!(why, vec![Refusing::NoPallet]);
    unavailable(&k).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_f5_5_a_metadata_change_after_binding_holds_nothing() {
    // USD is bound from genesis. Root changes its metadata on the layer (a force_set_metadata).
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    k.chain.set_decimals(&chain_usd(), USD, 6);
    let r = k.step().await.unwrap();
    // The adapter keeps serving, nothing is held, and withdrawals go on (REQ-AS-9).
    assert!(r.serving);
    let (s, v) = k.withdraw(&alice, 10, "after").await;
    assert_eq!(s, 200, "{v}");
    assert!(open_alerts(&k, "asset:").await.is_empty());
}

async fn operator_may(k: &Kit, kinds: &[&str]) {
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/ops/operator-grants",
            Some(&k.d.operator.token),
            &[],
            Some(json!({
                "principal": k.d.operator.identity,
                "kinds": kinds,
                "justification": "t",
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
}

/// TSTA at `Here(101)`, a live asset as Kreivo holds one (`TSTA`, 2 decimals), mapped by the
/// adapter; registered book-only, with an issuance grant held by the operator.
async fn book_only_tsta(k: &mut Kit) -> settle::chain::ChainAsset {
    use ledger_core::settlement::assets::{LayerAssetFacts, LayerAssetStatus};
    use settle::chain::ChainAsset;
    use settle::config::AssetBinding;
    let tsta = ChainAsset("Here(101)".into());
    k.chain.set_metadata(
        &tsta,
        Some(LayerAssetFacts {
            symbol: b"TSTA".to_vec(),
            decimals: 2,
            status: LayerAssetStatus::Live,
        }),
    );
    k.reconfigure(|c| {
        c.assets.push(AssetBinding {
            asset: "TSTA".into(),
            chain: tsta.clone(),
        })
    });
    assert!(k.step().await.unwrap().serving);
    operator_may(k, &["register_asset"]).await;
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/ops/assets",
            Some(&k.d.operator.token),
            &[("client-key", "register-tsta")],
            Some(json!({
                "id": "TSTA", "precision": 2, "kind": "stable",
                "review": "cd".repeat(32), "justification": "CR-8: TSTA/2, book-only",
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/grants",
            Some(&k.d.operator.token),
            &[],
            Some(json!({
                "kind": "issuance",
                "principal": k.d.operator.identity,
                "subject": { "asset": "TSTA" },
                "ops": ["issue", "redeem"],
                "per_window": { "length": 86_400_000u64, "limit": "1000000000000" },
                "assets": ["TSTA"],
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    tsta
}

async fn issue_tsta(k: &Kit, account: &str, amount: u64, key: &str) -> (u16, serde_json::Value) {
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/issuances",
            Some(&k.d.operator.token),
            &[("client-key", key)],
            Some(json!({ "asset": "TSTA", "destination": account, "amount": amount.to_string() })),
        )
        .await;
    (s.as_u16(), v)
}

async fn bind_tsta(k: &Kit, key: &str) -> (u16, serde_json::Value) {
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/ops/assets/TSTA/bind",
            Some(&k.d.operator.token),
            &[("client-key", key)],
            Some(json!({ "location": "Here(101)", "justification": "CR-9: bind TSTA on Kreivo" })),
        )
        .await;
    (s.as_u16(), v)
}

async fn tsta_binding(k: &Kit) -> String {
    let (_, assets) =
        k.d.call("GET", "/v1/assets", Some(&k.d.operator.token), &[], None)
            .await;
    assets
        .as_array()
        .and_then(|a| a.iter().find(|a| a["id"] == "TSTA"))
        .and_then(|a| a["binding"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Rounds until TSTA's binding is `want`, at most `n`.
async fn until_binding(k: &mut Kit, want: &str, n: usize) -> String {
    for _ in 0..n {
        let b = tsta_binding(k).await;
        if b == want {
            return b;
        }
        k.rounds(1).await.unwrap_or_else(|e| panic!("a round: {e}"));
    }
    tsta_binding(k).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_f5_1_to_ac_f5_4_a_book_only_asset_is_bound_at_zero_divergence_through_the_contract() {
    let mut k = serving_kit(KitOptions {
        anchoring: true,
        ..KitOptions::default()
    })
    .await;
    let tsta = book_only_tsta(&mut k).await;
    assert_eq!(tsta_binding(&k).await, "book_only");
    // AC-F5.1: issued and moved in the book; no external leg.
    let alice = k.d.party("alice", 0).await;
    let account = alice.account.clone().unwrap();
    let (s, v) = issue_tsta(&k, &account, 500, "i1").await;
    assert_eq!(s, 200, "{v}");
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/withdrawals",
            Some(&alice.token),
            &[("client-key", "w-book-only")],
            Some(json!({ "payer": account, "asset": "TSTA", "amount": "10" })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["error"].as_str()),
        (422, Some("ERR-UnsupportedLeg")),
        "{v}"
    );
    // AC-F5.3: the issuer puts one minor unit less than the book's outstanding balance in the
    // contract's account, by a plain transfer. The binding is asked for and, while it is pending,
    // issuance in the asset is refused.
    k.chain.top_up_position(INSTANCE, &tsta, 499);
    let (s, v) = bind_tsta(&k, "b1").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(tsta_binding(&k).await, "binding");
    let (s, v) = issue_tsta(&k, &account, 1, "i2").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (403, Some("ERR-NotAuthorised")),
        "{v}"
    );
    // The contract refuses it on chain; once that is final the asset is book-only again.
    assert_eq!(until_binding(&mut k, "book_only", 40).await, "book_only");
    let refused = k
        .core_settlement(|s| {
            s.incidents()
                .iter()
                .any(|i| matches!(i, Incident::BindRefused { .. }))
        })
        .unwrap();
    assert!(refused, "the refusal is an incident");
    // AC-F5.4: the issuer puts the missing unit in; the binding is asked again and is final.
    k.chain.top_up_position(INSTANCE, &tsta, 1);
    let (s, v) = bind_tsta(&k, "b2").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(until_binding(&mut k, "bound", 40).await, "bound");
    // Its external legs now work: a withdrawal settles from the position the binding recorded.
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/withdrawals",
            Some(&alice.token),
            &[("client-key", "w-bound")],
            Some(json!({ "payer": account, "asset": "TSTA", "amount": "10" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    // A binding is never asked twice.
    let (s, v) = bind_tsta(&k, "b3").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (403, Some("ERR-NotAuthorised")),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_f5_2_a_location_that_does_not_describe_the_asset_in_its_fields_is_refused() {
    use ledger_core::settlement::assets::{LayerAssetFacts, LayerAssetStatus};
    let mut k = serving_kit(KitOptions {
        anchoring: true,
        ..KitOptions::default()
    })
    .await;
    let tsta = book_only_tsta(&mut k).await;
    // The layer's decimals field says 6: refused, the asset stays book-only.
    k.chain.set_metadata(
        &tsta,
        Some(LayerAssetFacts {
            symbol: b"TSTA".to_vec(),
            decimals: 6,
            status: LayerAssetStatus::Live,
        }),
    );
    let lease = k.adapter.config().status_lease;
    k.d.clock.advance(lease);
    k.step().await.unwrap();
    let (s, v) = bind_tsta(&k, "d6").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (422, Some("ERR-UnsupportedLeg")),
        "{v}"
    );
    assert!(!open_alerts(&k, "asset:TSTA").await.is_empty());
    // No metadata at all (Kreivo's dUSD): refused the same way.
    k.chain.set_metadata(&tsta, None);
    k.d.clock.advance(lease);
    k.step().await.unwrap();
    let (s, v) = bind_tsta(&k, "none").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (422, Some("ERR-UnsupportedLeg")),
        "{v}"
    );
    assert_eq!(tsta_binding(&k).await, "book_only");
}

/// `AC-E4.4`, `AC-E4.2`: an upgrade to a runtime whose surface the adapter has no format for
/// (here, a new `transaction_version`) stops submission and signing; back on a known surface, it
/// serves again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_4_an_upgrade_to_an_unknown_surface_stops_submission_and_signing() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let (s, v) = k.withdraw(&alice, 10, "w").await;
    assert_eq!(s, 200, "{v}");
    let op = v["operation_reference"].as_str().unwrap().to_owned();
    k.chain.upgrade(|c| {
        c.spec_version = 200;
        c.transaction_version = 14;
    });
    let r = k.step().await.unwrap();
    assert!(!r.serving);
    // Reserved, not handed over: submission is refused, and nothing is signed.
    let (s, v) = k.submit(&alice, &op).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (422, Some("ERR-SettlementLayerUnavailable")),
        "{v}"
    );
    assert!(k.store.extrinsics().await.unwrap().is_empty());
    assert!(!open_alerts(&k, "refusing").await.is_empty());
    // Back on a known surface: served again.
    k.chain.upgrade_to(201, &settle::chain::kreivo::FORMAT_133);
    assert!(k.step().await.unwrap().serving);
    // A new request (the refused one keeps its answer under its client key, REQ-OP-11).
    let (s, v) =
        k.d.call(
            "POST",
            &format!("/v1/withdrawals/{op}/submit"),
            Some(&alice.token),
            &[("client-key", "second-try")],
            Some(json!({ "destination": { "account": hex::encode(PAYEE) } })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_2_a_stale_declaration_makes_the_layer_unavailable() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    // The adapter stops declaring: once its last declaration is older than its lease, the core
    // refuses withdrawals.
    k.pass(k.adapter.config().status_lease + 1).await;
    let (s, v) = k.withdraw(&alice, 10, "late").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (422, Some("ERR-SettlementLayerUnavailable")),
        "{v}"
    );
    // It declares again: served.
    k.step().await.unwrap();
    let (s, v) = k.withdraw(&alice, 10, "again").await;
    assert_eq!(s, 200, "{v}");
}

// ------------------------------------------------------------------------------ T-016-03

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e4_1_an_unsupported_asset_is_refused_at_submission_before_any_reservation() {
    let k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    // EUR is in the registry, not in the adapter's declaration.
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/withdrawals",
            Some(&alice.token),
            &[],
            Some(json!({ "payer": alice.account, "asset": "EUR", "amount": "5" })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["error"].as_str()),
        (422, Some("ERR-UnsupportedLeg")),
        "{v}"
    );
    assert_eq!(k.balances(&alice).await, (1_000, 0));
    // The operator's registration through CTR-OPS replaces the declaration: USD is no longer
    // supported either.
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/ops/operator-grants",
            Some(&k.d.operator.token),
            &[],
            Some(json!({
                "principal": k.d.operator.identity,
                "kinds": ["declare_adapter_support"],
                "justification": "t",
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/ops/adapter-support",
            Some(&k.d.operator.token),
            &[],
            Some(json!({ "legs": [{ "kind": "issue", "asset": "EUR" }], "justification": "t" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let (s, v) = k.withdraw(&alice, 5, "usd").await;
    assert_eq!((s, v["error"].as_str()), (422, Some("ERR-UnsupportedLeg")));
}

// ------------------------------------------------------------------------------ T-016-04

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_so_6_the_core_reaches_submitted_only_after_the_row_is_durable() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    // Stored, then dead before the acknowledgement: the core still has it Reserved.
    k.adapter.crash_at(CrashPoint::AfterStore);
    assert_eq!(
        k.step().await.unwrap_err(),
        settle::AdapterError::Crashed(CrashPoint::AfterStore)
    );
    assert!(!instruction(&k, &op).await.acknowledged);
    assert_eq!(k.state(&alice, &op).await, "Reserved");
    // Acknowledged, then dead before the row says so: the next process sends the same
    // acknowledgement again, under the same client key.
    k.restart();
    k.adapter.crash_at(CrashPoint::AfterAck);
    assert!(k.step().await.is_err());
    assert_eq!(k.state(&alice, &op).await, "Submitted");
    assert!(!instruction(&k, &op).await.acknowledged);
    k.restart();
    k.step().await.unwrap();
    assert!(instruction(&k, &op).await.acknowledged);
    assert_eq!(until(&mut k, &alice, &op, "Settled", 5).await, "Settled");
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&op)), 1);
}

// ------------------------------------------------------------------------------ T-016-05

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_6_a_crash_at_any_submission_step_never_yields_a_second_extrinsic() {
    for point in [
        CrashPoint::AfterSign,
        CrashPoint::AfterPersist,
        CrashPoint::AfterBroadcast,
    ] {
        let mut k = serving_kit(KitOptions::default()).await;
        let alice = k.d.party("alice", 1_000).await;
        let op = k.withdrawal(&alice, 10, "w").await;
        k.adapter.crash_at(point);
        assert_eq!(
            k.step().await.unwrap_err(),
            settle::AdapterError::Crashed(point)
        );
        k.restart();
        assert_eq!(
            until(&mut k, &alice, &op, "Settled", 6).await,
            "Settled",
            "{point:?}"
        );
        assert_eq!(
            k.chain.extrinsics_for(&Kit::chain_ref(&op)),
            1,
            "{point:?}: one extrinsic for one operation reference"
        );
        let exts = k.store.extrinsics().await.unwrap();
        assert_eq!(exts.len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_so_7_an_uncertain_nonce_is_never_reused_and_a_dead_one_is() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    // The node loses the first extrinsic: it stays uncertain until its era ends.
    k.chain.drop_submissions(true);
    let first = k.withdrawal(&alice, 10, "w1").await;
    k.step().await.unwrap();
    let second = k.withdrawal(&alice, 20, "w2").await;
    let e1 = k.store.extrinsics().await.unwrap();
    assert_eq!(e1.len(), 1);
    let n1 = e1[0].nonce;
    let death = e1[0].era.death();
    // While the first is uncertain the lane is busy: the second is not signed.
    for _ in 0..3 {
        k.chain.advance(1);
        k.step().await.unwrap();
        assert_eq!(k.store.extrinsics().await.unwrap().len(), 1);
    }
    // The first's era ends in a finalized block with its nonce unspent: provably not
    // executed, and the lane takes the same nonce for the second.
    while k.chain.finalized_number() < death {
        k.chain.advance(1);
    }
    k.chain.drop_submissions(false);
    k.step().await.unwrap();
    let row = instruction(&k, &first).await;
    assert!(
        matches!(
            row.state,
            InstructionState::NotExecuted {
                proof: NotExecuted::EraEnded { .. }
            }
        ),
        "{row:?}"
    );
    let exts = k.store.extrinsics().await.unwrap();
    let e2 = exts.iter().find(|e| e.operation == second).expect("signed");
    assert_eq!(e2.nonce, n1, "the dead nonce is used again");
    assert_eq!(
        until(&mut k, &alice, &second, "Settled", 5).await,
        "Settled"
    );
    // The first: released once the finalized head passes its expiry (REQ-SL-4, AC-E1.4).
    assert_eq!(
        until(&mut k, &alice, &first, "Released", 60).await,
        "Released"
    );
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&first)), 1);
    assert_eq!(k.balances(&alice).await, (980, 0));
}

// ------------------------------------------------------------------------------ T-016-06

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_7_the_cursor_never_passes_a_position_it_did_not_report() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    k.step().await.unwrap();
    k.chain.advance(1);
    for point in [
        CrashPoint::AfterObserve,
        CrashPoint::AfterReport,
        CrashPoint::AfterVerdict,
    ] {
        k.adapter.crash_at(point);
        assert!(k.step().await.is_err(), "{point:?}");
        assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 0, "{point:?}");
        k.restart();
    }
    k.step().await.unwrap();
    assert_eq!(k.store.cursor(INSTANCE).await.unwrap(), 1);
    assert_eq!(k.state(&alice, &op).await, "Settled");
    // Every report has its verdict, and one coordinate was applied once.
    let reports = k.store.reports().await.unwrap();
    assert!(reports.iter().all(|r| r.verdict.is_some()), "{reports:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inv_12_a_repeated_coordinate_gets_already_applied() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    assert_eq!(until(&mut k, &alice, &op, "Settled", 4).await, "Settled");
    let obs = k
        .store
        .reports()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.kind == "observe")
        .unwrap();
    // The same observation under another client key: the core's verdict is "already applied".
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/settlement/observations",
            Some(&k.adapter_party.token),
            &[("client-key", "again")],
            Some(obs.body.clone()),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["error"].as_str()),
        (409, Some("ERR-ObservationDuplicate")),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_in_4_a_provisional_record_a_reorg_replaces_is_orphaned_and_the_final_one_settles() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    k.step().await.unwrap();
    k.chain.produce();
    k.step().await.unwrap();
    assert_eq!(k.state(&alice, &op).await, "Observed");
    // A reorg above the finality point: a deposit now takes position 0, the withdrawal 1.
    assert_eq!(k.chain.reorg(1, false), 1);
    let from = [0x77; 32];
    k.chain.mint(from, &chain_usd(), 100);
    k.chain.deposit(from, INSTANCE, [0x42; 32], &chain_usd(), 1);
    k.chain.advance(1);
    k.step().await.unwrap();
    assert_eq!(until(&mut k, &alice, &op, "Settled", 3).await, "Settled");
    let checks: Vec<_> = k
        .store
        .reports()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "check" && r.body["still_matches"] == false)
        .collect();
    assert_eq!(checks.len(), 1, "the orphaned coordinate was checked once");
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&op)), 1);
}

// ------------------------------------------------------------------------------ T-016-07

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_4_each_disposition_is_kept_apart() {
    // Executed.
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let executed = k.withdrawal(&alice, 10, "executed").await;
    assert_eq!(
        until(&mut k, &alice, &executed, "Settled", 4).await,
        "Settled"
    );
    assert_eq!(
        instruction(&k, &executed).await.state,
        InstructionState::Executed
    );

    // The call failed in a finalized block: its nonce is spent, nothing moved, and no inbox
    // record names it. The batch carrying it failed as a whole (`F-027`): the contract's admin
    // removed the lane's key before it landed, so `execute` reverted and wrote nothing.
    let failed = k.withdrawal(&alice, 20, "failed").await;
    k.step().await.unwrap();
    let lane = k.lanes[0].account();
    k.chain.set_keys(INSTANCE, &[]);
    k.chain.advance(1);
    k.chain.set_keys(INSTANCE, &[lane]);
    k.chain.advance(1);
    k.step().await.unwrap();
    let row = instruction(&k, &failed).await;
    assert!(
        matches!(
            row.state,
            InstructionState::NotExecuted {
                proof: NotExecuted::DispatchFailed { .. }
            }
        ),
        "{row:?}"
    );
    let ext = k
        .store
        .extrinsics()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.operation == failed)
        .unwrap();
    assert!(matches!(ext.status, ExtrinsicStatus::Included(ref i) if !i.success));
    // The owner's ruling of 2026-09-24: the inbox shows no record, but the signing key's nonce
    // shows the transaction was included. The core cannot tell a failed dispatch from an
    // execution its reader missed, and never releases on that: review, still reserved, with an
    // incident that says so.
    assert_eq!(
        until(&mut k, &alice, &failed, "Needs-Review", 60).await,
        "Needs-Review"
    );
    let failed_op = OperationId::parse(&failed).unwrap();
    assert!(
        k.core_settlement(|s| s
            .incidents()
            .iter()
            .any(|i| matches!(i, Incident::IncludedWithoutRecord { op, .. } if *op == failed_op)))
            .unwrap()
    );
    assert_eq!(k.balances(&alice).await.1, 20, "still reserved");

    // Refused by the layer: WithdrawRefused. Once the record is final and the signing key's
    // nonce shows its transaction included, it is a proved rejection (amendment 0006 item 3,
    // `REQ-OP-9`): the reservation is released at once, with no review.
    let refused = k.withdrawal(&alice, 30, "refused").await;
    k.step().await.unwrap();
    k.chain.set_position(INSTANCE, &chain_usd(), 0);
    k.chain.advance(1);
    k.step().await.unwrap();
    assert!(matches!(
        instruction(&k, &refused).await.state,
        InstructionState::Refused { .. }
    ));
    assert_eq!(
        until(&mut k, &alice, &refused, "Released", 60).await,
        "Released"
    );
    assert_eq!(k.balances(&alice).await.1, 20, "the refused 30 released");
}

/// The owner's ruling of 2026-09-24, `INV-11`, `REQ-SA-4`: the layer executes a withdrawal, but
/// the reader loses the operation reference of its inbox entry (a reader fault). The inbox then
/// shows no record of the operation over a range that is complete and final, which alone would
/// prove non-inclusion; the signing key's nonce, which the adapter reports with every head,
/// shows the transaction was included. Review, never release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inv_11_an_included_transaction_whose_inbox_entry_the_reader_misses_goes_to_review() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 40, "w").await;
    k.chain.misread_reference_of(Kit::chain_ref(&op));
    assert_eq!(
        until(&mut k, &alice, &op, "Needs-Review", 80).await,
        "Needs-Review"
    );
    // It did execute: the payee holds the value.
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 40);
    let id = OperationId::parse(&op).unwrap();
    let lane = k.lanes[0].account();
    let (signing, fact, incident) = k
        .core_settlement(|s| {
            let i = s.instruction(&id).unwrap();
            let incident = s.incidents().iter().any(|x| {
                matches!(x, Incident::IncludedWithoutRecord { op, key, .. }
                    if *op == id && *key == lane)
            });
            (i.signing, i.nonce, incident)
        })
        .unwrap();
    // The core recorded the transaction's key before it was broadcast, and its nonce consumed.
    assert!(
        matches!(signing, Some(SigningRecord::Signed(s)) if s.key == lane),
        "{signing:?}"
    );
    assert!(matches!(fact, Some(NonceFact::Consumed { .. })), "{fact:?}");
    assert!(incident);
    // Reserved, never released, however long the layer runs.
    k.rounds(10).await.unwrap();
    assert_eq!(k.state(&alice, &op).await, "Needs-Review");
    assert_eq!(k.balances(&alice).await, (960, 40));
}

/// The owner's ruling of 2026-09-24: every finalised head the adapter reports carries the
/// account nonce of each signing key, read at that block; and nothing is broadcast before the
/// core records the transaction's key and nonce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_4_heads_carry_the_signing_keys_nonces_and_signings_precede_broadcast() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    assert_eq!(until(&mut k, &alice, &op, "Settled", 5).await, "Settled");
    let reports = k.store.reports().await.unwrap();
    let lane = hex::encode(k.lanes[0].account());
    let heads: Vec<_> = reports.iter().filter(|r| r.kind == "head").collect();
    assert!(!heads.is_empty());
    for h in &heads {
        let nonces = h.body["nonces"].as_array().expect("nonces");
        assert!(nonces.iter().any(|n| n["key"] == lane), "{}", h.body);
    }
    let sign = reports
        .iter()
        .find(|r| r.kind == "sign" && r.key == format!("settle/sign/{op}"))
        .expect("the signing was recorded");
    assert_eq!(sign.verdict.as_deref(), Some("decided"));
    assert_eq!(sign.body["record"]["signed"]["key"], lane);
    let ext = k
        .store
        .extrinsics()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.operation == op)
        .unwrap();
    assert_eq!(sign.body["record"]["signed"]["nonce"], ext.nonce);
    assert_eq!(
        sign.body["record"]["signed"]["last_valid"],
        ext.era.last_valid()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e1_5_an_unknown_outcome_stays_non_terminal_and_releases_nothing() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    k.step().await.unwrap();
    // Block production stops after the broadcast.
    k.chain.halt(true);
    for _ in 0..3 {
        k.step().await.unwrap();
    }
    assert_eq!(k.state(&alice, &op).await, "Submitted");
    // Nothing about the operation was reported: no observation, no status.
    let reports = k.store.reports().await.unwrap();
    assert!(
        !reports
            .iter()
            .any(|r| r.kind == "observe" || r.kind == "check"),
        "{reports:?}"
    );
    // Its deadline passes: Needs-Review, never Released.
    k.pass(600_000).await;
    let s = k.state(&alice, &op).await;
    assert_eq!(s, "Needs-Review");
    assert_eq!(k.balances(&alice).await, (990, 10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e1_4_an_instruction_that_can_no_longer_be_signed_is_tombstoned_and_released() {
    let mut k = serving_kit(KitOptions {
        position: 0,
        ..KitOptions::default()
    })
    .await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    k.step().await.unwrap();
    assert!(matches!(
        instruction(&k, &op).await.state,
        InstructionState::Parked { .. }
    ));
    // Parked past its expiry: tombstoned, never signed, and released on proof.
    assert_eq!(until(&mut k, &alice, &op, "Released", 60).await, "Released");
    assert!(matches!(
        instruction(&k, &op).await.state,
        InstructionState::NotExecuted {
            proof: NotExecuted::NeverSigned { .. }
        }
    ));
    // Liquidity returns: the tombstone holds, nothing is ever signed for it.
    // Liquidity the contract accounts for (`F-027`): a plain transfer would not raise it.
    k.chain.fund_position(INSTANCE, &chain_usd(), 1_000);
    k.rounds(3).await.unwrap();
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&op)), 0);
    assert_eq!(k.balances(&alice).await, (1_000, 0));
}

// ------------------------------------------------------------------------------ T-016-08

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e3_1_and_e3_2_twenty_withdrawals_on_a_short_position_park_raise_one_alert_and_resume() {
    let mut k = serving_kit(KitOptions {
        position: 5,
        expiry_window: 200,
        ..KitOptions::default()
    })
    .await;
    let alice = k.d.party("alice", 10_000).await;
    let mut ops = Vec::new();
    for i in 0..20 {
        ops.push(k.withdrawal(&alice, 10, &format!("w{i}")).await);
    }
    k.step().await.unwrap();
    k.step().await.unwrap();
    for op in &ops {
        assert!(
            matches!(
                instruction(&k, op).await.state,
                InstructionState::Parked {
                    reason: ParkReason::Liquidity,
                    ..
                }
            ),
            "{op}"
        );
    }
    let alerts = open_alerts(&k, "position:").await;
    assert_eq!(alerts.len(), 1, "one alert per position, not per operation");
    assert_eq!(alerts[0].raised, 1);
    assert!(
        k.store.extrinsics().await.unwrap().is_empty(),
        "nothing parked is signed"
    );
    assert_eq!(k.balances(&alice).await, (9_800, 200), "nothing released");
    // Liquidity returns: every one resumes without an operator.
    // Liquidity the contract accounts for (`F-027`): a plain transfer would not raise it.
    k.chain.fund_position(INSTANCE, &chain_usd(), 1_000);
    for _ in 0..80 {
        k.chain.advance(1);
        k.step().await.unwrap();
        if k.balances(&alice).await == (9_800, 0) {
            break;
        }
    }
    for op in &ops {
        assert_eq!(k.state(&alice, op).await, "Settled", "{op}");
        assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(op)), 1);
    }
    assert!(
        open_alerts(&k, "position:").await.is_empty(),
        "the alert cleared"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sn_2_a_fee_shortfall_parks_with_one_alert_and_resumes() {
    let mut k = serving_kit(KitOptions {
        native: 50,
        ..KitOptions::default()
    })
    .await;
    let alice = k.d.party("alice", 1_000).await;
    let a = k.withdrawal(&alice, 10, "a").await;
    let b = k.withdrawal(&alice, 10, "b").await;
    k.step().await.unwrap();
    for op in [&a, &b] {
        assert!(matches!(
            instruction(&k, op).await.state,
            InstructionState::Parked {
                reason: ParkReason::Fees,
                ..
            }
        ));
    }
    assert_eq!(open_alerts(&k, "fees:").await.len(), 1);
    k.chain.set_native(k.lanes[0].account(), 1_000_000);
    // One lane, one extrinsic in flight: they go one after the other, in either order.
    assert_eq!(until(&mut k, &alice, &a, "Settled", 10).await, "Settled");
    assert_eq!(until(&mut k, &alice, &b, "Settled", 10).await, "Settled");
    assert!(open_alerts(&k, "fees:").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_e3_3_an_operation_already_executing_is_never_parked() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    k.step().await.unwrap();
    // The position empties after the extrinsic went out: it is not parked; the layer refuses
    // it, the refusal is reported, and the core releases it as a proved rejection (amendment
    // 0006 item 3) — never back to parking.
    k.chain.set_position(INSTANCE, &chain_usd(), 0);
    k.chain.advance(1);
    k.step().await.unwrap();
    let row = instruction(&k, &op).await;
    assert!(
        matches!(row.state, InstructionState::Refused { .. }),
        "{row:?}"
    );
    k.pass(600_000).await;
    assert_eq!(k.state(&alice, &op).await, "Released");
    assert!(open_alerts(&k, "position:").await.is_empty());
}

// The re-scan's case (`T-016-09`, `AC-D2.1`) and the pruned gap's (`AC-D2.2`) are the chain's
// since amendment 0040: `tests/chain.rs`.

// ------------------------------------------------------------------------------ INV-11

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inv_11_no_report_carries_a_status() {
    let mut k = serving_kit(KitOptions::default()).await;
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    assert_eq!(until(&mut k, &alice, &op, "Settled", 4).await, "Settled");
    fn keys(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    out.push(k.clone());
                    keys(x, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| keys(x, out)),
            _ => {}
        }
    }
    for r in k.store.reports().await.unwrap() {
        if r.kind == "declare" {
            continue;
        }
        let mut ks = Vec::new();
        keys(&r.body, &mut ks);
        for forbidden in ["status", "state", "outcome", "verdict", "settled", "result"] {
            assert!(
                !ks.iter().any(|k| k == forbidden),
                "{} carries {forbidden}",
                r.kind
            );
        }
    }
}
