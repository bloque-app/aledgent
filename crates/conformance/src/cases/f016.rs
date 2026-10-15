// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-016` — Settlement adapter: its registered cases (`F-015` §6, §7).
//!
//! Every case runs a whole deployment in this process ([`settle::testkit::Kit`]): ingress and the
//! in-process core (as `F-009`'s cases do), the settlement adapter (`services/settle`) with its
//! store, and the fake settlement layer, which models finality, reorgs above it, mortal eras,
//! nonces, fees, and the settlement pallet's instances, positions and sequenced inbox. The adapter
//! reaches the core through ingress's router as the principal the settlement configuration names.
//! The core the cases run is the in-process one on both targets: where the decision runs is issue
//! #38's to rule (`F-005`), and nothing here depends on it.
//!
//! Against a local node running the pallet, the same flow is `services/settle/tests/live_node.rs`,
//! ignored by default: this environment cannot run one (`features/F-016-settlement-adapter/
//! NOTES.md`).
//!
//! `AC-E1.4` runs here by the adapter's route to "provably not executed": an instruction that can
//! no longer be signed within its expiry is tombstoned, and the core releases it once the
//! finalised head proves non-inclusion. The route through a proved rejection (a final
//! `WithdrawRefused` record, amendment 0006 item 3) is `F-010`'s case. Every `ERR-*` case asserts its refusal with
//! [`crate::shape::expect_refusal`].

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::future::Future;

