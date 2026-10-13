// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The forwarder, the ticker and leadership (`T-009-03`, `T-009-06`): the core killed
//! mid-forward, every row decided once with gapless input numbers; deadlines that pass only after
//! a tick; a killed leader replaced within one interval; an ingress replica restarted with
//! nothing lost.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use ingress::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, InputKey, OutboxPage, Query,
    QueryReply,
};
use ingress::inprocess::{Fault, InProcessCore, MemJournal};
use ingress::service::tick_once;
use ingress::store::mem::MemStore;
use ingress::store::{InputKind, Status, Store};
use ingress::testkit::{Deployment, T0, TestCore, USD, fast_config, genesis, test_clerk};
use ledger_core::account::Time;
use ledger_core::id::OperationId;
use ledger_core::lifecycle::State;
use serde_json::json;

/// `T-009-03`, `REQ-CS-7`, `INV-18`, `DEC-5`: the core is killed mid-forward — before its journal
/// write, after it, and after the apply with the reply lost — and restarted each time. Every row
/// is decided, none twice, and the input numbers the rows hold are exactly `1..=N`, where `N` is
/// the number of inputs the core applied.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_03_a_core_killed_mid_forward_decides_every_row_once_with_gapless_numbers() {
    let d = Arc::new(Deployment::start().await);
    let alice = d.party("alice", 10_000).await;
    let bob = d.party("bob", 10_000).await;
    let faults = [
        Fault::CrashBeforeJournal,
        Fault::CrashAfterJournal,
        Fault::CrashAfterApply,
    ];
    let mut references = Vec::new();
    for round in 0..9 {
        d.core.inject(faults[round % faults.len()]);
        for i in 0..4 {
            let (from, to, token) = if i % 2 == 0 {
                (&alice, &bob, &alice.token)
            } else {
                (&bob, &alice, &bob.token)
            };
            let (s, r) = d
                .call(
                    "POST",
                    "/v1/transfers",
                    Some(token),
                    &[("client-key", &format!("r{round}-{i}"))],
                    Some(json!({ "legs": [{ "from": from.account, "to": to.account, "asset": USD, "amount": "1" }] })),
                )
                .await;
            assert_eq!(s, StatusCode::ACCEPTED, "{r}");
            references.push(r["operation_reference"].as_str().unwrap().to_owned());
        }
        // Let the fault fire, then bring the core back.
        let until = Instant::now() + Duration::from_secs(5);
        while d.core.is_up() && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        d.core.start().unwrap();
    }
    assert!(
        d.quiesce(Duration::from_secs(30)).await,
        "every row decided"
    );
    // Gapless: the rows hold 1..=N, each once, read at one consistent instant (issue #48).
    if let Err((numbers, n)) = d
        .gapless_inputs(|| d.core.inputs(), Duration::from_secs(10))
        .await
    {
        panic!("rows hold {numbers:?} against the core's {n} inputs");
    }
    // None decided twice: each transfer settled once, with one notification per transition.
    let per_op = d
        .core
        .with_ledger(|l| {
            let mut per: BTreeMap<OperationId, usize> = BTreeMap::new();
            for n in l.state().operations().outbox() {
                *per.entry(n.op).or_default() += 1;
            }
            per
        })
        .unwrap();
    for r in &references {
        let op = OperationId::parse(r).unwrap();
        let state = d
            .core
            .with_ledger(|l| l.operation(&op).map(|o| o.state()))
            .unwrap();
        assert_eq!(state, Some(State::Settled), "{r}");
        assert_eq!(
            per_op.get(&op),
            Some(&2),
            "{r}: Received→Accepted, Accepted→Settled"
        );
    }
    assert!(d.core.starts() >= 10);
}

