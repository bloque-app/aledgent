// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-009` — Ingress process: its registered cases (`F-015` §6, §7).
//!
//! Every case runs a whole deployment in this process: ingress's surface, received-log, forwarder,
//! ticker and delivery worker ([`ingress::testkit::Deployment`]), over the in-process core
//! ([`ingress::inprocess::InProcessCore`], `ledger-core` behind a write-ahead journal), with
//! recipients that verify every delivery with the reference verifier. Requests go through the
//! real router. The core the cases run is the in-process one on both targets: where the decision
//! runs is issue #38's to rule, and the `F-005` Agent implements the same boundary.
//!
//! - `AC-M2.1`–`AC-M2.3`, `AC-M3.1`: notifications at least once, self-contained, authentic and
//!   not replayable; outcomes reached with every recipient down.
//! - `AC-K3.1`, `AC-K3.2`: after a restart, storage alone says who was told; an undelivered
//!   notification is redelivered under one identity, across a crash between delivery and record.
//! - `AC-C1.1`, `AC-M5.1` (amendment 0008, `REQ-CS-8`): a reservation, which moves value, is
//!   acknowledged at `Received` and measured against `NFR-1`; an account or security command is
//!   answered with its decision, never `Received`, also while the core is down, and measured
//!   against `NFR-16`.
//! - `AC-M6.1`–`AC-M6.4` (amendment 0013, `REQ-CS-9`, `T-009-18`): a principal whose answering
//!   setting says "decided" gets its transfers and reservations answered with their decision, or
//!   `ERR-DecisionUnavailable` past its wait with the decision on retry and by notification; the
//!   default is acknowledged; a command keeps the way it was taken in; a wait above the decision
//!   wait is refused.
//! - `AC-C6.4` (amendment 0013, `REQ-EV-8`): reservation reports, pending on `T-009-20`.
//! - The exit criterion of `F-009` (`T-009-10`), registered under `AC-K3.1`: the core restarted
//!   ten times, and an ingress replica once, under steady mixed load. Every acknowledged operation
//!   reaches a decision, every owed notification is delivered and recorded, the input numbers are
//!   gapless, and every `Received` row is decided within the bound after the core returns.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ingress::boundary::{Core, OutboxItem};
use ingress::delivery::{ReferenceVerifier, Rejected, Verdict, signed_headers};
use ingress::inprocess::Fault;
use ingress::store::{InputKind, Recipient, Status, Store};
use ingress::testkit::{Deployment, Party, USD, fast_config};
use ledger_core::Refusal;
use serde_json::{Value, json};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::Harness;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F009,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-009`'s registered cases.
pub const CASES: &[Case] = &[
    service_case!(
        "AC-K3.1",
        ac_k3_1__service,
        &["REQ-RT-4", "REQ-SA-5", "REQ-EV-6"]
    ),
    service_case!(
        "AC-K3.1",
        ac_k3_1_core_restart_under_load_loses_no_command_and_no_notification__service,
        &[
            "REQ-AR-7", "REQ-CS-7", "REQ-EV-6", "REQ-RT-4", "NFR-5", "INV-18", "DEC-5",
        ]
    ),
    service_case!("AC-K3.2", ac_k3_2__service, &["REQ-EV-3", "REQ-RT-4"]),
    service_case!("AC-M2.1", ac_m2_1__service, &["REQ-EV-1", "REQ-EV-3"]),
    service_case!("AC-M2.2", ac_m2_2__service, &["REQ-EV-1", "REQ-EV-2"]),
    service_case!("AC-M2.3", ac_m2_3__service, &["REQ-EV-4", "REQ-AU-8"]),
    // Amendment 0011 (R3): the latest final anchor in every notification; with F-017.
    service_case!(
        "AC-M2.4",
        ac_m2_4_every_notification_names_the_latest_final_anchor__service,
        &["REQ-EV-1", "REQ-EV-3", "REQ-PR-7", "REQ-PR-8"]
    ),
    service_case!("AC-M3.1", ac_m3_1__service, &["REQ-EV-5"]),
    service_case!(
        "ERR-DecisionUnavailable",
        err_decision_unavailable__service,
        &["REQ-CS-7", "REQ-CS-3", "REQ-OP-10"]
    ),
    service_case!(
        "AC-C1.1",
        ac_c1_1_a_reservation_is_acknowledged_at_received__service,
        &["REQ-CS-2", "REQ-CS-3", "REQ-CS-8", "NFR-1", "NFR-15"]
    ),
    service_case!(
        "AC-M5.1",
        ac_m5_1__service,
        &["REQ-CS-2", "REQ-CS-7", "REQ-CS-8", "NFR-16"]
    ),
    service_case!(
        "ERR-MalformedCommand",
        err_malformed_command__service,
        &["REQ-ER-3", "REQ-ER-1", "REQ-CS-4"]
    ),
    service_case!(
        "ERR-IdentifierArchived",
        err_identifier_archived__service,
        &["REQ-RC-7", "REQ-ER-1", "REQ-CS-4"]
    ),
    service_case!(
        "AC-M6.1",
        ac_m6_1_a_decided_principal_gets_the_decision_in_the_reply__service,
        &["REQ-CS-9", "REQ-CS-3", "NFR-16"]
    ),
    service_case!(
        "AC-M6.2",
        ac_m6_2_past_the_wait_the_command_is_kept_and_decided__service,
        &[
            "REQ-CS-9",
            "REQ-CS-7",
            "REQ-EV-1",
            "ERR-DecisionUnavailable"
        ]
    ),
    service_case!(
        "AC-M6.3",
        ac_m6_3_acknowledged_by_default_and_as_taken_in__service,
        &["REQ-CS-9", "REQ-CS-8", "NFR-1"]
    ),
    service_case!(
        "AC-M6.4",
        ac_m6_4_a_wait_above_the_decision_wait_is_refused__service,
        &["REQ-CS-9", "ERR-NotAuthorised"]
    ),
    // Amendment 0013 (R2): reservation reports pushed at the recipient's cadence; T-009-20.
    Case::pending(
        "AC-C6.4",
        "ac_c6_4_a_reservation_report_per_period__service",
        Layer::Service,
        Feature::F009,
        &["REQ-EV-8", "REQ-EV-3"],
    ),
    // Amendment 0020: limits() (T-009-23), credit notifications (T-009-22) and pass-through data
    // (T-009-21), with F-007 and F-011.
    Case::pending(
        "AC-B3.4",
        "ac_b3_4_limits_publishes_the_most_legs_and_a_submission_at_it_is_taken__service",
        Layer::Service,
        Feature::F009,
        &["REQ-CS-10"],
    ),
    Case::pending(
        "AC-M7.1",
        "ac_m7_1_a_credit_notifies_the_accounts_serving_principal__service",
        Layer::Service,
        Feature::F009,
        &["REQ-EV-9"],
    ),
    Case::pending(
        "AC-M7.2",
        "ac_m7_2_no_credit_notification_for_a_release_or_an_opening__service",
        Layer::Service,
        Feature::F009,
        &["REQ-EV-9"],
    ),
    Case::pending(
        "AC-M7.3",
        "ac_m7_3_the_requester_gets_both_notifications_each_with_its_identity__service",
        Layer::Service,
        Feature::F009,
        &["REQ-EV-9", "REQ-EV-3"],
    ),
    Case::pending(
        "AC-M7.4",
        "ac_m7_4_an_identity_opened_account_notifies_the_identitys_opener__service",
        Layer::Service,
        Feature::F009,
        &["REQ-EV-9"],
    ),
    service_case!(
        "AC-M8.1",
        ac_m8_1_pass_through_comes_back_everywhere_its_owner_reads__service,
        &["REQ-PO-5"]
    ),
    service_case!(
        "AC-M8.2",
        ac_m8_2_different_pass_through_under_one_key_is_refused__service,
        &["REQ-PO-5", "ERR-ClientKeyReused"]
    ),
    service_case!(
        "AC-M8.3",
        ac_m8_3_pass_through_over_the_bound_is_refused__service,
        &["REQ-PO-5", "ERR-MalformedCommand", "REQ-ER-3"]
    ),
    service_case!(
        "AC-M8.4",
        ac_m8_4_another_party_never_sees_the_pass_through__service,
        &["REQ-PO-5", "REQ-EV-9"]
    ),
];

type Result<T = ()> = std::result::Result<T, String>;

fn check(cond: bool, why: impl FnOnce() -> String) -> Result {
    if cond { Ok(()) } else { Err(why()) }
}

/// How long a case waits for a condition the deployment reaches on its own — a row decided, a
/// notification delivered, an attempt kept — before it fails (issue #48). Generous: the suite
/// runs every case at once, in a debug build, on a shared two-vCPU runner. A wait ends as soon as
/// its condition holds, so this costs nothing on a passing run. It is not a bound any
/// acceptance criterion states: those are checked as the deployment states them.
const PATIENCE: Duration = Duration::from_secs(120);

/// `eventually!(what, probe)`: evaluate `probe` (an expression of type [`Result`], usually
/// `async { … }.await`) until it is `Ok`, polling. It is `Err` with `what` and the last thing
/// the probe saw if it has not held within [`PATIENCE`]. A macro, so the probe borrows the
/// case's locals freely.
macro_rules! eventually {
    ($what:expr, $probe:expr) => {{
        let until = Instant::now() + PATIENCE;
        loop {
            let seen: Result = $probe;
            match seen {
                Ok(()) => break Ok::<(), String>(()),
                Err(seen) if Instant::now() >= until => {
                    break Err(format!(
                        "{}: not within {PATIENCE:?}; last seen: {seen}",
                        $what
                    ));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }};
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

fn transfer(from: &Party, to: &Party, amount: u64) -> Value {
    json!({ "legs": [{ "from": from.account, "to": to.account, "asset": USD, "amount": amount.to_string() }] })
}

fn reference(v: &Value) -> Result<String> {
    v["operation_reference"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("no operation reference in {v}"))
}

/// Every notification in the core's outbox, delivered or not.
async fn outbox(d: &Deployment) -> Result<Vec<OutboxItem>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = d
            .core
            .outbox(cursor.as_deref(), false, d.clock_now())
            .await
            .map_err(|e| format!("the outbox did not answer: {e:?}"))?;
        all.extend(page.items);
        match page.next {
            Some(n) => cursor = Some(n),
            None => return Ok(all),
        }
    }
}

trait ClockNow {
    fn clock_now(&self) -> u64;
}

impl ClockNow for Deployment {
    fn clock_now(&self) -> u64 {
        use ingress::clock::Clock;
        self.clock.now()
    }
}

/// A transfer, a captured reservation and a released one, from `alice` to `bob`. Returns the
/// three operation references.
async fn three_outcomes(d: &Deployment, alice: &Party, bob: &Party) -> Result<Vec<String>> {
    let a = Some(alice.token.as_str());
    let (s, t) = d
        .call(
            "POST",
            "/v1/transfers",
            a,
            &[],
            Some(transfer(alice, bob, 5)),
        )
        .await;
    check(s.as_u16() == 202, || format!("transfer: {s} {t}"))?;
    let reserve = json!({ "payer": alice.account, "asset": USD, "amount": "7" });
    let (s, r1) = d
        .call_decided(
            "POST",
            "/v1/reservations",
            a,
            &[("client-key", "r1")],
            Some(reserve.clone()),
        )
        .await;
    check(s.as_u16() == 200, || format!("reserve: {s} {r1}"))?;
    let (s, r2) = d
        .call_decided(
            "POST",
            "/v1/reservations",
            a,
            &[("client-key", "r2")],
            Some(reserve),
        )
        .await;
    check(s.as_u16() == 200, || format!("reserve: {s} {r2}"))?;
    let (r1, r2) = (reference(&r1)?, reference(&r2)?);
    let (s, c) = d
        .call(
            "POST",
            &format!("/v1/reservations/{r1}/capture"),
            a,
            &[],
            Some(json!({ "destination": bob.account, "cleared": "7" })),
        )
        .await;
    check(s.as_u16() == 200 && c["state"] == "Settled", || {
        format!("capture: {s} {c}")
    })?;
    let (s, rel) = d
        .call(
            "POST",
            &format!("/v1/reservations/{r2}/release"),
            a,
            &[],
            None,
        )
        .await;
    check(s.as_u16() == 200 && rel["state"] == "Released", || {
        format!("release: {s} {rel}")
    })?;
    Ok(vec![reference(&t)?, r1, r2])
}

// ------------------------------------------------------------------------------ AC-M2.*, AC-M3.1

/// `AC-M2.1`: every terminal outcome is notified at least once; the notification carries the
/// operation reference and an identity that makes a redelivery recognisable.
fn ac_m2_1__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let bob = d.party("bob", 0).await;
        let refs = three_outcomes(&d, &alice, &bob).await?;
        check(d.all_delivered(PATIENCE).await, || {
            "not every notification was delivered".into()
        })?;
        let got = d.recipients.received(&alice.endpoint);
        for (r, outcome) in refs.iter().zip(["Settled", "Settled", "Released"]) {
            let n = got
                .fresh
                .iter()
                .find(|n| &n.operation_reference == r && n.terminal)
                .ok_or_else(|| format!("no terminal notification for {r}"))?;
            check(n.outcome.as_deref() == Some(outcome), || {
                format!("{r}: outcome {:?}, expected {outcome}", n.outcome)
            })?;
        }
        let ids: BTreeSet<&String> = got.fresh.iter().map(|n| &n.id).collect();
        check(ids.len() == got.fresh.len(), || {
            "two notifications share an identity".into()
        })?;
        check(got.rejected == 0, || {
            "a delivery failed verification".into()
        })
    })
}

/// `AC-M2.2`: arrival order is not guaranteed and each notification stands on its own: every
/// one carries the operation reference, the state with its class and terminal flag, the outcome
/// where terminal and the named refusal where refused.
fn ac_m2_2__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 10).await;
        let bob = d.party("bob", 0).await;
        // Held back, then released all at once: they arrive in the outbox's order, not the
        // transitions' order.
        d.recipients.set_up(&alice.endpoint, false);
        let a = Some(alice.token.as_str());
        let (_, ok) = d
            .call(
                "POST",
                "/v1/transfers",
                a,
                &[],
                Some(transfer(&alice, &bob, 4)),
            )
            .await;
        let (_, refused) = d
            .call(
                "POST",
                "/v1/transfers",
                a,
                &[],
                Some(transfer(&alice, &bob, 50)),
            )
            .await;
        check(d.quiesce(PATIENCE).await, || "undecided".into())?;
        d.recipients.set_up(&alice.endpoint, true);
        check(d.all_delivered(PATIENCE).await, || "not delivered".into())?;
        let got = d.recipients.received(&alice.endpoint);
        for n in &got.fresh {
            check(
                !n.operation_reference.is_empty() && !n.state.is_empty(),
                || format!("{n:?} is not self-contained"),
            )?;
            check(n.terminal == n.outcome.is_some(), || {
                format!("{n:?}: outcome where and only where terminal")
            })?;
            check((n.state == "Rejected") == n.error.is_some(), || {
                format!("{n:?}: an error where and only where refused")
            })?;
        }
        let of = |r: &str| -> Vec<&ingress::wire::NotificationBody> {
            got.fresh
                .iter()
                .filter(|n| n.operation_reference == r)
                .collect()
        };
        let settled = of(&reference(&ok)?);
        check(settled.iter().any(|n| n.state == "Settled"), || {
            "the settled transfer's terminal notification is missing".into()
        })?;
        let rejected = of(&reference(&refused)?);
        let last = rejected
            .iter()
            .find(|n| n.state == "Rejected")
            .ok_or("the rejected transfer's notification is missing")?;
        check(last.error == Some(Refusal::InsufficientAvailable), || {
            format!("the refusal is named: {:?}", last.error)
        })
    })
}

/// `AC-M2.4` (amendment 0011 R3, `REQ-EV-1`, `REQ-EV-3`): a notification names the latest anchor
/// that was final when its transition was recorded — its number, the last entry it commits to,
/// its record's coordinate and block — or says that none was. Every delivery names the same one.
///
/// A deployment that anchors ([`super::f017::anchoring_kit`]). Alice's first transfer is recorded
/// before any anchor is final, with her recipient down, so it is delivered only after checkpoint 1
/// is final: it still says none. Her second, recorded after, names checkpoint 1 as the core holds
/// it. Once checkpoint 2 is final too, the outbox — what every redelivery is sent from — still
/// holds both bodies exactly as delivered.
fn ac_m2_4_every_notification_names_the_latest_final_anchor__service(
    _h: &mut dyn Harness,
) -> Outcome {
    use super::f017::{anchoring, anchoring_kit, pass, until_final};
    use ingress::testkit::ANCHOR_INTERVAL_MS;
    run(|| async {
        let mut k = anchoring_kit().await?;
        let alice = k.d.party("alice", 10).await;
        let bob = k.d.party("bob", 0).await;
        check(k.d.all_delivered(PATIENCE).await, || "setup".into())?;
        k.d.recipients.set_up(&alice.endpoint, false);
        let a = Some(alice.token.as_str());
        let (_, t1) =
            k.d.call(
                "POST",
                "/v1/transfers",
                a,
                &[],
                Some(transfer(&alice, &bob, 1)),
            )
            .await;
        let first = reference(&t1)?;
        check(k.d.quiesce(PATIENCE).await, || "undecided".into())?;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 1, 12).await?;
        let one = anchoring(&k, |s| s.checkpoint(1).copied())?.ok_or("no checkpoint 1")?;
        k.d.recipients.set_up(&alice.endpoint, true);
        let (_, t2) =
            k.d.call(
                "POST",
                "/v1/transfers",
                a,
                &[],
                Some(transfer(&alice, &bob, 2)),
            )
            .await;
        let second = reference(&t2)?;
        check(k.d.quiesce(PATIENCE).await, || "undecided".into())?;
        check(k.d.all_delivered(PATIENCE).await, || "not delivered".into())?;
        let got = k.d.recipients.received(&alice.endpoint);
        let of = |r: &str| -> Vec<&ingress::wire::NotificationBody> {
            got.fresh
                .iter()
                .filter(|n| n.operation_reference == r)
                .collect()
        };
        let (before, after) = (of(&first), of(&second));
        check(!before.is_empty() && !after.is_empty(), || {
            format!("{} and {} notifications", before.len(), after.len())
        })?;
        check(before.iter().all(|n| n.anchor.is_none()), || {
            format!("recorded before any anchor was final, delivered after: {before:?}")
        })?;
        let coordinate = one.coordinate.ok_or("checkpoint 1 has no coordinate")?;
        let height = one.height.ok_or("checkpoint 1 has no height")?;
        for n in &after {
            let r = n
                .anchor
                .as_ref()
                .ok_or_else(|| format!("{n:?} names no anchor"))?;
            check(
                r.number == 1
                    && r.entry_sequence == one.entry_sequence
                    && r.coordinate.instance == coordinate.instance
                    && r.coordinate.seq == coordinate.seq
                    && r.coordinate.digest
                        == coordinate
                            .digest
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>()
                    && r.height == height,
                || format!("{r:?} is not checkpoint 1 as the core holds it: {one:?}"),
            )?;
        }
        // A later anchor changes nothing already recorded: the outbox every redelivery is sent
        // from holds the same bodies.
        let carol = k.d.party("carol", 0).await;
        let (_, t3) =
            k.d.call(
                "POST",
                "/v1/transfers",
                a,
                &[],
                Some(transfer(&alice, &carol, 1)),
            )
            .await;
        let third = reference(&t3)?;
        check(k.d.quiesce(PATIENCE).await, || "undecided".into())?;
        pass(&k, ANCHOR_INTERVAL_MS).await?;
        until_final(&mut k, 2, 12).await?;
        let items = outbox(&k.d).await?;
        for n in before.iter().chain(&after) {
            let i = items
                .iter()
                .find(|i| i.notification.id == n.id)
                .ok_or_else(|| format!("{} is not in the outbox", n.id))?;
            check(&i.notification == *n, || {
                format!("delivered {n:?}, the outbox now holds {:?}", i.notification)
            })?;
        }
        check(
            items
                .iter()
                .filter(|i| i.notification.operation_reference == third)
                .all(|i| i.notification.anchor.as_ref().map(|r| r.number) == Some(1)),
            || "recorded before checkpoint 2 was final: checkpoint 1".into(),
        )
    })
}

/// `AC-M2.3`: a recipient verifies authenticity, and cannot be made to act on a replay. A
/// recipient with no configured secret gets no delivery: the notification stays undelivered and
/// visible with its age.
fn ac_m2_3__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 100).await;
        let bob = d.party("bob", 100).await;
        // Bob's recipient has no secret.
        d.store
            .put_recipient(Recipient {
                principal: bob.identity.clone(),
                endpoint: bob.endpoint.clone(),
                secret: None,
            })
            .await
            .map_err(|e| e.to_string())?;
        let (_, t) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&alice.token),
                &[],
                Some(transfer(&alice, &bob, 1)),
            )
            .await;
        let (_, u) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&bob.token),
                &[],
                Some(transfer(&bob, &alice, 1)),
            )
            .await;
        check(d.quiesce(PATIENCE).await, || "undecided".into())?;
        // Wait for the delivery worker to have reached both: alice's notification delivered, and
        // every one owed to bob passed over as parked, without an attempt (a recipient without a
        // secret parks them, `F-031` T-031-15; issue #48: a fixed sleep raced the worker).
        let alices = reference(&t)?;
        eventually!("alice's notification did not arrive", {
            let got = d.recipients.received(&alice.endpoint);
            check(
                got.fresh.iter().any(|n| n.operation_reference == alices),
                || format!("{} fresh deliveries to alice", got.fresh.len()),
            )
        })?;
        eventually!(
            "the delivery worker did not reach bob's notifications",
            async {
                let items = outbox(&d).await?;
                let owed: Vec<String> = items
                    .iter()
                    .filter(|i| i.recipient.as_deref() == Some(bob.identity.as_str()))
                    .map(|i| i.notification.id.clone())
                    .collect();
                let parked = d
                    .replica
                    .shared
                    .metrics
                    .delivery_parked
                    .load(std::sync::atomic::Ordering::Relaxed);
                let mut attempted = Vec::new();
                for id in &owed {
                    if d.store.attempts(id).await.map_err(|e| e.to_string())? > 0 {
                        attempted.push(id.clone());
                    }
                }
                check(attempted.is_empty(), || {
                    format!("attempted without a secret: {attempted:?}")
                })?;
                check(!owed.is_empty() && parked >= owed.len() as u64, || {
                    format!("{parked} parked, {} owed to bob", owed.len())
                })
            }
            .await
        )?;
        // Alice's arrived, signed and verified.
        let got = d.recipients.received(&alice.endpoint);
        check(
            got.fresh.iter().any(|n| n.operation_reference == alices),
            || "alice's notification did not arrive".into(),
        )?;
        check(got.rejected == 0, || {
            "an authentic delivery was rejected".into()
        })?;
        // Bob's never left: no secret, no delivery. It is visible, with its age.
        check(
            d.recipients.received(&bob.endpoint).deliveries.is_empty(),
            || "bob received a delivery without a secret".into(),
        )?;
        let (s, list) = d
            .call(
                "GET",
                "/v1/notifications/undelivered",
                Some(&d.operator.token),
                &[],
                None,
            )
            .await;
        check(s.as_u16() == 200, || format!("undelivered: {s} {list}"))?;
        let bobs = reference(&u)?;
        let listed = list
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|i| i["notification"]["operation_reference"] == bobs.as_str())
                    .count()
            })
            .unwrap_or(0);
        check(listed == 2, || {
            format!("bob's two notifications are listed: {list}")
        })?;
        // The reference verifier: a replay is recognised, a stale or altered one refused.
        let n = got.fresh.first().ok_or("nothing to replay")?;
        let body = serde_json::to_vec(n).map_err(|e| e.to_string())?;
        let secret = format!("secret-of-{}", alice.token).into_bytes();
        let mut v = ReferenceVerifier::new(secret.clone(), 60_000);
        let at = 1_000_000;
        let h = signed_headers(&secret, &n.id, at, &body);
        check(v.verify(&h, &body, at) == Ok(Verdict::Fresh), || {
            "fresh".into()
        })?;
        check(
            v.verify(&h, &body, at + 1) == Ok(Verdict::Duplicate),
            || "a replay is recognised".into(),
        )?;
        check(
            v.verify(&h, &body, at + 60_001) == Err(Rejected::Stale),
            || "a stale replay is refused".into(),
        )?;
        let mut altered = body.clone();
        altered.push(b' ');
        check(
            v.verify(&h, &altered, at) == Err(Rejected::BadSignature),
            || "an altered body is refused".into(),
        )?;
        let forged = signed_headers(b"guess", &n.id, at, &body);
        check(
            ReferenceVerifier::new(secret, 60_000).verify(&forged, &body, at)
                == Err(Rejected::BadSignature),
            || "a forged signature is refused".into(),
        )
    })
}

/// `AC-M3.1`: with every recipient down, every operation still reaches its terminal state. The
/// core never waits for a recipient; the notifications wait, and are delivered once recipients
/// return.
fn ac_m3_1__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let bob = d.party("bob", 0).await;
        d.recipients.set_all(false);
        let refs = three_outcomes(&d, &alice, &bob).await?;
        check(d.quiesce(PATIENCE).await, || "undecided".into())?;
        for r in &refs {
            let op = ledger_core::id::OperationId::parse(r).map_err(|e| format!("{e:?}"))?;
            let terminal = d
                .core
                .with_ledger(|l| l.operation(&op).is_some_and(|o| o.state().is_terminal()))
                .unwrap_or(false);
            check(terminal, || format!("{r} did not reach a terminal state"))?;
        }
        check(d.undelivered_now().await.unwrap_or(0) > 0, || {
            "the notifications wait".into()
        })?;
        d.recipients.set_all(true);
        check(d.all_delivered(PATIENCE).await, || {
            "delivered once recipients returned".into()
        })
    })
}

// ------------------------------------------------------------------------------ errors

/// `ERR-DecisionUnavailable` (`REQ-CS-7`, amendment 0006 item 8): a command answered with its
/// decision that the core cannot decide within the replica's wait is answered with the refusal,
/// 503. Since amendment 0007 (`OQ-61` as ruled on 2026-09-29) a reservation is acknowledged
/// instead, so this uses an issuance. The command is kept: once the core returns it is decided,
/// and a retry under the same client key gets that decision, with no second issuance.
fn err_decision_unavailable__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut config = fast_config("ingress-0");
        config.decision_wait = Duration::from_millis(200);
        let d = Deployment::start_with(config).await;
        let alice = d.party("alice", 0).await;
        let account = alice.account.clone().ok_or("an account")?;
        let operator = d.operator.token.clone();
        d.core.stop();
        let body = json!({ "asset": USD, "destination": account, "amount": "100" });
        let key = [(ingress::surface::CLIENT_KEY, "hold-7")];
        let (s, v) = d
            .call(
                "POST",
                "/v1/issuances",
                Some(&operator),
                &key,
                Some(body.clone()),
            )
            .await;
        check(s.as_u16() == 503, || format!("{s}: {v}"))?;
        let refusal = serde_json::from_value::<Refusal>(v.clone()).map_err(|e| e.to_string())?;
        crate::shape::check_refusal(
            &crate::spec::Catalogue::checked_in(),
            Err::<(), Refusal>(refusal),
            "ERR-DecisionUnavailable",
        )?;
        d.core.start().map_err(|e| e.to_string())?;
        // The retry under the same key: the decision, once made.
        let mut last = Value::Null;
        eventually!(
            "the retry got the decision",
            async {
                let (s, v) = d
                    .call(
                        "POST",
                        "/v1/issuances",
                        Some(&operator),
                        &key,
                        Some(body.clone()),
                    )
                    .await;
                last = v.clone();
                check(s.as_u16() == 200, || format!("{s}: {v}"))
            }
            .await
        )?;
        // One issuance, not two: 100 is available, and 101 cannot be reserved.
        let (s, v) = d
            .call_decided(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[],
                Some(json!({ "payer": account, "asset": USD, "amount": "101" })),
            )
            .await;
        check(
            s.as_u16() == 422 && v["error"] == "ERR-InsufficientAvailable",
            || format!("a second issuance was made ({last}): {s} {v}"),
        )
    })
}

// ------------------------------------------------------------------------------ AC-C1.1, AC-M5.1

/// `AC-C1.1` through ingress (`REQ-CS-8`, amendment 0008): a reservation moves value, so it is
/// acknowledged at `Received` even while the core is down, and that reply is an `NFR-1` sample,
/// not an `NFR-16` one. Its decision follows by query.
fn ac_c1_1_a_reservation_is_acknowledged_at_received__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let account = alice.account.clone().ok_or("an account")?;
        let metrics = &d.replica.shared.metrics;
        let acknowledged = metrics.acknowledgements().samples;
        let decided = metrics.decided_replies().samples;
        d.core.stop();
        let (s, v) = d
            .call(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[],
                Some(json!({ "payer": account, "asset": USD, "amount": "100" })),
            )
            .await;
        check(
            s.as_u16() == 202 && v["state"] == "Received" && v["terminal"] == false,
            || format!("not acknowledged at Received: {s} {v}"),
        )?;
        check(
            metrics.acknowledgements().samples == acknowledged + 1
                && metrics.decided_replies().samples == decided,
            || "the acknowledgement was not measured as NFR-1's".to_owned(),
        )?;
        let path = format!("/v1/operations/{}", reference(&v)?);
        d.core.start().map_err(|e| e.to_string())?;
        eventually!(
            "the decision follows by query",
            async {
                let (_, q) = d.call("GET", &path, Some(&alice.token), &[], None).await;
                check(q["state"] == "Reserved", || q.to_string())
            }
            .await
        )
    })
}

/// `AC-M5.1` (`REQ-CS-8`, amendment 0008): account and security commands are answered with their
/// decision, a result or a named refusal, and never `Received`. While the core is down the answer
/// is `ERR-DecisionUnavailable`, not `202`, and a retry under the same client key gets the
/// decision. Every such reply is an `NFR-16` sample, none an `NFR-1` one.
fn ac_m5_1__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let mut config = fast_config("ingress-0");
        config.decision_wait = Duration::from_millis(200);
        let d = Deployment::start_with(config).await;
        let alice = d.party("alice", 5).await;
        let account = alice.account.clone().ok_or("an account")?;
        let op = d.operator.token.clone();
        let metrics = &d.replica.shared.metrics;
        let acknowledged = metrics.acknowledgements().samples;
        let decided = metrics.decided_replies().samples;
        let accepted = [
            (
                "open_identity",
                "/v1/identities".to_owned(),
                json!({ "kind": "person", "verifier": "0c" }),
            ),
            (
                "open_account",
                "/v1/accounts".to_owned(),
                json!({ "owner": alice.identity, "kind": "customer", "custody": "delegated" }),
            ),
            (
                "grant_operator",
                "/v1/ops/operator-grants".to_owned(),
                json!({
                    "principal": d.operator.identity,
                    "kinds": ["freeze_account", "unfreeze_account"],
                    "justification": "AC-M5.1",
                }),
            ),
            (
                "freeze_account",
                format!("/v1/ops/accounts/{account}/freeze"),
                json!({ "justification": "AC-M5.1" }),
            ),
            (
                "unfreeze_account",
                format!("/v1/ops/accounts/{account}/unfreeze"),
                json!({ "justification": "AC-M5.1 closed" }),
            ),
        ];
        for (name, path, body) in &accepted {
            let (s, v) = d
                .call("POST", path, Some(&op), &[], Some(body.clone()))
                .await;
            check(s.as_u16() == 200 && v["state"] != "Received", || {
                format!("{name} was not answered with its decision: {s} {v}")
            })?;
        }
        // A refusal is a decision too, and it is named: the account still holds value.
        let (s, v) = d
            .call(
                "POST",
                &format!("/v1/accounts/{account}/close"),
                Some(&op),
                &[],
                Some(json!({})),
            )
            .await;
        check(
            s.is_client_error() && serde_json::from_value::<Refusal>(v.clone()).is_ok(),
            || format!("close_account was not refused by name: {s} {v}"),
        )?;
        // The core down: no `202`, but `ERR-DecisionUnavailable`; the retry gets the decision.
        d.core.stop();
        let key = [(ingress::surface::CLIENT_KEY, "ac-m5-1")];
        let body = json!({ "kind": "person", "verifier": "0d" });
        let (s, v) = d
            .call(
                "POST",
                "/v1/identities",
                Some(&op),
                &key,
                Some(body.clone()),
            )
            .await;
        check(
            s.as_u16() == 503 && v["error"] == "ERR-DecisionUnavailable",
            || format!("open_identity with the core down: {s} {v}"),
        )?;
        d.core.start().map_err(|e| e.to_string())?;
        eventually!(
            "the retry got the decision",
            async {
                let (s, v) = d
                    .call(
                        "POST",
                        "/v1/identities",
                        Some(&op),
                        &key,
                        Some(body.clone()),
                    )
                    .await;
                check(
                    s.as_u16() == 200 && v["result"]["identity"].is_string(),
                    || format!("{s}: {v}"),
                )
            }
            .await
        )?;
        check(metrics.acknowledgements().samples == acknowledged, || {
            "an account or security command was measured as an acknowledgement (NFR-1)".to_owned()
        })?;
        let replies = accepted.len() as u64 + 3;
        check(
            metrics.decided_replies().samples >= decided + replies,
            || {
                format!(
                    "{} decided replies measured (NFR-16), at least {replies} expected",
                    metrics.decided_replies().samples - decided
                )
            },
        )
    })
}

// ------------------------------------------------------------------------------ AC-K3.*

/// `AC-K3.1`: after a restart of the core, storage alone says, for every operation, what was
/// attempted (the received-log), what happened (the operation's state) and whether every party
/// owed a notification received one (the delivery records).
fn ac_k3_1__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let bob = d.party("bob", 1_000).await;
        // Bob is down: his notifications stay owed.
        d.recipients.set_up(&bob.endpoint, false);
        let refs = three_outcomes(&d, &alice, &bob).await?;
        let (_, back) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&bob.token),
                &[],
                Some(transfer(&bob, &alice, 2)),
            )
            .await;
        check(d.quiesce(PATIENCE).await, || "undecided".into())?;
        // Before the restart, let the delivery worker finish what it can: every notification
        // owed to alice delivered and its record decided, and an attempt kept for every one owed
        // to bob. Issue #48: the case used to restart as soon as alice had been told of the
        // release, which raced the worker's first attempt at bob's last notification.
        eventually!(
            "the delivery worker did not settle before the restart",
            async {
                check(
                    d.recipients
                        .received(&alice.endpoint)
                        .fresh
                        .iter()
                        .any(|n| n.state == "Released"),
                    || "alice was not told of the release".into(),
                )?;
                check(d.quiesce(Duration::ZERO).await, || {
                    "records undecided".into()
                })?;
                let items = outbox(&d).await?;
                let unrecorded = items
                    .iter()
                    .filter(|i| i.recipient.as_deref() == Some(alice.identity.as_str()))
                    .filter(|i| i.delivered_at.is_none())
                    .count();
                check(unrecorded == 0, || {
                    format!("{unrecorded} of alice's notifications unrecorded")
                })?;
                let mut untried = 0;
                for i in items
                    .iter()
                    .filter(|i| i.recipient.as_deref() == Some(bob.identity.as_str()))
                {
                    let n = d
                        .store
                        .attempts(&i.notification.id)
                        .await
                        .map_err(|e| e.to_string())?;
                    untried += usize::from(n == 0);
                }
                check(untried == 0, || {
                    format!("{untried} of bob's notifications not yet tried")
                })
            }
            .await
        )?;
        // Restart: the core is rebuilt from its journal alone.
        d.core.stop();
        d.core.start()?;
        let told_alice: BTreeSet<String> = d
            .recipients
            .received(&alice.endpoint)
            .deliveries
            .into_iter()
            .collect();
        let items = outbox(&d).await?;
        check(!items.is_empty(), || {
            "the outbox survived the restart".into()
        })?;
        for i in &items {
            let owed_to_alice = i.recipient.as_deref() == Some(alice.identity.as_str());
            if owed_to_alice {
                check(
                    i.delivered_at.is_some() == told_alice.contains(&i.notification.id),
                    || format!("{}: record and recipient disagree", i.notification.id),
                )?;
            } else {
                check(i.delivered_at.is_none(), || {
                    format!("{}: bob was down, yet it is recorded", i.notification.id)
                })?;
            }
        }
        // Everything alice is owed was delivered and recorded.
        check(
            items
                .iter()
                .filter(|i| i.recipient.as_deref() == Some(alice.identity.as_str()))
                .all(|i| i.delivered_at.is_some()),
            || "alice's notifications are all recorded".into(),
        )?;
        // What was attempted: every command is in the received-log with the core's reply.
        let rows = d.store.rows().await.map_err(|e| e.to_string())?;
        for r in refs.iter().chain([reference(&back)?].iter()) {
            let op = ledger_core::id::OperationId::parse(r).map_err(|e| format!("{e:?}"))?;
            let logged = rows.iter().any(|row| {
                row.new.kind == InputKind::Command
                    && row.status == Status::Decided
                    && (row.new.op_ref.0 == *op.as_bytes()
                        || String::from_utf8_lossy(&row.new.body).contains(r.as_str()))
            });
            check(logged, || format!("{r} is not in the received-log"))?;
            let state = d
                .core
                .with_ledger(|l| l.operation(&op).map(|o| o.state()))
                .flatten();
            check(state.is_some_and(|s| s.is_terminal()), || {
                format!("{r}: what happened is not stated: {state:?}")
            })?;
        }
        // Bob's owed notifications are visible with their attempts.
        let owed: Vec<&OutboxItem> = items
            .iter()
            .filter(|i| i.recipient.as_deref() == Some(bob.identity.as_str()))
            .collect();
        check(!owed.is_empty(), || "bob is owed notifications".into())?;
        for i in owed {
            let n = d
                .store
                .attempts(&i.notification.id)
                .await
                .map_err(|e| e.to_string())?;
            check(n > 0, || format!("{}: no attempt kept", i.notification.id))?;
        }
        Ok(())
    })
}

/// `AC-K3.2`: a crash between the recipient's acknowledgement and the delivery record means one
/// redelivery under the same identity, recognised as such; then it is recorded.
fn ac_k3_2__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 100).await;
        let bob = d.party("bob", 0).await;
        check(d.all_delivered(PATIENCE).await, || "setup".into())?;
        d.replica
            .shared
            .faults
            .crash_before_record
            .store(true, Ordering::SeqCst);
        let (_, t) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&alice.token),
                &[],
                Some(transfer(&alice, &bob, 3)),
            )
            .await;
        check(d.all_delivered(PATIENCE).await, || {
            "not delivered after the crash".into()
        })?;
        let got = d.recipients.received(&alice.endpoint);
        let r = reference(&t)?;
        let ids: Vec<&String> = got
            .fresh
            .iter()
            .filter(|n| n.operation_reference == r)
            .map(|n| &n.id)
            .collect();
        let mut counts: BTreeMap<&String, usize> = BTreeMap::new();
        for id in &got.deliveries {
            *counts.entry(id).or_default() += 1;
        }
        let twice: Vec<&&String> = ids
            .iter()
            .filter(|id| counts.get(**id) == Some(&2))
            .collect();
        check(twice.len() == 1, || {
            format!("exactly one notification delivered twice under one identity: {counts:?}")
        })?;
        check(got.duplicates == 1, || {
            format!(
                "the redelivery was recognised: {} duplicates",
                got.duplicates
            )
        })?;
        let items = outbox(&d).await?;
        check(
            items
                .iter()
                .filter(|i| i.notification.operation_reference == r)
                .all(|i| i.delivered_at.is_some()),
            || "recorded after the redelivery".into(),
        )
    })
}

/// The exit criterion of `F-009` (`T-009-10`): the core restarted ten times under steady mixed
/// load — transfers acknowledged at `Received`, reservations answered synchronously, captures and
/// releases — with faults injected around the journal write, recipients flapping, and one ingress
/// replica killed and replaced. Afterwards: every acknowledged operation reached a decision;
/// every owed notification was delivered at least once and recorded; the core's input numbers
/// are gapless; and after each restart every `Received` row was decided within the bound.
fn ac_k3_1_core_restart_under_load_loses_no_command_and_no_notification__service(
    _h: &mut dyn Harness,
) -> Outcome {
    run(|| async {
        // The decision bound this deployment states (REQ-CS-7): generous enough for a debug
        // build on a shared CI runner, where every case of the suite runs at once.
        let mut config = fast_config("ingress-0");
        config.decision_bound = 30_000;
        let mut d = Deployment::start_with(config).await;
        let alice = d.party("alice", 1_000_000).await;
        let bob = d.party("bob", 1_000_000).await;
        let bound = Duration::from_millis(d.replica.shared.config.decision_bound);
        let router = Arc::new(std::sync::Mutex::new(d.router.clone()));
        let stop = Arc::new(AtomicBool::new(false));
        let acknowledged = Arc::new(std::sync::Mutex::new(Vec::<(String, bool)>::new()));
        // The load is a budget of operations the rounds hand out, not a rate (issue #48): a
        // client starts an operation only while fewer have started than the budget allows. So
        // the run holds the same load however slow the machine, and the received-log, the
        // journal and the outbox stay the same size.
        let budget = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let mut clients = Vec::new();
        for c in 0..3u32 {
            let (from, to) = if c == 1 {
                (bob.clone(), alice.clone())
            } else {
                (alice.clone(), bob.clone())
            };
            let router = Arc::clone(&router);
            let stop = Arc::clone(&stop);
            let acked = Arc::clone(&acknowledged);
            let (budget, started) = (Arc::clone(&budget), Arc::clone(&started));
            clients.push(tokio::spawn(async move {
                let mut i = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    let n = started.load(Ordering::SeqCst);
                    if n >= budget.load(Ordering::SeqCst)
                        || started
                            .compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                    {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue;
                    }
                    i += 1;
                    let r = router
                        .lock()
                        .map(|r| r.clone())
                        .map_err(|e| e.to_string())?;
                    let key = format!("c{c}-{i}");
                    let t = Some(from.token.as_str());
                    if c == 2 && i % 2 == 0 {
                        // A reservation, then its capture or release.
                        let body = json!({ "payer": from.account, "asset": USD, "amount": "2" });
                        let (s, v) = ingress::testkit::decided(
                            &r,
                            "POST",
                            "/v1/reservations",
                            t,
                            &[("client-key", &key)],
                            Some(body),
                        )
                        .await;
                        if s.as_u16() != 200 {
                            return Err(format!("reserve: {s} {v}"));
                        }
                        let op = reference(&v)?;
                        acked
                            .lock()
                            .map_err(|e| e.to_string())?
                            .push((op.clone(), true));
                        let (path, body) = if i % 4 == 0 {
                            (
                                format!("/v1/reservations/{op}/capture"),
                                Some(json!({ "destination": to.account, "cleared": "2" })),
                            )
                        } else {
                            (format!("/v1/reservations/{op}/release"), None)
                        };
                        let (s, v) = ingress::testkit::call(
                            &r,
                            "POST",
                            &path,
                            t,
                            &[("client-key", &format!("{key}-end"))],
                            body,
                        )
                        .await;
                        if s.as_u16() != 200 {
                            return Err(format!("{path}: {s} {v}"));
                        }
                    } else {
                        let (s, v) = ingress::testkit::call(
                            &r,
                            "POST",
                            "/v1/transfers",
                            t,
                            &[("client-key", &key)],
                            Some(transfer(&from, &to, 1)),
                        )
                        .await;
                        if s.as_u16() != 202 && s.as_u16() != 200 {
                            return Err(format!("transfer: {s} {v}"));
                        }
                        acked
                            .lock()
                            .map_err(|e| e.to_string())?
                            .push((reference(&v)?, false));
                    }
                    // Spread out, and well below the core's throughput in a debug build on a
                    // shared runner: a saturated core grows its backlog round after round, which
                    // measures capacity, not recovery.
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                Ok::<(), String>(())
            }));
        }
        // Ten restarts of the core, with faults around the journal write, and recipients
        // flapping; the ingress replica replaced half way.
        let faults = [
            None,
            Some(Fault::CrashAfterJournal),
            Some(Fault::CrashAfterApply),
            Some(Fault::CrashBeforeJournal),
        ];
        let mut second = None;
        let mut worst = Duration::ZERO;
        // Each round hands out PER_ROUND operations while the core is up and DOWN more while it
        // is down, and is paced by the clients finishing them, not by the wall clock (issue #48):
        // on a slow runner a fixed sleep let rounds pass with little or no load in them, and
        // free-running clients piled up load for as long as the run lasted.
        const PER_ROUND: usize = 8;
        const DOWN: usize = 2;
        let mut allowed = 0;
        let max_seq = |rows: &[ingress::store::Row]| rows.iter().map(|r| r.seq).max().unwrap_or(0);
        for round in 0..10usize {
            // Load before each stop: PER_ROUND more operations, every one handed out acknowledged.
            allowed += PER_ROUND;
            budget.store(allowed, Ordering::SeqCst);
            let want = allowed;
            eventually!(format!("round {round}: the clients made no progress"), {
                let n = acknowledged.lock().map_err(|e| e.to_string())?.len();
                check(n >= want, || {
                    format!("{n} of {want} operations acknowledged")
                })
            })?;
            match faults[round % faults.len()] {
                Some(f) => {
                    // The fault fires on the core's next input; wait for it to fire.
                    d.core.inject(f);
                    eventually!(format!("round {round}: {f:?} never fired"), {
                        check(!d.core.is_up(), || "the core is still up".into())
                    })?;
                    d.core.stop();
                }
                None => d.core.stop(),
            }
            let at_stop = max_seq(&d.store.rows().await.map_err(|e| e.to_string())?);
            d.recipients.set_up(&bob.endpoint, round % 2 == 0);
            if round == 5 {
                d.replica.kill();
                let mut config = fast_config("ingress-1");
                config.decision_bound = d.replica.shared.config.decision_bound;
                let (r, surface) = d.another_replica(config);
                *router.lock().map_err(|e| e.to_string())? = surface;
                second = Some(r);
            }
            // A backlog to recover: at least one command received while the core is away.
            allowed += DOWN;
            budget.store(allowed, Ordering::SeqCst);
            eventually!(
                format!("round {round}: no command arrived while the core was down"),
                async {
                    let rows = d.store.rows().await.map_err(|e| e.to_string())?;
                    check(
                        rows.iter()
                            .any(|r| r.seq > at_stop && r.new.kind == InputKind::Command),
                        || format!("{} rows, none a command after {at_stop}", rows.len()),
                    )
                }
                .await
            )?;
            let rows_before = d
                .store
                .rows()
                .await
                .map_err(|e| e.to_string())?
                .iter()
                .map(|r| r.seq)
                .max()
                .unwrap_or(0);
            let starting = Instant::now();
            d.core.start()?;
            let back = Instant::now();
            let replay = back - starting;
            // REQ-CS-7: every row received while the core was away is decided within the bound.
            loop {
                let rows = d.store.rows().await.map_err(|e| e.to_string())?;
                if rows
                    .iter()
                    .filter(|r| r.seq <= rows_before)
                    .all(|r| r.status == Status::Decided)
                {
                    break;
                }
                check(back.elapsed() <= bound, || {
                    let stuck: Vec<String> = rows
                        .iter()
                        .filter(|r| r.status != Status::Decided)
                        .take(3)
                        .map(|r| format!("{}:{:?}:{:?}", r.seq, r.new.kind, r.status))
                        .collect();
                    let leader = second.as_ref().map_or(&d.replica.shared, |s| &s.shared);
                    let m = &leader.metrics;
                    let undecided = rows.iter().filter(|r| r.status != Status::Decided).count();
                    format!(
                        "round {round}: the backlog was not decided within {bound:?}; core up {}, \
                         second leads {:?}, first undecided {stuck:?} of {undecided}; the core \
                         replayed its journal in {replay:?} and holds {:?} inputs; the leader \
                         decided {}, retried {} forwards, ticked {}, delivered {}",
                        d.core.is_up(),
                        second.as_ref().map(|s| s.leads()),
                        d.core.inputs(),
                        m.decisions.load(Ordering::Relaxed),
                        m.forward_retries.load(Ordering::Relaxed),
                        m.ticks.load(Ordering::Relaxed),
                        m.delivered.load(Ordering::Relaxed),
                    )
                })?;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            worst = worst.max(back.elapsed());
        }
        stop.store(true, Ordering::SeqCst);
        for c in clients {
            c.await.map_err(|e| e.to_string())??;
        }
        d.recipients.set_all(true);
        check(d.quiesce(PATIENCE).await, || "undecided rows".into())?;
        check(d.all_delivered(PATIENCE).await, || {
            "undelivered notifications".into()
        })?;
        check(d.core.starts() == 11, || {
            format!("{} starts", d.core.starts())
        })?;
        // Every acknowledged operation reached a decision.
        let acked = acknowledged.lock().map_err(|e| e.to_string())?.clone();
        // Every operation handed out before the last stop was acknowledged.
        check(acked.len() >= allowed - DOWN, || {
            format!("too little load: {} of {}", acked.len(), allowed - DOWN)
        })?;
        for (r, reservation) in &acked {
            let op = ledger_core::id::OperationId::parse(r).map_err(|e| format!("{e:?}"))?;
            let state = d
                .core
                .with_ledger(|l| l.operation(&op).map(|o| o.state()))
                .flatten()
                .ok_or_else(|| format!("{r} is unknown to the core"))?;
            check(state.is_terminal(), || {
                format!("{r} (reservation: {reservation}) is {state:?}")
            })?;
        }
        // The input numbers are gapless, each held by one row. The ticker still runs, so the core
        // may have applied a tick whose row is not yet marked decided (issue #48: read at the
        // wrong instant, 451 inputs against 450 rows). Read the core's count first, then the
        // rows, until one reading sees every input the core counted held by exactly one row. A
        // real gap or a number held twice never goes away, and fails.
        let mut n = 0;
        eventually!(
            "the input numbers are not gapless",
            async {
                n = d.core.inputs().ok_or("the core is down")?;
                let numbers = d.input_numbers().await;
                check(numbers == (1..=n).collect::<Vec<u64>>(), || {
                    let held: BTreeSet<u64> = numbers.iter().copied().collect();
                    let missing: Vec<u64> = (1..=n).filter(|i| !held.contains(i)).take(5).collect();
                    format!(
                        "{} rows hold numbers against the core's {n} inputs; missing {missing:?}, \
                     held twice {}",
                        numbers.len(),
                        numbers.len() - held.len()
                    )
                })
            }
            .await
        )?;
        // Every owed notification was delivered at least once and is recorded.
        let items = outbox(&d).await?;
        let delivered = d.recipients.delivered_ids();
        for i in &items {
            check(i.delivered_at.is_some(), || {
                format!("{} is not recorded", i.notification.id)
            })?;
            check(delivered.contains(&i.notification.id), || {
                format!("{} never reached its recipient", i.notification.id)
            })?;
        }
        // And every recorded delivery row carried a notification the core knows.
        let rows = d.store.rows().await.map_err(|e| e.to_string())?;
        check(
            rows.iter()
                .filter(|r| r.new.kind == InputKind::Delivery)
                .count()
                >= items.len(),
            || "fewer delivery records than notifications".into(),
        )?;
        drop(second);
        println!(
            "F-009 exit criterion: {} operations, {} inputs, {} notifications, slowest backlog {:?}",
            acked.len(),
            n,
            items.len(),
            worst
        );
        Ok(())
    })
}

// --------------------------------------------------------------------- AC-M6.1–AC-M6.4

const ANSWERING: &str = "/v1/principals/self/answering";

/// Set `token`'s answering setting; the reply's status and body.
async fn set_answering(d: &Deployment, token: &str, body: Value) -> (u16, Value) {
    let (s, v) = d
        .call("POST", ANSWERING, Some(token), &[], Some(body))
        .await;
    (s.as_u16(), v)
}

/// `AC-M6.1` (`REQ-CS-9`, amendment 0013): with its answering setting at "decided", a principal's
/// reservation and transfer are answered with their decision, never `Received`, and a refusal is
/// named in the reply. Each reply is an `NFR-16` sample, none an `NFR-1` one.
fn ac_m6_1_a_decided_principal_gets_the_decision_in_the_reply__service(
    _h: &mut dyn Harness,
) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let bob = d.party("bob", 0).await;
        let account = alice.account.clone().ok_or("an account")?;
        let to = bob.account.clone().ok_or("an account")?;
        // A wait well inside the decision wait, generous for a shared debug runner (issue #48).
        let (s, v) = set_answering(
            &d,
            &alice.token,
            json!({ "answer": "decided", "wait_ms": 20_000 }),
        )
        .await;
        check(
            s == 200 && v["answer"] == "decided" && v["wait_ms"] == 20_000,
            || format!("the setting was not taken: {s} {v}"),
        )?;
        let metrics = &d.replica.shared.metrics;
        let acknowledged = metrics.acknowledgements().samples;
        let decided = metrics.decided_replies().samples;
        let (s, v) = d
            .call(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[],
                Some(json!({ "payer": account, "asset": USD, "amount": "100" })),
            )
            .await;
        check(
            s.as_u16() == 200 && v["state"] == "Reserved" && v["terminal"] == false,
            || format!("the reservation was not answered with its decision: {s} {v}"),
        )?;
        let (s, v) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&alice.token),
                &[],
                Some(json!({ "legs": [{ "from": account, "to": to, "asset": USD, "amount": "10" }] })),
            )
            .await;
        check(
            s.as_u16() == 200 && v["state"] == "Settled" && v["terminal"] == true,
            || format!("the transfer was not answered with its decision: {s} {v}"),
        )?;
        // A refusal is named in the reply, not left to a notification.
        let (s, v) = d
            .call(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[],
                Some(json!({ "payer": account, "asset": USD, "amount": "1000000" })),
            )
            .await;
        check(
            s.as_u16() == 422 && v["error"] == "ERR-InsufficientAvailable",
            || format!("the refusal was not in the reply: {s} {v}"),
        )?;
        check(
            metrics.acknowledgements().samples == acknowledged
                && metrics.decided_replies().samples == decided + 3,
            || "the replies were not measured as NFR-16's".to_owned(),
        )
    })
}

/// `AC-M6.2` (`REQ-CS-9`, `REQ-CS-7`): past the setting's wait the reply is
/// `ERR-DecisionUnavailable`; the command is kept, decided once the core returns, notified, and a
/// retry under the same client key returns the decision.
fn ac_m6_2_past_the_wait_the_command_is_kept_and_decided__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let account = alice.account.clone().ok_or("an account")?;
        let (s, v) = set_answering(
            &d,
            &alice.token,
            json!({ "answer": "decided", "wait_ms": 200 }),
        )
        .await;
        check(s == 200, || format!("{s} {v}"))?;
        d.core.stop();
        let body = json!({ "payer": account, "asset": USD, "amount": "100" });
        let key = [(ingress::surface::CLIENT_KEY, "auth-1")];
        let (s, v) = d
            .call(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &key,
                Some(body.clone()),
            )
            .await;
        check(s.as_u16() == 503, || format!("{s}: {v}"))?;
        let refusal = serde_json::from_value::<Refusal>(v.clone()).map_err(|e| e.to_string())?;
        crate::shape::check_refusal(
            &crate::spec::Catalogue::checked_in(),
            Err::<(), Refusal>(refusal),
            "ERR-DecisionUnavailable",
        )?;
        d.core.start().map_err(|e| e.to_string())?;
        let mut decided = Value::Null;
        eventually!(
            "the retry got the decision",
            async {
                let (s, v) = d
                    .call(
                        "POST",
                        "/v1/reservations",
                        Some(&alice.token),
                        &key,
                        Some(body.clone()),
                    )
                    .await;
                decided = v.clone();
                check(s.as_u16() == 200 && v["state"] == "Reserved", || {
                    format!("{s}: {v}")
                })
            }
            .await
        )?;
        let op = reference(&decided)?;
        check(d.all_delivered(PATIENCE).await, || {
            "not every notification was delivered".into()
        })?;
        let got = d.recipients.received(&alice.endpoint);
        check(
            got.fresh
                .iter()
                .any(|n| n.operation_reference == op && n.state == "Reserved"),
            || format!("no notification of {op} at Reserved"),
        )?;
        // Kept once: 900 is still available, 901 is not.
        let (s, v) = d
            .call(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[],
                Some(json!({ "payer": account, "asset": USD, "amount": "901" })),
            )
            .await;
        check(
            s.as_u16() == 422 && v["error"] == "ERR-InsufficientAvailable",
            || format!("the reservation was made twice or not at all: {s} {v}"),
        )
    })
}

/// `AC-M6.3` (`REQ-CS-9`): with no setting a principal's reservations are acknowledged at
/// `Received`; a command keeps the way it was taken in across a change of the setting, both ways.
fn ac_m6_3_acknowledged_by_default_and_as_taken_in__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let account = alice.account.clone().ok_or("an account")?;
        let (s, v) = d
            .call("GET", ANSWERING, Some(&alice.token), &[], None)
            .await;
        check(
            s.as_u16() == 200
                && v["answer"] == "acknowledged"
                && v["wait_ms"] == 1_000
                && v.get("set_by").is_none(),
            || format!("the default is not acknowledged: {s} {v}"),
        )?;
        let reserve = |key: &'static str| {
            let body = json!({ "payer": account, "asset": USD, "amount": "10" });
            let token = alice.token.clone();
            let d = &d;
            async move {
                d.call(
                    "POST",
                    "/v1/reservations",
                    Some(&token),
                    &[(ingress::surface::CLIENT_KEY, key)],
                    Some(body),
                )
                .await
            }
        };
        d.core.stop();
        // Taken in acknowledged.
        let (s, v) = reserve("before").await;
        check(s.as_u16() == 202 && v["state"] == "Received", || {
            format!("not acknowledged by default: {s} {v}")
        })?;
        let (s, v) = set_answering(
            &d,
            &alice.token,
            json!({ "answer": "decided", "wait_ms": 200 }),
        )
        .await;
        check(s == 200, || format!("{s} {v}"))?;
        // Re-answered as it was taken in: acknowledged, at once, though the core is down.
        let (s, v) = reserve("before").await;
        check(s.as_u16() == 202 && v["state"] == "Received", || {
            format!("a command taken in acknowledged was re-answered otherwise: {s} {v}")
        })?;
        // Taken in decided: past the wait, ERR-DecisionUnavailable.
        let (s, v) = reserve("after").await;
        check(s.as_u16() == 503, || {
            format!("a command taken in decided was not: {s} {v}")
        })?;
        let (s, v) = set_answering(&d, &alice.token, json!({ "answer": "acknowledged" })).await;
        check(s == 200 && v["answer"] == "acknowledged", || {
            format!("{s} {v}")
        })?;
        // Still answered with its decision: never Received.
        let (s, v) = reserve("after").await;
        check(s.as_u16() == 503, || {
            format!("a command taken in decided was re-answered otherwise: {s} {v}")
        })?;
        d.core.start().map_err(|e| e.to_string())?;
        eventually!(
            "the command taken in decided gets its decision",
            async {
                let (s, v) = reserve("after").await;
                check(s.as_u16() == 200 && v["state"] == "Reserved", || {
                    format!("{s}: {v}")
                })
            }
            .await
        )
    })
}

/// `AC-M6.4` (`REQ-CS-9`): a wait of zero, or above the deployment's decision wait, is refused
/// with `ERR-NotAuthorised`, and so is a body with a field the setting does not have; the setting
/// stays as it was.
fn ac_m6_4_a_wait_above_the_decision_wait_is_refused__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 0).await;
        let ceiling = u64::try_from(d.replica.shared.config.decision_wait.as_millis())
            .map_err(|e| e.to_string())?;
        for body in [
            json!({ "answer": "decided", "wait_ms": 0 }),
            json!({ "answer": "decided", "wait_ms": ceiling + 1 }),
        ] {
            let (s, v) = set_answering(&d, &alice.token, body.clone()).await;
            check(s == 403 && v["error"] == "ERR-NotAuthorised", || {
                format!("{body} was not refused: {s} {v}")
            })?;
        }
        // A field the setting does not have is a shape fault (amendment 0021, `REQ-ER-3`).
        let body = json!({ "answer": "decided", "commands": ["withdraw"] });
        let (s, v) = set_answering(&d, &alice.token, body.clone()).await;
        check(s == 400 && v["error"] == "ERR-MalformedCommand", || {
            format!("{body} was not refused as malformed: {s} {v}")
        })?;
        let (s, v) = d
            .call("GET", ANSWERING, Some(&alice.token), &[], None)
            .await;
        check(s.as_u16() == 200 && v["answer"] == "acknowledged", || {
            format!("the setting changed: {s} {v}")
        })?;
        // The ceiling itself is accepted.
        let (s, v) = set_answering(
            &d,
            &alice.token,
            json!({ "answer": "decided", "wait_ms": ceiling }),
        )
        .await;
        check(s == 200 && v["wait_ms"] == ceiling, || format!("{s} {v}"))
    })
}

// --------------------------------------------------------------------- ERR-MalformedCommand

/// One malformed request: what it shows, the path, the token, headers, the body, and `"raw"` for
/// a body that is not JSON at all.
type MalformedCase<'a> = (
    &'a str,
    &'a str,
    Option<&'a str>,
    Vec<(&'a str, &'a str)>,
    Option<Value>,
    &'a str,
);

/// `ERR-MalformedCommand` (amendment 0021, `REQ-ER-3`): every shape fault of a command is refused
/// with it, with status 400 and the published shape, and nothing is logged: a body that is not
/// JSON, an unknown field, a missing field, a wrong type, an identifier that does not parse, an
/// empty list that must not be, a client key over its bound, and a body over the invocation limit
/// for anything but a transfer. A value that parses but names nothing keeps its own condition.
fn err_malformed_command__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 100).await;
        let account = alice.account.clone().ok_or("an account")?;
        let operator = d.operator.token.clone();
        let rows = d.store.rows().await.map_err(|e| e.to_string())?.len();
        let long_key = "k".repeat(ingress::surface::MAX_CLIENT_KEY + 1);
        let big = json!({ "kind": "person", "verifier": "0a".repeat(9_000) });
        let cases: Vec<MalformedCase<'_>> = vec![
            (
                "not JSON",
                "/v1/reservations",
                Some(&alice.token),
                vec![],
                None,
                "raw",
            ),
            (
                "unknown field",
                "/v1/reservations",
                Some(&alice.token),
                vec![],
                Some(json!({ "payer": account, "asset": USD, "amount": "1", "colour": "red" })),
                "",
            ),
            (
                "missing field",
                "/v1/reservations",
                Some(&alice.token),
                vec![],
                Some(json!({ "payer": account, "asset": USD })),
                "",
            ),
            (
                "wrong type",
                "/v1/reservations",
                Some(&alice.token),
                vec![],
                Some(json!({ "payer": account, "asset": USD, "amount": 1 })),
                "",
            ),
            (
                "identifier that does not parse",
                "/v1/accounts",
                Some(&operator),
                vec![],
                Some(json!({ "owner": "not-a-did", "kind": "customer", "custody": "delegated" })),
                "",
            ),
            (
                "client key over its bound",
                "/v1/reservations",
                Some(&alice.token),
                vec![(ingress::surface::CLIENT_KEY, long_key.as_str())],
                Some(json!({ "payer": account, "asset": USD, "amount": "1" })),
                "",
            ),
            (
                "body over the limit",
                "/v1/identities",
                Some(&operator),
                vec![],
                Some(big),
                "",
            ),
        ];
        for (what, path, token, headers, body, raw) in cases {
            let (s, v) = if raw == "raw" {
                d.call_raw("POST", path, token, &headers, b"{not json".to_vec())
                    .await
            } else {
                d.call("POST", path, token, &headers, body).await
            };
            check(s.as_u16() == 400, || format!("{what}: {s} {v}"))?;
            let refusal =
                serde_json::from_value::<Refusal>(v.clone()).map_err(|e| format!("{what}: {e}"))?;
            crate::shape::check_refusal(
                &crate::spec::Catalogue::checked_in(),
                Err::<(), Refusal>(refusal),
                "ERR-MalformedCommand",
            )
            .map_err(|e| format!("{what}: {e}"))?;
        }
        check(
            d.store.rows().await.map_err(|e| e.to_string())?.len() == rows,
            || "a malformed command was logged".to_owned(),
        )?;
        // A value that parses but names nothing keeps its own condition.
        let (s, v) = d
            .call(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[],
                Some(json!({ "payer": account, "asset": USD, "amount": "1.5" })),
            )
            .await;
        check(
            s.as_u16() == 422 && v["error"] == "ERR-PrecisionExceeded",
            || format!("excess precision: {s} {v}"),
        )
    })
}

// --------------------------------------------------------------------- ERR-IdentifierArchived

/// `ERR-IdentifierArchived` (`REQ-RC-7`, amendment 0024): a request that names a prior system's
/// identifier the initial state left out (in a command's body, a path, a query parameter, or as an
/// alias's name, by the operator too) is refused with it, 410, with the closed list's shape, and
/// nothing of it is logged. An ordinary unknown account keeps `ERR-UnknownAccount`.
fn err_identifier_archived__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let archived = "legacy-account-7";
        let archived_owner = "urn:example:qa:7";
        let mut config = fast_config("ingress-0");
        config.archived = Arc::new(ingress::archived::ArchivedIds::of([
            archived,
            archived_owner,
        ]));
        let d = Deployment::start_with(config).await;
        let alice = d.party("alice", 100).await;
        let account = alice.account.clone().ok_or("an account")?;
        let operator = d.operator.token.clone();
        let rows = d.store.rows().await.map_err(|e| e.to_string())?.len();
        let alias_path = format!("/v1/ops/accounts/{account}/aliases");
        let grants_path = format!("/v1/accounts/{archived}/grants");
        let alias_query = format!("/v1/aliases?source=system-role&name={archived_owner}");
        let balances = format!("/v1/balances?account={archived}");
        let cases: Vec<(&str, &str, &str, &str, Option<Value>)> = vec![
            (
                "a command's body",
                "POST",
                "/v1/reservations",
                alice.token.as_str(),
                Some(json!({ "payer": archived, "asset": USD, "amount": "1" })),
            ),
            (
                "an alias's name, by the operator",
                "POST",
                alias_path.as_str(),
                operator.as_str(),
                Some(
                    json!({ "source": "system-role", "name": archived_owner, "justification": "x" }),
                ),
            ),
            (
                "a path",
                "GET",
                grants_path.as_str(),
                operator.as_str(),
                None,
            ),
            (
                "alias(source, name)",
                "GET",
                alias_query.as_str(),
                operator.as_str(),
                None,
            ),
            (
                "a query parameter",
                "GET",
                balances.as_str(),
                alice.token.as_str(),
                None,
            ),
        ];
        for (what, method, path, token, body) in cases {
            let (s, v) = d.call(method, path, Some(token), &[], body).await;
            check(s.as_u16() == 410, || format!("{what}: {s} {v}"))?;
            let refusal =
                serde_json::from_value::<Refusal>(v.clone()).map_err(|e| format!("{what}: {e}"))?;
            crate::shape::check_refusal(
                &crate::spec::Catalogue::checked_in(),
                Err::<(), Refusal>(refusal),
                "ERR-IdentifierArchived",
            )
            .map_err(|e| format!("{what}: {e}"))?;
        }
        check(
            d.store.rows().await.map_err(|e| e.to_string())?.len() == rows,
            || "a request naming an archived identifier was logged".to_owned(),
        )?;
        // An identifier never issued keeps its own condition.
        let (s, v) = d
            .call(
                "GET",
                "/v1/aliases?source=system-role&name=never-issued",
                Some(&operator),
                &[],
                None,
            )
            .await;
        check(
            s.as_u16() == 404 && v["error"] == "ERR-UnknownAccount",
            || format!("an unknown alias: {s} {v}"),
        )
    })
}

// --------------------------------------------------------------------- AC-M8.1–AC-M8.4

/// Pass-through data as an integrating party sends it: its order and graph node.
fn pass_through() -> Value {
    json!({ "order": "o-1", "graph": { "id": "g-7", "node": "transfer_single" } })
}

fn with_pass_through(mut body: Value, data: Value) -> Value {
    body["reference"] = data;
    body
}

/// The movements of `who`'s account that belong to `op`, as `who` reads them.
async fn movements_of(d: &Deployment, who: &Party, op: &str) -> Result<Vec<Value>> {
    let a = who.account.as_deref().unwrap_or_default();
    let (s, page) = d
        .call(
            "GET",
            &format!("/v1/movements?account={a}&limit=50"),
            Some(&who.token),
            &[],
            None,
        )
        .await;
    check(s.as_u16() == 200, || format!("movements: {s} {page}"))?;
    Ok(page["movements"]
        .as_array()
        .map(|m| m.iter().filter(|m| m["operation"] == op).cloned().collect())
        .unwrap_or_default())
}

async fn command_rows(d: &Deployment) -> Result<usize> {
    Ok(d.store
        .rows()
        .await
        .map_err(|e| format!("the store: {e}"))?
        .iter()
        .filter(|r| r.new.kind == InputKind::Command)
        .count())
}

/// `AC-M8.1` (`REQ-PO-5`, amendment 0020 R1): a transfer's pass-through data comes back in its
/// acknowledgement and its decided reply, in `operation(reference)`, in every notification of its
/// transitions (inside the body the recipient verified) and in its movements.
fn ac_m8_1_pass_through_comes_back_everywhere_its_owner_reads__service(
    _h: &mut dyn Harness,
) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let bob = d.party("bob", 0).await;
        let a = Some(alice.token.as_str());
        let key = [("client-key", "m8-1")];
        let body = with_pass_through(transfer(&alice, &bob, 5), pass_through());
        let (s, ack) = d
            .call("POST", "/v1/transfers", a, &key, Some(body.clone()))
            .await;
        check(
            s.as_u16() == 202 && ack["reference"] == pass_through(),
            || format!("the acknowledgement: {s} {ack}"),
        )?;
        let op = reference(&ack)?;
        let (s, decided) = d
            .call_decided("POST", "/v1/transfers", a, &key, Some(body))
            .await;
        check(
            s.as_u16() == 200 && decided["reference"] == pass_through(),
            || format!("the decided reply: {s} {decided}"),
        )?;
        let (s, got) = d
            .call("GET", &format!("/v1/operations/{op}"), a, &[], None)
            .await;
        check(
            s.as_u16() == 200 && got["reference"] == pass_through(),
            || format!("the operation: {s} {got}"),
        )?;
        eventually!(
            "alice's notifications carry it",
            async {
                let fresh = d.recipients.received(&alice.endpoint).fresh;
                let mine: Vec<_> = fresh
                    .iter()
                    .filter(|n| n.operation_reference == op)
                    .collect();
                let carried = mine.iter().all(|n| {
                    n.reference
                        .as_ref()
                        .and_then(|r| serde_json::from_str::<Value>(r.get()).ok())
                        == Some(pass_through())
                });
                check(!mine.is_empty() && carried, || {
                    format!("{} notifications, all carrying it: {carried}", mine.len())
                })
            }
            .await
        )?;
        let moved = movements_of(&d, &alice, &op).await?;
        check(
            !moved.is_empty() && moved.iter().all(|m| m["reference"] == pass_through()),
            || format!("alice's movements: {moved:?}"),
        )
    })
}

/// `AC-M8.2` (`REQ-PO-5`, `REQ-OP-10`): a repeat under the same client key with different
/// pass-through data, or with none, is refused with `ERR-ClientKeyReused`; with the same data it
/// is the same request.
fn ac_m8_2_different_pass_through_under_one_key_is_refused__service(
    _h: &mut dyn Harness,
) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let a = Some(alice.token.as_str());
        let key = [("client-key", "m8-2")];
        let reserve = json!({ "payer": alice.account, "asset": USD, "amount": "7" });
        let (s, first) = d
            .call(
                "POST",
                "/v1/reservations",
                a,
                &key,
                Some(with_pass_through(reserve.clone(), pass_through())),
            )
            .await;
        check(s.as_u16() == 202, || format!("reserve: {s} {first}"))?;
        let (s, same) = d
            .call(
                "POST",
                "/v1/reservations",
                a,
                &key,
                Some(with_pass_through(reserve.clone(), pass_through())),
            )
            .await;
        check(
            s.is_success() && same["operation_reference"] == first["operation_reference"],
            || format!("the same request: {s} {same}"),
        )?;
        let refused = json!({
            "error": Refusal::ClientKeyReused.code(),
            "description": Refusal::ClientKeyReused.description(),
        });
        for other in [
            with_pass_through(reserve.clone(), json!({ "order": "o-2" })),
            reserve.clone(),
        ] {
            let (s, r) = d
                .call("POST", "/v1/reservations", a, &key, Some(other))
                .await;
            check(s.as_u16() == 409 && r == refused, || {
                format!("another request: {s} {r}")
            })?;
        }
        Ok(())
    })
}

/// `AC-M8.3` (`REQ-PO-5`, `REQ-ER-3`): pass-through data over 1 KiB, or not one JSON value, is
/// refused with `ERR-MalformedCommand`, and nothing is taken in. 1 KiB is taken.
fn ac_m8_3_pass_through_over_the_bound_is_refused__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let bob = d.party("bob", 0).await;
        let a = Some(alice.token.as_str());
        // A JSON string of n bytes, its quotes included.
        let of_len = |n: usize| Value::String("x".repeat(n - 2));
        let (s, r) = d
            .call(
                "POST",
                "/v1/transfers",
                a,
                &[],
                Some(with_pass_through(transfer(&alice, &bob, 1), of_len(1024))),
            )
            .await;
        check(s.as_u16() == 202, || format!("1 024 bytes: {s} {r}"))?;
        let rows = command_rows(&d).await?;
        let malformed = json!({
            "error": Refusal::MalformedCommand.code(),
            "description": Refusal::MalformedCommand.description(),
        });
        let (s, r) = d
            .call(
                "POST",
                "/v1/transfers",
                a,
                &[],
                Some(with_pass_through(transfer(&alice, &bob, 1), of_len(1025))),
            )
            .await;
        check(s.as_u16() == 400 && r == malformed, || {
            format!("1 025 bytes: {s} {r}")
        })?;
        let not_a_value = format!(
            r#"{{"legs":[{{"from":"{}","to":"{}","asset":"USD","amount":"1"}}],"reference":{{"order":}}}}"#,
            alice.account.as_deref().unwrap_or_default(),
            bob.account.as_deref().unwrap_or_default(),
        );
        let (s, r) = d
            .call_raw("POST", "/v1/transfers", a, &[], not_a_value.into_bytes())
            .await;
        check(s.as_u16() == 400 && r == malformed, || {
            format!("not one value: {s} {r}")
        })?;
        let after = command_rows(&d).await?;
        check(after == rows, || {
            format!("nothing taken in: {rows} rows before, {after} after")
        })
    })
}

/// `AC-M8.4` (`REQ-PO-5`): the payee, a principal of another party, reads its own movement of the
/// operation without the pass-through data, and is not shown it on the operation. The credit
/// notification half of this criterion waits for credit notifications (`REQ-EV-9`, `T-009-22`).
fn ac_m8_4_another_party_never_sees_the_pass_through__service(_h: &mut dyn Harness) -> Outcome {
    run(|| async {
        let d = Deployment::start().await;
        let alice = d.party("alice", 1_000).await;
        let bob = d.party("bob", 0).await;
        let body = with_pass_through(transfer(&alice, &bob, 5), pass_through());
        let (s, t) = d
            .call_decided("POST", "/v1/transfers", Some(&alice.token), &[], Some(body))
            .await;
        check(s.as_u16() == 200 && t["state"] == "Settled", || {
            format!("transfer: {s} {t}")
        })?;
        let op = reference(&t)?;
        let theirs = movements_of(&d, &bob, &op).await?;
        check(
            !theirs.is_empty() && theirs.iter().all(|m| m.get("reference").is_none()),
            || format!("bob's movements: {theirs:?}"),
        )?;
        let (_, got) = d
            .call(
                "GET",
                &format!("/v1/operations/{op}"),
                Some(&bob.token),
                &[],
                None,
            )
            .await;
        check(got.get("reference").is_none(), || {
            format!("bob's read of the operation: {got}")
        })
    })
}