use ledger_core::Refusal;
use serde_json::{Value, json};
use settle::store::{InstructionState, NotExecuted, ParkReason, Store};
use settle::testkit::{INSTANCE, Kit, KitOptions, chain_usd};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::Harness;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F016,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-016`'s registered cases.
pub const CASES: &[Case] = &[
    // Errors (SPEC §10).
    service_case!(
        "ERR-UnsupportedLeg",
        err_unsupported_leg__service,
        &["REQ-SA-1", "AC-E4.1"]
    ),
    service_case!(
        "ERR-SettlementLayerUnavailable",
        err_settlement_layer_unavailable__service,
        &["REQ-SA-2", "AC-E4.2"]
    ),
    // Acceptance criteria of the V0 stories.
    service_case!("AC-D2.1", ac_d2_1__service, &["REQ-IN-5", "REQ-IN-6"]),
    service_case!("AC-D2.2", ac_d2_2__service, &["REQ-SA-7"]),
    service_case!(
        "AC-E1.4",
        ac_e1_4__service,
        &["REQ-SA-4", "REQ-SL-4", "REQ-OP-9"]
    ),
    service_case!(
        "AC-E1.5",
        ac_e1_5__service,
        &["REQ-SO-4", "REQ-OP-6", "INV-11"]
    ),
    service_case!("AC-E3.1", ac_e3_1__service, &["REQ-SN-2", "REQ-SA-4"]),
    service_case!("AC-E3.2", ac_e3_2__service, &["REQ-SN-2"]),
    service_case!("AC-E3.3", ac_e3_3__service, &["REQ-OP-6", "REQ-SN-2"]),
    service_case!("AC-E4.1", ac_e4_1__service, &["REQ-SA-1", "REQ-OS-2"]),
    service_case!("AC-E4.2", ac_e4_2__service, &["REQ-SA-2"]),
    // Amendment 0031: pending until `T-027-12` builds the format chosen by the runtime's surface and
    // the layer's confirmation.
    Case::pending(
        "AC-E4.3",
        "ac_e4_3_an_upgrade_that_changes_nothing_signed_keeps_the_adapter_serving__service",
        Layer::Service,
        Feature::F016,
        &["REQ-SA-9"],
    ),
    Case::pending(
        "AC-E4.4",
        "ac_e4_4_a_changed_surface_or_an_unconfirmed_instruction_refuses_to_serve__service",
        Layer::Service,
        Feature::F016,
        &["REQ-SA-9"],
    ),
];

type Result<T = ()> = std::result::Result<T, String>;

fn check(cond: bool, why: impl FnOnce() -> String) -> Result {
    if cond { Ok(()) } else { Err(why()) }
}

/// Run an async case body on a runtime of its own.
fn run<F: Future<Output = Result>>(body: impl FnOnce() -> F) -> Outcome {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return Outcome::Fail(format!("no runtime: {e}")),
    };
    match rt.block_on(body()) {
        Ok(()) => Outcome::Pass,
        Err(why) => Outcome::Fail(why),
    }
}

/// A refusal reply as the command's result.
fn refused(status: u16, v: &Value) -> std::result::Result<Value, Refusal> {
    match serde_json::from_value::<Refusal>(v.clone()) {
        Ok(r) if (400..500).contains(&status) => Err(r),
        _ => Ok(v.clone()),
    }
}

/// A kit whose adapter has taken its first step and serves.
async fn serving(options: KitOptions) -> Result<Kit> {
    let mut k = Kit::start_with(options).await;
    let r = k.step().await.map_err(|e| e.to_string())?;
    check(r.serving, || {
        format!("the adapter refuses: {:?}", k.adapter.status())
    })?;
    Ok(k)
}

async fn step(k: &mut Kit) -> Result {
    k.step().await.map(|_| ()).map_err(|e| e.to_string())
}

/// Rounds (a block, finality, a step) until `op` shows `want`, at most `n`.
async fn until(k: &mut Kit, p: &ingress::testkit::Party, op: &str, want: &str, n: usize) -> Result {
    for _ in 0..n {
        if k.state(p, op).await == want {
            return Ok(());
        }
        k.chain.advance(1);
        step(k).await?;
    }
    let s = k.state(p, op).await;
    check(s == want, || format!("{op}: {s}, expected {want}"))
}

async fn instruction_state(k: &Kit, op: &str) -> Result<InstructionState> {
    k.store
        .instructions()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|r| r.operation == op)
        .map(|r| r.state)
        .ok_or_else(|| format!("no instruction {op}"))
}

async fn open_alerts(k: &Kit, prefix: &str) -> Result<usize> {
    Ok(k.store
        .alerts()
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .filter(|a| a.key.starts_with(prefix) && a.cleared_at.is_none())
        .count())
}

// ------------------------------------------------------------------------------ errors

/// `ERR-UnsupportedLeg` (`REQ-SA-1`): an asset the adapter did not declare, refused at
/// submission.
fn err_unsupported_leg__service(_: &mut dyn Harness) -> Outcome {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build();
    let Ok(rt) = rt else {
        return Outcome::Fail("no runtime".into());
    };
    rt.block_on(async {
        let k = match serving(KitOptions::default()).await {
            Ok(k) => k,
            Err(e) => return Outcome::Fail(e),
        };
        let alice = k.d.party("alice", 1_000).await;
        let (s, v) =
            k.d.call_decided(
                "POST",
                "/v1/withdrawals",
                Some(&alice.token),
                &[],
                Some(json!({ "payer": alice.account, "asset": "EUR", "amount": "5" })),
            )
            .await;
        crate::shape::expect_refusal(refused(s.as_u16(), &v), "ERR-UnsupportedLeg")
    })
}

/// `ERR-SettlementLayerUnavailable` (`REQ-SA-2`): the adapter refuses to serve.
fn err_settlement_layer_unavailable__service(_: &mut dyn Harness) -> Outcome {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build();
    let Ok(rt) = rt else {
        return Outcome::Fail("no runtime".into());
    };
    rt.block_on(async {
        // A runtime not on the adapter's list (DEC-20: a decimals mismatch no longer stops the
        // adapter; it holds the one asset, AC-E4.1).
        let mut k = Kit::start_with(KitOptions {
            fake: settle::chain::fake::FakeConfig {
                spec_version: 999,
                ..Default::default()
            },
            ..KitOptions::default()
        })
        .await;
        match k.step().await {
            Ok(r) if !r.serving => {}
            other => return Outcome::Fail(format!("expected a refusing adapter: {other:?}")),
        }
        let alice = k.d.party("alice", 1_000).await;
        let (s, v) = k.withdraw(&alice, 5, "w").await;
        crate::shape::expect_refusal(refused(s, &v), "ERR-SettlementLayerUnavailable")
    })
}

// ------------------------------------------------------------------------------ AC-D2

/// `AC-D2.1`: a comparison of the layer's records with the ingested observations is available,
/// and a difference is an incident — here a record the core's copy lacks.
fn ac_d2_1__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = serving(KitOptions::default()).await?;
        let from = [0x77; 32];
        k.chain.mint(from, &chain_usd(), 100);
        for _ in 0..3 {
            k.chain.deposit(from, INSTANCE, [0x42; 32], &chain_usd(), 1);
            k.chain.produce();
        }
        k.chain.finalize_all();
        k.adapter.drop_report_of(1);
        step(&mut k).await?;
        let found = k.adapter.rescan(0, 3).await.map_err(|e| e.to_string())?;
        let missing: Vec<u64> = found.core.missing.iter().map(|m| m.seq).collect();
        check(missing == vec![1], || format!("missing {missing:?}"))?;
        check(open_alerts(&k, "incident:missing:").await? == 1, || {
            "no incident raised".into()
        })?;
        let incidents = k.core_settlement(|s| s.incidents().len()).unwrap_or(0);
        check(incidents >= 1, || "the core raised no incident".into())
    })
}

/// `AC-D2.2`: a range pruned before it was read is an explicit gap, visible to operators.
fn ac_d2_2__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = serving(KitOptions::default()).await?;
        let from = [0x77; 32];
        k.chain.mint(from, &chain_usd(), 100);
        for _ in 0..5 {
            k.chain.deposit(from, INSTANCE, [0x42; 32], &chain_usd(), 1);
            k.chain.produce();
        }
        k.chain.prune(INSTANCE, 3);
        k.chain.advance(1);
        step(&mut k).await?;
        let gaps = k.store.gaps().await.map_err(|e| e.to_string())?;
        check(
            gaps.len() == 1 && gaps[0].from == 0 && gaps[0].to == 3,
            || format!("adapter gaps {gaps:?}"),
        )?;
        check(open_alerts(&k, "gap:").await? == 1, || {
            "no gap alert".into()
        })?;
        let core = k.core_settlement(|s| s.gaps().to_vec()).unwrap_or_default();
        check(
            core.len() == 1 && core[0].from == 0 && core[0].to == 3,
            || format!("core gaps {core:?}"),
        )
    })
}

// ------------------------------------------------------------------------------ AC-E1

/// `AC-E1.4`: execution refused before it could take effect — the position does not cover
/// it, so it is never signed, its expiry passes, and value returns to available.
fn ac_e1_4__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = serving(KitOptions {
            position: 0,
            ..KitOptions::default()
        })
        .await?;
        let alice = k.d.party("alice", 1_000).await;
        let op = k.withdrawal(&alice, 10, "w").await;
        until(&mut k, &alice, &op, "Released", 60).await?;
        let s = instruction_state(&k, &op).await?;
        check(
            matches!(
                s,
                InstructionState::NotExecuted {
                    proof: NotExecuted::NeverSigned { .. }
                }
            ),
            || format!("{s:?}"),
        )?;
        check(k.chain.extrinsics_for(&Kit::chain_ref(&op)) == 0, || {
            "an extrinsic was produced".into()
        })?;
        let b = k.balances(&alice).await;
        check(b == (1_000, 0), || format!("balances {b:?}"))
    })
}

/// `AC-E1.5`: the outcome is unknown (block production stopped after the broadcast): the
/// operation stays non-terminal, nothing is released, and nothing is reported for it.
fn ac_e1_5__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = serving(KitOptions::default()).await?;
        // The deadline is taken when the case passes time, and only then.
        k.drive_ticks();
        let alice = k.d.party("alice", 1_000).await;
        let op = k.withdrawal(&alice, 10, "w").await;
        step(&mut k).await?;
        k.chain.halt(true);
        for _ in 0..3 {
            step(&mut k).await?;
        }
        let reports = k.store.reports().await.map_err(|e| e.to_string())?;
        check(
            !reports
                .iter()
                .any(|r| r.kind == "observe" || r.kind == "check"),
            || "something was reported for an unknown outcome".into(),
        )?;
        k.pass(600_000).await;
        let s = k.state(&alice, &op).await;
        check(s == "Needs-Review", || format!("state {s}"))?;
        let b = k.balances(&alice).await;
        check(b == (990, 10), || format!("balances {b:?}"))
    })
}

// ------------------------------------------------------------------------------ AC-E3

async fn twenty_parked() -> Result<(Kit, ingress::testkit::Party, Vec<String>)> {
    let mut k = serving(KitOptions {
        position: 5,
        expiry_window: 200,
        ..KitOptions::default()
    })
    .await?;
    let alice = k.d.party("alice", 10_000).await;
    let mut ops = Vec::new();
    for i in 0..20 {
        ops.push(k.withdrawal(&alice, 10, &format!("w{i}")).await);
    }
    step(&mut k).await?;
    step(&mut k).await?;
    for op in &ops {
        let s = instruction_state(&k, op).await?;
        check(
            matches!(
                s,
                InstructionState::Parked {
                    reason: ParkReason::Liquidity,
                    ..
                }
            ),
            || format!("{op}: {s:?}"),
        )?;
    }
    Ok((k, alice, ops))
}

/// `AC-E3.1`: twenty withdrawals on a short position park, release nothing, and raise one
/// alert for the position.
fn ac_e3_1__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let (k, alice, _) = twenty_parked().await?;
        check(open_alerts(&k, "position:").await? == 1, || {
            "not one alert per position".into()
        })?;
        let raised: u64 = k
            .store
            .alerts()
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .filter(|a| a.key.starts_with("position:"))
            .map(|a| a.raised)
            .sum();
        check(raised == 1, || format!("raised {raised} times"))?;
        let exts = k.store.extrinsics().await.map_err(|e| e.to_string())?;
        check(exts.is_empty(), || "a parked instruction was signed".into())?;
        let b = k.balances(&alice).await;
        check(b == (9_800, 200), || format!("balances {b:?}"))
    })
}

/// `AC-E3.2`: liquidity restored, every parked operation resumes without an operator.
fn ac_e3_2__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let (mut k, alice, ops) = twenty_parked().await?;
        // Liquidity the contract accounts for (`F-027`): a plain transfer would not raise it.
        k.chain.fund_position(INSTANCE, &chain_usd(), 1_000);
        for _ in 0..80 {
            k.chain.advance(1);
            step(&mut k).await?;
            if k.balances(&alice).await == (9_800, 0) {
                break;
            }
        }
        for op in &ops {
            let s = k.state(&alice, op).await;
            check(s == "Settled", || format!("{op}: {s}"))?;
        }
        check(k.chain.duplicated_references().is_empty(), || {
            "a second extrinsic".into()
        })?;
        check(open_alerts(&k, "position:").await? == 0, || {
            "the alert did not clear".into()
        })
    })
}

/// `AC-E3.3`: an operation already executing is never parked. Here the layer took the
/// withdrawal and refused it (the position was drained after signing): a proved rejection, which
/// since amendment 0006 item 3 is released at once rather than sent to `Needs-Review`. Either way,
/// nothing parks it and no liquidity alert is raised for it.
fn ac_e3_3__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = serving(KitOptions::default()).await?;
        // The deadline is taken when the case passes time, and only then.
        k.drive_ticks();
        let alice = k.d.party("alice", 1_000).await;
        let op = k.withdrawal(&alice, 10, "w").await;
        step(&mut k).await?;
        k.chain.set_position(INSTANCE, &chain_usd(), 0);
        k.chain.advance(1);
        step(&mut k).await?;
        let s = instruction_state(&k, &op).await?;
        check(matches!(s, InstructionState::Refused { .. }), || {
            format!("{s:?}")
        })?;
        k.pass(600_000).await;
        let s = k.state(&alice, &op).await;
        check(s == "Released", || format!("state {s}"))?;
        check(open_alerts(&k, "position:").await? == 0, || {
            "an executing operation was parked".into()
        })
    })
}

// ------------------------------------------------------------------------------ AC-E4

/// `AC-E4.1`: an asset the adapter does not support is refused at submission, before any
/// reservation; so is one the operator's registration through `CTR-OPS` no longer declares.
fn ac_e4_1__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let k = serving(KitOptions::default()).await?;
        let alice = k.d.party("alice", 1_000).await;
        let (s, v) =
            k.d.call_decided(
                "POST",
                "/v1/withdrawals",
                Some(&alice.token),
                &[],
                Some(json!({ "payer": alice.account, "asset": "EUR", "amount": "5" })),
            )
            .await;
        check(
            s.as_u16() == 422 && v["error"] == "ERR-UnsupportedLeg",
            || format!("{s} {v}"),
        )?;
        let b = k.balances(&alice).await;
        check(b == (1_000, 0), || format!("balances {b:?}"))?;
        let op = Some(k.d.operator.token.as_str());
        let (s, v) =
            k.d.call(
                "POST",
                "/v1/ops/operator-grants",
                op,
                &[],
                Some(json!({
                    "principal": k.d.operator.identity,
                    "kinds": ["declare_adapter_support"],
                    "justification": "conformance",
                })),
            )
            .await;
        check(s.as_u16() == 200, || format!("{s} {v}"))?;
        let (s, v) =
            k.d.call(
                "POST",
                "/v1/ops/adapter-support",
                op,
                &[],
                Some(
                    json!({ "legs": [{ "kind": "issue", "asset": "USD" }], "justification": "c" }),
                ),
            )
            .await;
        check(s.as_u16() == 200, || format!("{s} {v}"))?;
        let (s, v) = k.withdraw(&alice, 5, "redeem-usd").await;
        check(s == 422 && v["error"] == "ERR-UnsupportedLeg", || {
            format!("{s} {v}")
        })
    })
}

/// `AC-E4.2`: an adapter whose network is not the declared one refuses to serve — a wrong
/// genesis, and a runtime not on its list. (Decimals that differ from the registry's hold the one
/// asset, not the adapter: `DEC-20`, `F-029`.)
fn ac_e4_2__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        for options in [
            KitOptions {
                fake: settle::chain::fake::FakeConfig {
                    genesis: [0x01; 32],
                    ..Default::default()
                },
                ..KitOptions::default()
            },
            KitOptions {
                fake: settle::chain::fake::FakeConfig {
                    spec_version: 999,
                    ..Default::default()
                },
                ..KitOptions::default()
            },
        ] {
            let mut k = Kit::start_with(options).await;
            let r = k.step().await.map_err(|e| e.to_string())?;
            check(!r.serving, || "the adapter served".into())?;
            let alice = k.d.party("alice", 1_000).await;
            let (s, v) = k.withdraw(&alice, 5, "w").await;
            check(
                s == 422 && v["error"] == "ERR-SettlementLayerUnavailable",
                || format!("{s} {v}"),
            )?;
            check(k.chain.pool_len() == 0, || "something was broadcast".into())?;
        }
        Ok(())
    })
}

// ------------------------------------------------------------------------------ F-029 (amendment 0011)
//
// `US-F5`'s cases (`crate::cases::f029` registers them). They live here because they run the
// adapter over its fake chain, with this module's kit helpers.

/// Whether a reply is the named refusal, in the catalogue's shape.
fn refusal_is(status: u16, v: &Value, id: &str) -> Result {
    crate::shape::check_refusal(
        &crate::spec::Catalogue::checked_in(),
        refused(status, v),
        id,
    )
}

/// TSTA at `Here(101)`, a live asset as Kreivo holds one (`TSTA`, 2 decimals), mapped by the
/// adapter; registered book-only, with an issuance grant held by the operator.
async fn book_only_tsta(k: &mut Kit) -> Result<settle::chain::ChainAsset> {
    use ledger_core::settlement::assets::{LayerAssetFacts, LayerAssetStatus};
    let tsta = settle::chain::ChainAsset("Here(101)".into());
    k.chain.set_metadata(
        &tsta,
        Some(LayerAssetFacts {
            symbol: b"TSTA".to_vec(),
            decimals: 2,
            status: LayerAssetStatus::Live,
        }),
    );
    k.reconfigure(|c| {
        c.assets.push(settle::config::AssetBinding {
            asset: "TSTA".into(),
            chain: tsta.clone(),
        })
    });
    step(k).await?;
    let op = k.d.operator.token.clone();
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/ops/operator-grants",
            Some(&op),
            &[],
            Some(json!({
                "principal": k.d.operator.identity,
                "kinds": ["register_asset"],
                "justification": "t",
            })),
        )
        .await;
    check(s.as_u16() == 200, || format!("operator grant: {v}"))?;
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/ops/assets",
            Some(&op),
            &[("client-key", "register-tsta")],
            Some(json!({
                "id": "TSTA", "precision": 2, "kind": "stable",
                "review": "cd".repeat(32), "justification": "CR-8: TSTA/2, book-only",
            })),
        )
        .await;
    check(s.as_u16() == 200, || format!("register_asset: {v}"))?;
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/grants",
            Some(&op),
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
    check(s.as_u16() == 200, || format!("issuance grant: {v}"))?;
    Ok(tsta)
}

async fn tsta_call(k: &Kit, path: &str, key: &str, body: Value) -> (u16, Value) {
    let (s, v) =
        k.d.call_decided(
            "POST",
            path,
            Some(&k.d.operator.token),
            &[("client-key", key)],
            Some(body),
        )
        .await;
    (s.as_u16(), v)
}

async fn issue_tsta(k: &Kit, account: &str, amount: u64, key: &str) -> (u16, Value) {
    tsta_call(
        k,
        "/v1/issuances",
        key,
        json!({ "asset": "TSTA", "destination": account, "amount": amount.to_string() }),
    )
    .await
}

async fn bind_tsta(k: &Kit, key: &str) -> (u16, Value) {
    tsta_call(
        k,
        "/v1/ops/assets/TSTA/bind",
        key,
        json!({ "location": "Here(101)", "justification": "CR-9: bind TSTA on Kreivo" }),
    )
    .await
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
async fn until_binding(k: &mut Kit, want: &str, n: usize) -> Result {
    for _ in 0..n {
        if tsta_binding(k).await == want {
            return Ok(());
        }
        k.chain.advance(1);
        step(k).await?;
    }
    let b = tsta_binding(k).await;
    check(b == want, || format!("TSTA is {b}, expected {want}"))
}

async fn anchoring() -> Result<Kit> {
    serving(KitOptions {
        anchoring: true,
        ..KitOptions::default()
    })
    .await
}

async fn withdraw_tsta(k: &Kit, p: &ingress::testkit::Party, key: &str) -> (u16, Value) {
    let (s, v) =
        k.d.call_decided(
            "POST",
            "/v1/withdrawals",
            Some(&p.token),
            &[("client-key", key)],
            Some(json!({ "payer": p.account, "asset": "TSTA", "amount": "10" })),
        )
        .await;
    (s.as_u16(), v)
}

/// `AC-F5.1` (`REQ-AS-7`): a book-only asset issues and moves in the book, and its external leg
/// is refused at submission with `ERR-UnsupportedLeg`.
pub(crate) fn ac_f5_1__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring().await?;
        book_only_tsta(&mut k).await?;
        let alice = k.d.party("alice", 0).await;
        let account = alice.account.clone().unwrap_or_default();
        let (s, v) = issue_tsta(&k, &account, 500, "i1").await;
        check(s == 200, || format!("issue: {v}"))?;
        let (s, v) = withdraw_tsta(&k, &alice, "w").await;
        refusal_is(s, &v, "ERR-UnsupportedLeg")
    })
}

/// `AC-F5.2` (`REQ-AS-8`): a layer whose decimals field is not the registry's precision, or that
/// has no metadata, refuses the binding with `ERR-UnsupportedLeg`; no name is read.
pub(crate) fn ac_f5_2__service(_: &mut dyn Harness) -> Outcome {
    use ledger_core::settlement::assets::{LayerAssetFacts, LayerAssetStatus};
    run(|| async {
        let mut k = anchoring().await?;
        let tsta = book_only_tsta(&mut k).await?;
        let lease = k.adapter.config().status_lease;
        for (facts, key) in [
            (
                Some(LayerAssetFacts {
                    symbol: b"TSTA".to_vec(),
                    decimals: 6,
                    status: LayerAssetStatus::Live,
                }),
                "d6",
            ),
            (
                Some(LayerAssetFacts {
                    symbol: b"TSTA/2".to_vec(),
                    decimals: 2,
                    status: LayerAssetStatus::Live,
                }),
                "name",
            ),
            (None, "none"),
        ] {
            k.chain.set_metadata(&tsta, facts);
            k.d.clock.advance(lease);
            step(&mut k).await?;
            let (s, v) = bind_tsta(&k, key).await;
            refusal_is(s, &v, "ERR-UnsupportedLeg")?;
        }
        let b = tsta_binding(&k).await;
        check(b == "book_only", || format!("TSTA is {b}"))
    })
}

/// `AC-F5.3` and `AC-F5.4` (`REQ-AS-8`, `NFR-9`): a binding one minor unit off is refused on the
/// layer and the asset is book-only again; at zero divergence it is bound once final, its
/// issuance refused while pending, and its external legs then served.
async fn bind_through_the_contract(k: &mut Kit) -> Result<ingress::testkit::Party> {
    let tsta = book_only_tsta(k).await?;
    let alice = k.d.party("alice", 0).await;
    let account = alice.account.clone().unwrap_or_default();
    let (s, v) = issue_tsta(k, &account, 500, "i1").await;
    check(s == 200, || format!("issue: {v}"))?;
    k.chain.top_up_position(INSTANCE, &tsta, 499);
    let (s, v) = bind_tsta(k, "b1").await;
    check(s == 200, || format!("bind: {v}"))?;
    let (s, v) = issue_tsta(k, &account, 1, "i2").await;
    refusal_is(s, &v, "ERR-NotAuthorised")?;
    until_binding(k, "book_only", 40).await?;
    let (s, v) = issue_tsta(k, &account, 1, "i3").await;
    check(s == 200, || {
        format!("issuance resumes after a refusal: {v}")
    })?;
    k.chain.top_up_position(INSTANCE, &tsta, 2);
    let (s, v) = bind_tsta(k, "b2").await;
    check(s == 200, || format!("bind again: {v}"))?;
    Ok(alice)
}

/// `AC-F5.3` (`REQ-AS-8`).
pub(crate) fn ac_f5_3__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring().await?;
        bind_through_the_contract(&mut k).await.map(|_| ())
    })
}

/// `AC-F5.4` (`REQ-AS-8`, `NFR-9`).
pub(crate) fn ac_f5_4__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring().await?;
        let alice = bind_through_the_contract(&mut k).await?;
        until_binding(&mut k, "bound", 40).await?;
        let (s, v) = withdraw_tsta(&k, &alice, "w-bound").await;
        check(s == 200, || format!("a withdrawal of a bound asset: {v}"))
    })
}

/// `AC-F5.5` (`REQ-AS-9`): once bound, a change of the layer's metadata holds nothing and refuses
/// nothing. `USD` is bound from the test genesis.
pub(crate) fn ac_f5_5__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = serving(KitOptions::default()).await?;
        let alice = k.d.party("alice", 1_000).await;
        k.chain.set_decimals(&chain_usd(), ingress::testkit::USD, 6);
        step(&mut k).await?;
        let (s, v) = k.withdraw(&alice, 10, "after").await;
        check(s == 200, || {
            format!("withdraw after a metadata change: {v}")
        })
    })
}