/// `AC-C4.1` with `F-007`, `INV-15`, `DEC-6`: a reservation's deadline passes only after a tick.
/// The core reads no clock: past the deadline by the ingress clock, the reservation is untouched
/// until the ticker's input arrives; after it, it is overdue and in the review queue with an owner
/// and a service level.
#[tokio::test(flavor = "multi_thread")]
async fn ac_c4_1_a_deadline_passes_only_after_a_tick() {
    let mut config = fast_config("ingress-0");
    config.tick_interval = Duration::from_secs(3_600);
    let d = Deployment::start_with(config).await;
    let alice = d.party("alice", 1_000).await;
    let (s, r) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[],
            Some(json!({ "payer": alice.account, "asset": USD, "amount": "10", "deadline": T0 + 1_000 })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    assert!(d.quiesce(Duration::from_secs(5)).await);
    let op = OperationId::parse(r["operation_reference"].as_str().unwrap()).unwrap();
    d.clock.set(T0 + 10_000);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (overdue, queued) = d
        .core
        .with_ledger(|l| {
            let clock = l.state().operations().clock().unwrap_or(0);
            (
                l.operation(&op).unwrap().overdue(),
                l.review_queue(clock).iter().any(|e| e.op == op),
            )
        })
        .unwrap();
    assert!(!overdue && !queued, "no tick, no deadline");
    assert!(tick_once(&d.replica.shared).await);
    assert!(d.quiesce(Duration::from_secs(5)).await);
    let entry = d
        .core
        .with_ledger(|l| {
            let clock = l.state().operations().clock().unwrap();
            assert_eq!(clock, T0 + 10_000, "the tick carried the ingress clock");
            l.review_queue(clock).into_iter().find(|e| e.op == op)
        })
        .unwrap()
        .expect("in the review queue");
    // Past its deadline the reservation moved to Needs-Review (amendment 0006 item 5).
    assert_eq!(entry.shown.state, State::NeedsReview);
    assert_eq!(entry.owner.to_did(), d.operator.identity);
    // The ticker age is readable (RISK-7).
    assert_eq!(d.core.last_tick().await.unwrap(), Some(T0 + 10_000));
    let (_, m) = d
        .call("GET", "/metrics", Some(&d.operator.token), &[], None)
        .await;
    assert!(
        m.as_str()
            .unwrap()
            .contains("aledgent_ingress_ticker_age_ms 0")
    );
}

/// `T-009-06`, `RISK-7`: only the leader ticks; a killed leader is replaced within one interval,
/// and ticks carry on.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_06_a_killed_leader_is_replaced_within_one_interval() {
    let mut d = Deployment::start().await;
    let (mut second, _) = d.another_replica(fast_config("ingress-1"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(d.replica.leads() ^ second.leads(), "exactly one leader");
    let interval = fast_config("x").tick_interval;
    // Whichever leads, kill it.
    let first_led = d.replica.leads();
    let killed_at = Instant::now();
    if first_led {
        d.replica.kill();
    } else {
        second.kill();
    }
    let survivor = |d: &Deployment, s: &ingress::service::Replica<_, _>| {
        if first_led {
            s.leads()
        } else {
            d.replica.leads()
        }
    };
    while !survivor(&d, &second) {
        assert!(
            killed_at.elapsed() < interval * 3,
            "no replacement within the lease and one renewal"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let took = killed_at.elapsed();
    assert!(
        took <= interval + interval / 2,
        "replaced in {took:?}, interval {interval:?}"
    );
    // Ticks carry on under the new leader.
    d.clock.set(T0 + 50_000);
    let until = Instant::now() + Duration::from_secs(5);
    while d.core.last_tick().await.unwrap() != Some(T0 + 50_000) {
        assert!(Instant::now() < until, "the new leader ticks");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Only one replica forwarded at any time: the rows are decided once each.
    let numbers = d.input_numbers().await;
    let mut dedup = numbers.clone();
    dedup.dedup();
    assert_eq!(numbers, dedup);
}

/// `REQ-AR-7`, `NFR-5`: an ingress replica killed with commands logged and undecided loses
/// none of them; a new replica decides them, and a caller retrying gets the decision.
#[tokio::test(flavor = "multi_thread")]
async fn req_ar_7_a_restarted_ingress_replica_loses_no_received_command() {
    let mut d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    d.core.stop();
    let body = json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "2" }] });
    for i in 0..5 {
        let (s, _) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&alice.token),
                &[("client-key", &format!("k{i}"))],
                Some(body.clone()),
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED);
    }
    d.replica.kill();
    d.core.start().unwrap();
    let (_new, surface) = d.another_replica(fast_config("ingress-2"));
    assert!(d.quiesce(Duration::from_secs(10)).await);
    let rows = d.store.rows().await.unwrap();
    assert!(
        rows.iter()
            .filter(|r| r.new.kind == InputKind::Command)
            .all(|r| r.status == Status::Decided)
    );
    let (s, r) = ingress::testkit::call(
        &surface,
        "POST",
        "/v1/transfers",
        Some(&alice.token),
        &[("client-key", "k0")],
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["state"], "Settled");
}

/// The in-process core, handed batches (`T-031-04`): it records each batch's keys, can hold them
/// back so that rows pile up, and can fail one position of the next batch long enough to have it,
/// before that input runs.
#[derive(Clone)]
struct Batches {
    inner: TestCore,
    seen: Arc<Mutex<Vec<Vec<InputKey>>>>,
    hold: Arc<AtomicBool>,
    fail_at: Arc<Mutex<Option<usize>>>,
    /// Run at most this many inputs a call, and answer the rest `Unavailable` without running
    /// them, as the Agent core does past one invocation (`T-031-18`).
    cap: Arc<Mutex<Option<usize>>>,
}

impl Core for Batches {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        self.inner.apply(input).await
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        while self.hold.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        self.seen
            .lock()
            .unwrap()
            .push(inputs.iter().map(|i| i.key).collect());
        let fail = {
            let mut f = self.fail_at.lock().unwrap();
            match *f {
                Some(k) if k < inputs.len() => f.take(),
                _ => None,
            }
        };
        let cap = self.cap.lock().unwrap().unwrap_or(usize::MAX);
        let mut out = Vec::new();
        for (k, input) in inputs.iter().enumerate() {
            if fail == Some(k) || k >= cap {
                break;
            }
            let r = self.inner.apply(input).await;
            let failed = r.is_err();
            out.push(r);
            if failed {
                break;
            }
        }
        out.resize(inputs.len(), Err(CoreError::Unavailable));
        out
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> Result<OutboxPage, CoreError> {
        self.inner.outbox(cursor, undelivered, now).await
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        self.inner.last_tick().await
    }

    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        self.inner.query(caller, query, now).await
    }

    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        self.inner.credential(credential).await
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        self.inner.party_of(principal).await
    }
}

