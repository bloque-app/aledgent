// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-017` — Anchoring and disclosure: its registered cases (`F-015` §6, §7).
//!
//! Every service case runs a whole deployment in this process ([`settle::testkit::Kit`] with
//! `anchoring`): ingress and the in-process core, which holds the test clerk key, the settlement
//! adapter with its anchor key, and the fake settlement layer, which runs the contract's `anchor`
//! call (strict order, a replay of the latest a no-op, a conflict or an older number refused) and
//! keeps every anchor in its blocks' history. The core is
//! the in-process one: where it runs is issue #38's to rule (`F-005`), and nothing here depends
//! on it. Against a local node the same flow is `services/settle/tests/live_node.rs`, ignored by
//! default (`features/F-016-settlement-adapter/NOTES.md`).
//!
//! The core ticks only when a case passes time, and every wait is on a condition with a generous
//! deadline (issue #85): the replica's own ticker, left running, took checkpoints at whatever
//! moment it ran, so their number and what each covered depended on timing.
//!
//! The build-gate case of `INV-17` checks the shapes an anchored value can take, with no
//! deployment. Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::future::Future;

use ingress::testkit::{ANCHOR_INTERVAL_MS, PATIENCE, Party, USD};
use ledger_core::Refusal;
use ledger_core::anchor::{AnchorIncident, AnchorInstruction, AnchorStore, CheckpointStatus};
use ledger_core::export::Leaves;
use serde_json::{Value, json};
use settle::chain::AnchorCall;
use settle::testkit::{INSTANCE, Kit, KitOptions, chain_usd};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::Harness;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F017,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-017`'s registered cases.
pub const CASES: &[Case] = &[
    // Invariants (SPEC §9), at the layer each case exercises.
    Case::run(
        "INV-17",
        "inv_17_no_anchored_value_carries_personal_data__build_gate",
        Layer::BuildGate,
        Feature::F017,
        Port::Build,
        &["REQ-PV-3"],
        inv_17_no_anchored_value_carries_personal_data__build_gate,
    ),
    service_case!(
        "INV-19",
        inv_19_an_anchor_commits_to_a_stated_sequence_and_nothing_else__service,
        &["REQ-PR-1"]
    ),
    // Errors (SPEC §10).
    service_case!(
        "ERR-DisclosureUnsupported",
        err_disclosure_unsupported__service,
        &[]
    ),
    service_case!("ERR-DisclosureFalse", err_disclosure_false__service, &[]),
    service_case!("ERR-NotDisclosable", err_not_disclosable__service, &[]),
    service_case!("ERR-AnchorNotFinal", err_anchor_not_final__service, &[]),
    // Acceptance criteria of the V0 stories.
    service_case!("AC-I1.1", ac_i1_1__service, &["REQ-PR-2", "INV-17"]),
    service_case!("AC-J1.1", ac_j1_1__service, &["REQ-PR-1"]),
    service_case!("AC-J1.2", ac_j1_2__service, &["REQ-PR-7", "INV-19"]),
    service_case!("AC-J1.3", ac_j1_3__service, &["REQ-PR-8"]),
    service_case!("AC-J1.4", ac_j1_4__service, &["REQ-PR-9"]),
    service_case!("AC-J1.5", ac_j1_5__service, &["REQ-PR-10"]),
    service_case!("AC-J3.1", ac_j3_1__service, &["REQ-PR-5"]),
    service_case!("AC-J3.2", ac_j3_2__service, &["REQ-PR-5", "NFR-9"]),
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

/// Run an async case body that ends in an [`Outcome`] of its own (the refusal cases).
fn run_outcome<F: Future<Output = Outcome>>(body: impl FnOnce() -> F) -> Outcome {
    match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(body()),
        Err(e) => Outcome::Fail(format!("no runtime: {e}")),
    }
}

/// A refusal reply as the command's result.
fn refused(status: u16, v: &Value) -> std::result::Result<Value, Refusal> {
    match serde_json::from_value::<Refusal>(v.clone()) {
        Ok(r) if (400..500).contains(&status) => Err(r),
        _ => Ok(v.clone()),
    }
}

/// A kit whose adapter anchors, has taken its first step and serves. The core ticks only when
/// the case passes time ([`pass`]), so the case says which checkpoints exist and what each covers
/// (issue #85).
pub(super) async fn anchoring_kit() -> Result<Kit> {
    let mut k = Kit::start_with(KitOptions {
        anchoring: true,
        ..KitOptions::default()
    })
    .await;
    k.d.drive_ticks();
    // The adapter has not run a step yet: a fresh one takes the new replica's surface.
    k.restart();
    let r = k.step().await.map_err(|e| e.to_string())?;
    check(r.serving, || {
        format!("the adapter refuses: {:?}", k.adapter.status())
    })?;
    Ok(k)
}

pub(super) fn anchoring<T>(k: &Kit, f: impl FnOnce(&AnchorStore) -> T) -> Result<T> {
    k.d.core
        .with_ledger(|l| f(l.state().anchoring()))
        .ok_or_else(|| "the core is down".to_owned())
}

/// Let `ms` of the test clock pass, and wait until the core has ticked at the new time: the
/// checkpoint that interval makes is then taken (issue #85).
///
/// Not [`Kit::pass`]: it logs one tick and does not check that the tick was decided. With the
/// replica's ticker running, that call logged nothing while the ticker held a tick from before
/// the clock moved. The checkpoint was then taken by a later tick, after the case had already
/// looked for it (`AC-J3.1` on CI: `checkpoint 3 not final: [(1, Final), (2, Final)]`).
pub(super) async fn pass(k: &Kit, ms: u64) -> Result {
    k.d.pass(ms, PATIENCE).await
}

/// Rounds (a block, finality, a step) until no checkpoint is in flight and at least `number` is
/// final, at most `n`. Every adapter call waits for the core's decision; each round also waits
/// until every row is decided, so the count of rounds does not depend on the forwarder's pace.
pub(super) async fn until_final(k: &mut Kit, number: u64, n: usize) -> Result {
    for _ in 0..n {
        let done = anchoring(k, |s| {
            s.in_flight().is_none() && s.latest_final().is_some_and(|c| c.number >= number)
        })?;
        if done {
            return Ok(());
        }
        k.chain.advance(1);
        k.step().await.map_err(|e| e.to_string())?;
        check(k.d.quiesce(PATIENCE).await, || "rows undecided".into())?;
    }
    let s = anchoring(k, |s| {
        s.checkpoints()
            .map(|c| (c.number, c.status))
            .collect::<Vec<_>>()
    })?;
    Err(format!("checkpoint {number} not final: {s:?}"))
}

async fn transfer(k: &Kit, from: &Party, to: &Party, amount: u64) -> Result {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = format!(
        "f017-transfer-{}",
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/transfers",
            Some(&from.token),
            &[("client-key", key.as_str())],
            Some(json!({ "legs": [{
                "from": from.account, "to": to.account, "asset": USD, "amount": amount.to_string()
            }] })),
        )
        .await;
    check(s.is_success(), || format!("transfer: {s} {v}"))?;
    check(k.d.quiesce(PATIENCE).await, || {
        "the transfer was not decided".into()
    })
}

async fn disclose(k: &Kit, who: &Party, body: Value) -> (u16, Value) {
    let (s, v) =
        k.d.call("POST", "/v1/disclosures", Some(&who.token), &[], Some(body))
            .await;
    (s.as_u16(), v)
}

fn holds(subject: &Option<String>, amount: u64) -> Value {
    json!({
        "subject": subject,
        "recipient_reference": hex32([0x11; 32]),
        "assertion": { "shape": "holds_at_least", "asset": USD, "amount": amount.to_string(), "kind": "available" },
    })
}

fn hex32(b: [u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The kernel's leaves as they stood right after entry `s`, from the core's delta log.
fn leaves_through(k: &Kit, s: u64) -> Result<Leaves> {
    k.d.core
        .with_ledger(|l| {
            let store = l.book().store();
            use ledger_kernel::store::LeafStore;
            let last = store.last_committed().ok().flatten()?;
            let mut out = None;
            for c in 0..=last {
                let leaves = Leaves::from_log(store, c).ok()?;
                if leaves.next_posting() > s + 1 {
                    break;
                }
                if leaves.next_posting() == s + 1 {
                    out = Some(leaves);
                }
            }
            out
        })
        .flatten()
        .ok_or_else(|| format!("no prefix of the log ends at entry {s}"))
}

// ------------------------------------------------------------------------------ invariants

/// `INV-17` at the build gate (`REQ-PV-3`, `REQ-PR-2`): an anchored value is a number and 32
/// bytes, in every form it takes — the core's instruction, the adapter's read of it, the chain
/// call — so there is nowhere for personal data to go. The core's type is fixed by a `const`
/// assertion in `ledger-core`; this case checks the rest.
fn inv_17_no_anchored_value_carries_personal_data__build_gate(_: &mut dyn Harness) -> Outcome {
    let body = || -> Result {
        check(std::mem::size_of::<AnchorInstruction>() == 40, || {
            "the anchor instruction is not a u64 and 32 bytes".into()
        })?;
        let read = serde_json::to_value(ingress::settlement::AnchorInstructionBody {
            number: 1,
            commitment: hex32([0; 32]),
            acknowledged: false,
        })
        .map_err(|e| e.to_string())?;
        let keys: Vec<&String> = read
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        check(keys == ["acknowledged", "commitment", "number"], || {
            format!("the adapter's read carries {keys:?}")
        })?;
        let call = serde_json::to_value(AnchorCall {
            instance: 1,
            number: 1,
            commitment: [0; 32],
        })
        .map_err(|e| e.to_string())?;
        let keys: Vec<&String> = call
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        check(keys == ["commitment", "instance", "number"], || {
            format!("the chain call carries {keys:?}")
        })?;
        // The inbox event the layer records: its two fields, and nothing else can be named.
        let e = settle::chain::InboxEvent::Anchored {
            number: 1,
            commitment: [0; 32],
        };
        let settle::chain::InboxEvent::Anchored {
            number: _,
            commitment: _,
        } = e
        else {
            return Err("not an anchor".into());
        };
        Ok(())
    };
    match body() {
        Ok(()) => Outcome::Pass,
        Err(why) => Outcome::Fail(why),
    }
}

/// `INV-19` (`REQ-PR-1`, `REQ-PR-7`): a final checkpoint commits to every entry up to and
/// including its sequence and to nothing else: a rebuild of the kernel from the core's delta log
/// up to that entry gives its root; entries after it change nothing it holds; and the layer
/// stores exactly its commitment.
fn inv_19_an_anchor_commits_to_a_stated_sequence_and_nothing_else__service(
    _: &mut dyn Harness,
) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        let alice = k.d.party("alice", 1_000).await;
        let bob = k.d.party("bob", 0).await;
        transfer(&k, &alice, &bob, 30).await?;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 1, 12).await?;
        let c = anchoring(&k, |s| s.latest_final().copied())?.ok_or("no final checkpoint")?;
        let leaves = leaves_through(&k, c.entry_sequence)?;
        check(leaves.root() == c.kernel_root, || {
            "the rebuilt root is not the checkpoint's".into()
        })?;
        // `v3` (`T-037-03`): the checkpoint's own construction and code binding.
        check(
            verify::checkpoint_commitment(
                c.construction,
                c.binding.as_ref(),
                INSTANCE,
                c.number,
                c.entry_sequence,
                &leaves.root(),
            ) == c.commitment,
            || "the commitment does not follow from the rebuild".into(),
        )?;
        check(
            k.chain.anchor_at_finalized(INSTANCE, c.number).map(|a| a.0) == Some(c.commitment),
            || "the layer does not hold the commitment".into(),
        )?;
        transfer(&k, &bob, &alice, 5).await?;
        let after = anchoring(&k, |s| s.checkpoint(c.number).copied())?;
        check(after == Some(c), || {
            "a later entry changed the checkpoint".into()
        })?;
        check(
            leaves_through(&k, c.entry_sequence)?.root() == c.kernel_root,
            || "a later entry changed the anchored prefix".into(),
        )
    })
}

// ------------------------------------------------------------------------------ errors

/// `ERR-DisclosureUnsupported`: a shape aledgent does not make.
fn err_disclosure_unsupported__service(_: &mut dyn Harness) -> Outcome {
    run_outcome(|| async {
        let mut k = match anchoring_kit().await {
            Ok(k) => k,
            Err(e) => return Outcome::Fail(e),
        };
        let alice = k.d.party("alice", 1_000).await;
        if let Err(e) = pass(&k, ANCHOR_INTERVAL_MS).await {
            return Outcome::Fail(e);
        }
        if let Err(e) = until_final(&mut k, 1, 12).await {
            return Outcome::Fail(e);
        }
        let (s, v) = disclose(
            &k,
            &alice,
            json!({
                "subject": alice.account,
                "recipient_reference": hex32([0x11; 32]),
                "assertion": { "shape": "solvent", "asset": USD },
            }),
        )
        .await;
        crate::shape::expect_refusal(refused(s, &v), "ERR-DisclosureUnsupported")
    })
}

/// `ERR-DisclosureFalse` (`REQ-DS-4`): a claim the book shows false at the checkpoint.
fn err_disclosure_false__service(_: &mut dyn Harness) -> Outcome {
    run_outcome(|| async {
        let mut k = match anchoring_kit().await {
            Ok(k) => k,
            Err(e) => return Outcome::Fail(e),
        };
        let alice = k.d.party("alice", 1_000).await;
        if let Err(e) = pass(&k, ANCHOR_INTERVAL_MS).await {
            return Outcome::Fail(e);
        }
        if let Err(e) = until_final(&mut k, 1, 12).await {
            return Outcome::Fail(e);
        }
        let (s, v) = disclose(&k, &alice, holds(&alice.account, 1_001)).await;
        crate::shape::expect_refusal(refused(s, &v), "ERR-DisclosureFalse")
    })
}

/// `ERR-NotDisclosable` (`REQ-DS-5`): neither the subject nor an authority it granted.
fn err_not_disclosable__service(_: &mut dyn Harness) -> Outcome {
    run_outcome(|| async {
        let mut k = match anchoring_kit().await {
            Ok(k) => k,
            Err(e) => return Outcome::Fail(e),
        };
        let alice = k.d.party("alice", 1_000).await;
        let mallory = k.d.party("mallory", 0).await;
        if let Err(e) = pass(&k, ANCHOR_INTERVAL_MS).await {
            return Outcome::Fail(e);
        }
        if let Err(e) = until_final(&mut k, 1, 12).await {
            return Outcome::Fail(e);
        }
        let (s, v) = disclose(&k, &mallory, holds(&alice.account, 1)).await;
        crate::shape::expect_refusal(refused(s, &v), "ERR-NotDisclosable")
    })
}

/// `ERR-AnchorNotFinal` (`REQ-PR-8`): a checkpoint submitted but not final cannot be cited.
fn err_anchor_not_final__service(_: &mut dyn Harness) -> Outcome {
    run_outcome(|| async {
        let mut k = match anchoring_kit().await {
            Ok(k) => k,
            Err(e) => return Outcome::Fail(e),
        };
        let alice = k.d.party("alice", 1_000).await;
        if let Err(e) = pass(&k, ANCHOR_INTERVAL_MS).await {
            return Outcome::Fail(e);
        }
        // The adapter takes and submits it, and the layer has not produced a block since.
        if let Err(e) = k.step().await {
            return Outcome::Fail(e.to_string());
        }
        let status = anchoring(&k, |s| s.checkpoint(1).map(|c| c.status))
            .ok()
            .flatten();
        if status != Some(CheckpointStatus::Submitted) {
            return Outcome::Fail(format!("expected a submitted checkpoint, found {status:?}"));
        }
        let mut body = holds(&alice.account, 1);
        body["checkpoint"] = json!(1);
        let (s, v) = disclose(&k, &alice, body).await;
        crate::shape::expect_refusal(refused(s, &v), "ERR-AnchorNotFinal")
    })
}

// ------------------------------------------------------------------------------ AC-I1, AC-J1

/// `AC-I1.1` (`REQ-PV-2`, `REQ-PR-2`): the public record of an anchor is its number and its
/// commitment. Neither the anchor's inbox entry nor the stored anchor holds an account, an
/// identity, an amount, an asset or a sequence of the book.
fn ac_i1_1__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        let alice = k.d.party("alice", 123_456).await;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 1, 12).await?;
        let c = anchoring(&k, |s| s.latest_final().copied())?.ok_or("no final checkpoint")?;
        // The entries, rebuilt from the contract's events at the finalized head (amendment 0040).
        let mut anchors = 0;
        for (_, e) in k.chain.finalized_entries(INSTANCE) {
            let settle::chain::InboxEvent::Anchored { number, commitment } = e.event else {
                continue;
            };
            anchors += 1;
            check(number == c.number && commitment == c.commitment, || {
                format!("anchor {number} is not the checkpoint's")
            })?;
            // The stored bytes (the settlement contract's `Anchored` entry, `F-027`): the block
            // number, the variant, the anchor's reference, the number, 32 bytes. The reference is
            // a digest of the deployment's instance and the anchor's number, nothing else.
            check(e.raw.len() == 4 + 1 + 32 + 8 + 32, || {
                format!("the anchor's entry is {} bytes", e.raw.len())
            })?;
            let reference = ledger_core::settlement::envelope::anchor_reference(INSTANCE, c.number);
            check(e.raw[5..37] == reference, || {
                "the anchor's reference is not its instance's and number's".into()
            })?;
            for secret in [
                alice.account.clone().unwrap_or_default(),
                alice.identity.clone(),
                "123456".to_owned(),
                USD.to_owned(),
            ] {
                check(!String::from_utf8_lossy(&e.raw).contains(&secret), || {
                    format!("the anchor's entry holds {secret}")
                })?;
            }
        }
        check(anchors == 1, || format!("{anchors} anchors in the inbox"))
    })
}

/// `AC-J1.1` (`REQ-PR-1`): with ticks, a checkpoint appears within one interval when entries
/// exist, and none without.
fn ac_j1_1__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        // No entry: no checkpoint, however long.
        pass(&k, ANCHOR_INTERVAL_MS * 3).await?;
        let n = anchoring(&k, |s| s.checkpoints().count())?;
        check(n == 0, || format!("{n} checkpoints without an entry"))?;
        let alice = k.d.party("alice", 10).await;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 1, 12).await?;
        let first = anchoring(&k, |s| s.latest_final().copied())?.ok_or("no checkpoint")?;
        // No new entry: nothing after another interval.
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        let n = anchoring(&k, |s| s.checkpoints().count())?;
        check(n == 1, || format!("{n} checkpoints with no new entry"))?;
        // A new entry: the next within one interval.
        let bob = k.d.party("bob", 0).await;
        transfer(&k, &alice, &bob, 3).await?;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 2, 12).await?;
        let second = anchoring(&k, |s| s.latest_final().copied())?.ok_or("no checkpoint")?;
        check(
            second.number == 2 && second.entry_sequence > first.entry_sequence,
            || format!("{second:?} after {first:?}"),
        )
    })
}

/// `AC-J1.2` (`REQ-PR-7`): the anchor commits to every entry up to and including a stated
/// sequence, and to nothing else: its commitment is the one its checkpoint's fields produce, the
/// layer holds that commitment at a finalized block, and the stated sequence is the last entry
/// the kernel root covers.
fn ac_j1_2__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        let alice = k.d.party("alice", 500).await;
        let bob = k.d.party("bob", 0).await;
        transfer(&k, &alice, &bob, 7).await?;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 1, 12).await?;
        let c = anchoring(&k, |s| s.latest_final().copied())?.ok_or("no checkpoint")?;
        check(c.consistent(), || {
            "the commitment is not the fields'".into()
        })?;
        let leaves = leaves_through(&k, c.entry_sequence)?;
        check(leaves.next_posting() == c.entry_sequence + 1, || {
            "the stated sequence is not the last entry covered".into()
        })?;
        check(leaves.root() == c.kernel_root, || {
            "the root is not the entries'".into()
        })?;
        check(
            k.chain.anchor_at_finalized(INSTANCE, 1).map(|a| a.0) == Some(c.commitment),
            || "the layer does not hold the commitment".into(),
        )
    })
}

/// `AC-J1.5` (`REQ-PR-10`, `DEC-25`): a checkpoint names the construction of its state
/// commitment, and the anchor binds that name: the layer holds the commitment of the checkpoint's
/// fields under the build's construction, and the same fields under the other construction do not
/// match it. The name is the content's: the checkpoint's kernel root is its entries' root under
/// the named construction, and not under the other. The default build runs it from a v2 genesis;
/// CI's `conformance (v1 roots at genesis)` runs it from a v1 genesis (`smt-v1`, owner ruling
/// cs-q04), where the checkpoint names construction 1.
fn ac_j1_5__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        let alice = k.d.party("alice", 500).await;
        let bob = k.d.party("bob", 0).await;
        transfer(&k, &alice, &bob, 7).await?;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 1, 12).await?;
        let c = anchoring(&k, |s| s.latest_final().copied())?.ok_or("no checkpoint")?;
        let named = ledger_core::anchor::construction();
        check(verify::construction_known(named), || {
            format!("construction {named} is not one a recipient can check")
        })?;
        check(c.construction == named, || {
            format!(
                "the checkpoint names construction {}, not {named}",
                c.construction
            )
        })?;
        let anchored = k.chain.anchor_at_finalized(INSTANCE, 1).map(|a| a.0);
        // `v3` (`T-037-03`): the same checkpoint, code binding included, under each construction.
        let under = |construction: u8| {
            verify::checkpoint_commitment(
                construction,
                c.binding.as_ref(),
                INSTANCE,
                c.number,
                c.entry_sequence,
                &c.kernel_root,
            )
        };
        check(anchored == Some(under(named)), || {
            "the anchor does not hold the commitment under the named construction".into()
        })?;
        let other = if named == verify::CONSTRUCTION_V2 {
            verify::CONSTRUCTION_V1
        } else {
            verify::CONSTRUCTION_V2
        };
        check(anchored != Some(under(other)), || {
            "the anchor also matches another construction".into()
        })?;
        let version = |c: u8| {
            if c == verify::CONSTRUCTION_V1 {
                ledger_kernel::compact_smt::Version::V1
            } else {
                ledger_kernel::compact_smt::Version::V2
            }
        };
        let leaves = leaves_through(&k, c.entry_sequence)?;
        check(leaves.root_in(version(named)) == c.kernel_root, || {
            format!("the root is not the entries' root under construction {named}")
        })?;
        check(leaves.root_in(version(other)) != c.kernel_root, || {
            format!("the root is also the entries' root under construction {other}")
        })
    })
}

/// `AC-J1.3` (`REQ-PR-8`): an anchor whose publication did not reach finality establishes
/// nothing: citing it is `ERR-AnchorNotFinal`; and one whose number the layer holds with another
/// commitment never becomes final and raises an incident.
fn ac_j1_3__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        let alice = k.d.party("alice", 100).await;
        // Another key of the instance stored anchor 1 with another commitment first.
        k.chain.inject_anchor(INSTANCE, 1, [0xEE; 32]);
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        for _ in 0..8 {
            k.chain.advance(1);
            k.step().await.map_err(|e| e.to_string())?;
        }
        let status = anchoring(&k, |s| s.checkpoint(1).map(|c| c.status))?;
        check(status != Some(CheckpointStatus::Final), || {
            "a mismatched anchor became final".into()
        })?;
        let incident = anchoring(&k, |s| {
            s.incidents()
                .iter()
                .any(|i| matches!(i, AnchorIncident::CommitmentMismatch { number: 1, .. }))
        })?;
        check(incident, || "no incident for the mismatch".into())?;
        let mut body = holds(&alice.account, 1);
        body["checkpoint"] = json!(1);
        let (s, v) = disclose(&k, &alice, body).await;
        check(
            refused(s, &v).err() == Some(Refusal::AnchorNotFinal),
            || format!("citing it: {s} {v}"),
        )?;
        let (s, v) = disclose(&k, &alice, holds(&alice.account, 1)).await;
        check(
            refused(s, &v).err() == Some(Refusal::AnchorNotFinal),
            || format!("citing the latest: {s} {v}"),
        )
    })
}

/// `AC-J1.4` (`REQ-PR-9`): with the adapter stopped, holders' commands run as before and the
/// anchor lag grows.
fn ac_j1_4__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        let alice = k.d.party("alice", 1_000).await;
        let bob = k.d.party("bob", 0).await;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 1, 12).await?;
        let anchored = anchoring(&k, |s| s.latest_final().map(|c| c.entry_sequence))?
            .ok_or("no checkpoint")?;
        // The adapter stops: no step from here on.
        for _ in 0..10 {
            transfer(&k, &alice, &bob, 1).await?;
            pass(&k, ANCHOR_INTERVAL_MS).await?;
        }
        let (available, _) = k.balances(&bob).await;
        check(available == 10, || format!("bob holds {available}"))?;
        let (lag, in_flight) = anchoring(&k, |s| {
            (
                s.latest().map_or(0, |c| c.entry_sequence) - anchored,
                s.in_flight().map(|c| c.status),
            )
        })?;
        check(
            lag > 0 && in_flight == Some(CheckpointStatus::Pending),
            || format!("lag {lag}, in flight {in_flight:?}"),
        )?;
        let final_now = anchoring(&k, |s| s.latest_final().map(|c| c.entry_sequence))?;
        check(final_now == Some(anchored), || {
            "the anchored sequence moved without the adapter".into()
        })
    })
}

// ------------------------------------------------------------------------------ AC-J3

/// `AC-J3.1` (`REQ-PR-5`): every final anchor has a recorded comparison of the book's record of
/// what it holds on the settlement layer with the layer's own position.
fn ac_j3_1__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        let alice = k.d.party("alice", 100).await;
        let bob = k.d.party("bob", 0).await;
        for n in 1..=3 {
            transfer(&k, &alice, &bob, 1).await?;
            pass(&k, ANCHOR_INTERVAL_MS).await?;
            until_final(&mut k, n, 12).await?;
            k.chain.advance(1);
            k.step().await.map_err(|e| e.to_string())?;
        }
        let (finals, uncompared, recorded) = anchoring(&k, |s| {
            (
                s.checkpoints()
                    .filter(|c| c.status == CheckpointStatus::Final)
                    .count(),
                s.uncompared(),
                s.comparisons().len(),
            )
        })?;
        check(
            finals == 3 && uncompared.is_empty() && recorded >= 1,
            || format!("{finals} final, uncompared {uncompared:?}, {recorded} recorded"),
        )?;
        // The operator reads them on its surface.
        let (s, v) =
            k.d.call(
                "GET",
                "/v1/ops/anchors",
                Some(&k.d.operator.token),
                &[],
                None,
            )
            .await;
        check(s.is_success(), || format!("the operator's read: {s} {v}"))?;
        let compared: usize = v["comparisons"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| c["anchors"].as_array().map_or(0, Vec::len))
                    .sum()
            })
            .unwrap_or(0);
        check(compared == 3, || {
            format!("{compared} anchors compared: {v}")
        })
    })
}

/// `AC-J3.2` (`NFR-9`): one minor unit sent to the position with a plain asset transfer, outside
/// the pallet's calls, is reported as a difference of one. Tolerance is zero.
fn ac_j3_2__service(_: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut k = anchoring_kit().await?;
        k.d.party("alice", 100).await;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 1, 12).await?;
        let before =
            anchoring(&k, |s| s.latest_comparison().cloned())?.ok_or("no comparison yet")?;
        let base = before.rows.iter().map(|r| r.difference()).sum::<i128>();
        k.chain.top_up_position(INSTANCE, &chain_usd(), 1);
        k.chain.advance(1);
        k.step().await.map_err(|e| e.to_string())?;
        let after = anchoring(&k, |s| s.latest_comparison().cloned())?.ok_or("no comparison")?;
        let diff = after.rows.iter().map(|r| r.difference()).sum::<i128>();
        check(diff - base == 1, || {
            format!("difference {diff} after {base}: {after:?}")
        })?;
        let incident = anchoring(&k, |s| {
            s.incidents().iter().any(|i| {
                matches!(i, AnchorIncident::PositionDifference { book, layer, .. }
                    if i128::from(layer.get()) - *book == diff)
            })
        })?;
        check(incident, || "no incident for the difference".into())
    })
}