/// `T-031-04`, `REQ-CS-7`, `INV-18`: rows that pile up while the core is busy are forwarded in
/// batches, in log order; the core fails one input of a batch, the rows before it are decided and
/// it and the rest are retried. Every row is decided once, the input numbers are gapless, and no
/// row holds a smaller number than a row logged before it.
#[tokio::test(flavor = "multi_thread")]
async fn t_031_04_the_forwarder_decides_rows_in_log_order_in_batches() {
    let journal = MemJournal::new();
    let inner = InProcessCore::in_memory(genesis(T0), journal.clone()).with_clerk(test_clerk());
    inner.start().unwrap();
    let founded = inner.founded().unwrap();
    let core = Batches {
        inner,
        seen: Arc::default(),
        hold: Arc::default(),
        fail_at: Arc::default(),
        cap: Arc::default(),
    };
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        founded,
        journal,
    )
    .await;
    let alice = d.party("alice", 10_000).await;
    let bob = d.party("bob", 10_000).await;
    core.hold.store(true, Ordering::SeqCst);
    *core.fail_at.lock().unwrap() = Some(5);
    for i in 0..12 {
        let (from, to, token) = if i % 2 == 0 {
            (&alice, &bob, &alice.token)
        } else {
            (&bob, &alice, &bob.token)
        };
        let (s, r) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(token),
                &[("client-key", &format!("b{i}"))],
                Some(json!({ "legs": [{ "from": from.account, "to": to.account, "asset": USD, "amount": "1" }] })),
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    }
    core.hold.store(false, Ordering::SeqCst);
    assert!(
        d.quiesce(Duration::from_secs(30)).await,
        "every row decided"
    );
    if let Err((numbers, n)) = d
        .gapless_inputs(|| d.core.inner.inputs(), Duration::from_secs(10))
        .await
    {
        panic!("rows hold {numbers:?} against the core's {n} inputs");
    }
    // Nothing here reserves: the forwarding order is the arrival order.
    let rows = d.store.rows().await.unwrap();
    assert!(rows.iter().all(|r| !r.new.reserves));
    // A tick or a delivery record the replica logged after the quiesce may still be undecided.
    let numbers: Vec<u64> = rows.iter().filter_map(|r| r.core_input_seq).collect();
    assert!(
        numbers.windows(2).all(|w| w[0] < w[1]),
        "decided out of log order: {numbers:?}"
    );
    let seen = core.seen.lock().unwrap().clone();
    assert!(
        seen.iter().any(|b| b.len() > 5),
        "the rows were batched: {:?}",
        seen.iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert!(
        core.fail_at.lock().unwrap().is_none(),
        "the failure was injected"
    );
}

/// `T-031-18`, `REQ-CS-7`: a core that runs only the first inputs of a batch and answers the rest
/// `Unavailable` without running them (the Agent core, one invocation a call) has every row
/// decided, in log order, and the forwarder sends the rest again at once: no retry, no backoff.
#[tokio::test(flavor = "multi_thread")]
async fn t_031_18_a_core_that_runs_part_of_a_batch_is_fed_the_rest_without_backoff() {
    let journal = MemJournal::new();
    let inner = InProcessCore::in_memory(genesis(T0), journal.clone()).with_clerk(test_clerk());
    inner.start().unwrap();
    let founded = inner.founded().unwrap();
    let core = Batches {
        inner,
        seen: Arc::default(),
        hold: Arc::default(),
        fail_at: Arc::default(),
        cap: Arc::new(Mutex::new(Some(2))),
    };
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        founded,
        journal,
    )
    .await;
    let alice = d.party("alice", 10_000).await;
    let bob = d.party("bob", 10_000).await;
    let retries = d
        .replica
        .shared
        .metrics
        .forward_retries
        .load(Ordering::SeqCst);
    core.hold.store(true, Ordering::SeqCst);
    for i in 0..12 {
        let (s, r) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&alice.token),
                &[("client-key", &format!("c{i}"))],
                Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "1" }] })),
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    }
    core.hold.store(false, Ordering::SeqCst);
    assert!(
        d.quiesce(Duration::from_secs(30)).await,
        "every row decided"
    );
    assert_eq!(
        d.replica
            .shared
            .metrics
            .forward_retries
            .load(Ordering::SeqCst),
        retries,
        "a partial answer is not a retry"
    );
    let rows = d.store.rows().await.unwrap();
    let numbers: Vec<u64> = rows.iter().map(|r| r.core_input_seq.unwrap()).collect();
    assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
}
