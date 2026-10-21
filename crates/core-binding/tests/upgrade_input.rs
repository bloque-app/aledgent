// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The `Upgrade` input, and the way out, through ingress's `CTR-OPS` routes end to end
//! (`T-037-04`; `REQ-RT-16`, `REQ-RT-17`, `REQ-RT-20`, `REQ-AT-3`; `AC-K7.1`, `AC-K7.2`,
//! `AC-K7.16`; amendment 0035; reviews of #44, F1 and N5): a real deployment (surface, replica,
//! store, the forwarder's sequenced feed) whose core is the host's [`Sequencer`] over
//! [`AgentCore`] over the native Agent, the executor the core host runs, with a source of code the
//! node stages from.
//!
//! - The code is staged **before** the command is logged: an upgrade whose code cannot be had, or
//!   names another generation, or whose package carries another constant, is refused at the door and
//!   leaves nothing in the received log.
//! - The door asks the core whether the caller may take the code in hand before it stages or
//!   prioritises anything; a caller who may not is logged as any command is, and refused and
//!   recorded by the writer.
//! - A way out logged behind inputs already `Received` reaches a fenced writer before them, and it
//!   is its input's only call through the real executor, so it is decided, not malformed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use core_binding::core::{AgentCore, Founding, MemLog};
use core_binding::host::{AgentPort, NativePort};
use core_binding::hot::{Hot, Limits, MAX_BUDGET};
use ingress::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, InputBody, InputKey,
    OutboxPage, Query, QueryReply, Staged,
};
use ingress::feed::{Feed, FeedReply, Sequencer};
use ingress::inprocess::MemJournal;
use ingress::store::mem::MemStore;
use ingress::store::{Status, Store as _};
use ingress::testkit::{Deployment, Party, T0, call, fast_config, genesis};
use serde_json::{Value, json};

mod common;
use common::HeldCode;

/// The host's core: the sequencer in front of the Agent core, as `aledgent-core-host serve` runs
/// it, so a feed is ordered by its sequences and a refused input gives its sequence back.
#[derive(Clone)]
struct Node(Arc<Sequencer<AgentCore<NativePort>>>, Arc<AtomicBool>);

impl Core for Node {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        self.0.apply(input).await
    }
    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        self.0.apply_batch(inputs).await
    }
    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: u64,
    ) -> Result<OutboxPage, CoreError> {
        self.0.outbox(cursor, undelivered, now).await
    }
    async fn last_tick(&self) -> Result<Option<u64>, CoreError> {
        self.0.last_tick().await
    }
    async fn next_due(&self) -> Result<Option<u64>, CoreError> {
        self.0.next_due().await
    }
    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: u64,
    ) -> Result<QueryReply, CoreError> {
        self.0.query(caller, query, now).await
    }
    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        self.0.credential(credential).await
    }
    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        self.0.party_of(principal).await
    }
    async fn genesis_open(&self) -> Result<bool, CoreError> {
        self.0.genesis_open().await
    }
    async fn stage_code(&self, code: &[u8; 32]) -> Result<Staged, CoreError> {
        self.0.stage_code(code).await
    }
    async fn feed(&self, feed: &Feed) -> FeedReply {
        let reply = self.0.feed(feed).await;
        // The link failing after the core ran the feed: the feeder is told it may have run, and
        // keeps the rows' sequences (`F-038` §3).
        if self.1.swap(false, Ordering::SeqCst) {
            return FeedReply::Ambiguous;
        }
        reply
    }
}

type Dep = Deployment<MemStore, Node>;

struct World {
    /// Arm it, and the next feed the core answers is reported ambiguous to ingress.
    ambiguous_next: Arc<AtomicBool>,
    d: Dep,
    core: AgentCore<NativePort>,
    code: HeldCode,
}

fn founding() -> Founding {
    let g = genesis(T0);
    Founding {
        genesis: g.genesis,
        op: g.op,
        at: g.at,
        keys: core_binding::host::HostKeys::local(g.root, None),
        limits: Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    }
}

/// A deployment over the sequenced Agent core, a source of code, and a ticker that never ticks:
/// the inputs are the test's own.
async fn world() -> World {
    let ambiguous_next = Arc::new(AtomicBool::new(false));
    let core = AgentCore::open(NativePort::new(), founding(), MemLog::default())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let code = HeldCode::default();
    core.set_code_source(Arc::new(code.clone())).unwrap();
    let mut config = fast_config("ingress-0");
    // A command the writer cannot decide yet answers `ERR-DecisionUnavailable` after this.
    config.decision_wait = Duration::from_millis(400);
    // No tick within a test: the inputs are the test's own.
    config.tick_interval = Duration::from_secs(365 * 86_400);
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        config,
        Node(
            Arc::new(Sequencer::new(core.clone(), None)),
            ambiguous_next.clone(),
        ),
        core.founded(),
        MemJournal::new(),
    )
    .await;
    World {
        ambiguous_next,
        d,
        core,
        code,
    }
}

fn hot(w: &World) -> Hot {
    w.core
        .with_port(|p| p.hot())
        .and_then(Result::ok)
        .flatten()
        .expect("the Agent is founded")
}

fn operator(w: &World) -> String {
    w.d.operator.token.clone()
}

fn upgrade_body(code: &[u8; 32], from: u64) -> Value {
    json!({ "code": hex::encode(code), "from": from, "justification": "T-037-04: the next code" })
}

/// How many commands the received log holds.
async fn logged(w: &World) -> usize {
    w.d.store.rows().await.unwrap().len()
}

/// The error condition a refusal names, `ERR-…`.
fn condition(v: &Value) -> String {
    v.to_string()
}

async fn until<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..1_000 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `AC-K7.2` and `AC-K7.16` (second part), through `CTR-OPS`: an upgrade whose code no source
/// returns, or returns with another digest, or names another generation than the writer's plus one,
/// or whose package carries another constant, is refused **before it is logged**: the received
/// log does not change, and the writer's code does not.
#[tokio::test(flavor = "multi_thread")]
async fn ac_k7_2_ac_k7_16_an_upgrade_that_cannot_be_staged_is_refused_before_it_is_logged() {
    let w = world().await;
    let op = operator(&w);
    let before = logged(&w).await;
    let g = hot(&w);
    let mut tries: Vec<(&str, [u8; 32])> = Vec::new();
    // No source returns the manifest.
    tries.push(("no source returns it", [0xB1; 32]));
    // Another generation than the writer's plus one: 0, 2, 7.
    for gen_ in [0, 2, 7] {
        tries.push(("another generation", w.code.version(gen_)));
    }
    // A package that carries another constant than its manifest names.
    let (p, r) = (common::code_package(2), common::CODE_RUNTIME.to_vec());
    let m = common::code_manifest(1, &p, &r);
    for b in [&p, &r] {
        w.code.put(b);
    }
    w.code.put(&m.to_bytes());
    tries.push(("another constant", m.version()));
    // An artefact under another digest: the right generation, the wrong bytes.
    let good = w.code.version(1);
    let m = ledger_core::code::CodeManifest::from_bytes(&ingress_fetch(&w.code, &good)).unwrap();
    let package = m.artefact(ledger_core::code::Role::Package).unwrap().digest;
    let mut bytes = ingress_fetch(&w.code, &package);
    bytes[0] ^= 1;
    w.code.lie(package, &bytes);
    tries.push(("another digest", good));
    for (why, code) in tries {
        let (s, v) =
            w.d.call(
                "POST",
                "/v1/ops/upgrade",
                Some(&op),
                &[],
                Some(upgrade_body(&code, g.inputs + 50)),
            )
            .await;
        assert_eq!(s.as_u16(), 409, "{why}: {v}");
        assert!(
            condition(&v).contains("ERR-InvalidTransition"),
            "{why}: {v}"
        );
    }
    assert_eq!(logged(&w).await, before, "nothing was logged");
    let h = hot(&w);
    assert_eq!((h.code, h.generation, h.upgrade), (g.code, 0, None));
    assert_eq!(h.inputs, g.inputs, "the writer saw none of them");
    assert!(
        w.core
            .with_port(|p| p.code_map.is_empty())
            .unwrap_or_default(),
        "nothing was staged"
    );
}

fn ingress_fetch(code: &HeldCode, digest: &[u8; 32]) -> Vec<u8> {
    use core_binding::staging::CodeFetch as _;
    code.fetch(digest, None).expect("held")
}

/// The door asks the core whether the caller may take the code in hand **before** it stages or
/// prioritises anything (review of #44, N5): a holder's upgrade is logged like any command, stages
/// nothing (its code is not even held), and the writer refuses it and records the refusal.
#[tokio::test(flavor = "multi_thread")]
async fn t037_04_a_caller_who_may_not_stages_nothing_and_is_refused_by_the_writer() {
    let w = world().await;
    let alice: Party = w.d.party("alice", 0).await;
    let before = logged(&w).await;
    let (s, v) =
        w.d.call(
            "POST",
            "/v1/ops/upgrade",
            Some(&alice.token),
            &[("client-key", "alice-upgrade")],
            // Code nobody holds: an operator's would be refused at the door as unstageable.
            Some(upgrade_body(&[0xB1; 32], hot(&w).inputs + 50)),
        )
        .await;
    assert_eq!(s.as_u16(), 403, "{v}");
    assert!(condition(&v).contains("ERR-NotAuthorised"), "{v}");
    assert_eq!(logged(&w).await, before + 1, "it was logged, and decided");
    assert_eq!(hot(&w).upgrade, None);
    // And the refusal is recorded by the writer: the input took its number.
    let rows = w.d.store.rows().await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.new.client_key.as_deref() == Some("alice-upgrade"))
        .unwrap();
    assert!(!row.new.overtakes);
    assert_eq!(row.status, Status::Decided);
    assert!(row.core_input_seq.is_some());
}

/// `AC-K7.1`, `REQ-RT-16`, `REQ-RT-20`, through `CTR-OPS`: the operator upgrades to a version the
/// node stages first; the writer schedules it; the host switches before input `n`; input `n` and
/// those after it run under *H*.
#[tokio::test(flavor = "multi_thread")]
async fn ac_k7_1_an_upgrade_through_ops_is_staged_scheduled_switched_and_in_force_from_n() {
    let w = world().await;
    let op = operator(&w);
    let g = hot(&w);
    let h1 = w.code.version(1);
    let n = g.inputs + 4;
    let before = logged(&w).await;
    let (s, v) =
        w.d.call(
            "POST",
            "/v1/ops/upgrade",
            Some(&op),
            &[],
            Some(upgrade_body(&h1, n)),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(logged(&w).await, before + 1);
    let h = hot(&w);
    assert_eq!(
        (h.code, h.generation, h.upgrade),
        (g.code, 0, Some((h1, n)))
    );
    // Staged on the node: the version the switch will install is held.
    assert_eq!(
        w.core.with_port(|p| p.code_map.get(&h1).copied()).flatten(),
        Some(1)
    );
    // Ordinary inputs up to n − 1 run under the old code; the one that is n runs under H.
    let mut k = 0;
    while hot(&w).inputs < n {
        k += 1;
        let (s, v) =
            w.d.call(
                "POST",
                "/v1/identities",
                Some(&op),
                &[("client-key", &format!("person-{k}"))],
                Some(json!({ "kind": "person", "verifier": "0a" })),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "{v}");
        if hot(&w).inputs < n {
            assert_eq!(hot(&w).code, g.code, "before n the old code is in force");
            assert_eq!(w.core.with_port(|p| p.generation), Some(0));
        }
    }
    let h = hot(&w);
    assert_eq!((h.code, h.generation, h.upgrade), (h1, 1, None));
    assert_eq!(w.core.with_port(|p| p.switched.clone()), Some(vec![h1]));
    assert_eq!(w.core.with_port(|p| p.generation), Some(1));
}

/// `REQ-OP-10`, `INV-18` on the new routes: a retry of an upgrade is answered from its first
/// decision, not staged again. After the upgrade is in force its code names the generation the
/// writer is at, and its source may be gone; the retry still gets the original answer, and logs
/// nothing. With an explicit client key and with a derived one.
#[tokio::test(flavor = "multi_thread")]
async fn t037_04_a_retried_upgrade_is_answered_from_its_first_decision() {
    for key in [Some("upgrade-once"), None] {
        let w = world().await;
        let op = operator(&w);
        let h1 = w.code.version(1);
        let n = hot(&w).inputs + 3;
        let headers: Vec<(&str, &str)> = key.map(|k| ("client-key", k)).into_iter().collect();
        let (s, first) =
            w.d.call(
                "POST",
                "/v1/ops/upgrade",
                Some(&op),
                &headers,
                Some(upgrade_body(&h1, n)),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "{first}");
        // Up to and through `n`: the upgrade is in force.
        let mut k = 0;
        while hot(&w).inputs < n {
            k += 1;
            let (s, _) =
                w.d.call(
                    "POST",
                    "/v1/identities",
                    Some(&op),
                    &[("client-key", &format!("person-{k}"))],
                    Some(json!({ "kind": "person", "verifier": "0a" })),
                )
                .await;
            assert_eq!(s.as_u16(), 200);
        }
        assert_eq!(hot(&w).generation, 1);
        // The source forgets the version.
        w.code.forget(&h1);
        let before = logged(&w).await;
        let (s, again) =
            w.d.call(
                "POST",
                "/v1/ops/upgrade",
                Some(&op),
                &headers,
                Some(upgrade_body(&h1, n)),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "{key:?}: {again}");
        assert_eq!(
            again["operation_reference"], first["operation_reference"],
            "{key:?}: the same operation"
        );
        assert_eq!(logged(&w).await, before, "{key:?}: nothing was logged");
        // A changed request under the same key is refused as ever.
        if key.is_some() {
            let (s, v) =
                w.d.call(
                    "POST",
                    "/v1/ops/upgrade",
                    Some(&op),
                    &headers,
                    Some(upgrade_body(&h1, n + 1)),
                )
                .await;
            assert_eq!(s.as_u16(), 409, "{v}");
            assert!(condition(&v).contains("ERR-ClientKeyReused"), "{v}");
        }
    }
}

/// A writer fenced at `n`: the upgrade staged, scheduled from the next input, and the host's
/// switch taking no effect, so the old package refuses input `n` (the fence). Returns the world,
/// the code version, `n`, and a holder.
async fn fenced_at_n() -> (World, [u8; 32], u64, Party) {
    fence(world().await).await
}

/// [`fenced_at_n`] over a world the test has already used.
async fn fence(w: World) -> (World, [u8; 32], u64, Party) {
    let alice: Party = w.d.party("alice", 0).await;
    let op = operator(&w);
    let h1 = w.code.version(1);
    let n = hot(&w).inputs + 2;
    let (s, v) =
        w.d.call(
            "POST",
            "/v1/ops/upgrade",
            Some(&op),
            &[],
            Some(upgrade_body(&h1, n)),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(hot(&w).inputs + 1, n);
    // The host's switch takes no effect (a node that missed it): the package stays generation 0.
    w.core.with_port(|p| p.code_map.clear());
    (w, h1, n, alice)
}

/// Post an ordinary command that the fenced writer cannot decide; it is answered after the
/// decision wait, `ERR-DecisionUnavailable`, and stays `Received`.
async fn queue_ordinary(w: &World, key: &str) {
    let (s, _) = call(
        &w.d.router,
        "POST",
        "/v1/identities",
        Some(&operator(w)),
        &[("client-key", key)],
        Some(json!({ "kind": "person", "verifier": "0a" })),
    )
    .await;
    assert_eq!(s.as_u16(), 503);
}

/// The received-log row logged under a client key.
async fn row(w: &World, key: &str) -> ingress::store::Row {
    w.d.store
        .rows()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.new.client_key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no row under {key}"))
}

/// Review of #44, F1 and N5; amendment 0035; `AC-K7.12`, `REQ-RT-20`, end to end. The writer is
/// fenced at `n`: the forwarder retries the input that is refused, first, for ever. Three ordinary
/// inputs are `Received` ahead of the way out. A way out from a caller the core says may take it
/// overtakes them: it reaches the fenced writer first, **as input `n` itself**, is decided (not
/// refused as malformed: it is its input's first and only call through the real executor,
/// amendment 0035), cancels the upgrade, and the ordinary inputs then run, after it, under the old
/// code. A way out from a caller who may not is logged behind them, with no priority, and is
/// decided by the writer after them as a recorded refusal.
#[tokio::test(flavor = "multi_thread")]
async fn ac_k7_12_a_way_out_overtakes_the_queue_of_a_fenced_writer_end_to_end() {
    let (w, _h1, n, alice) = fenced_at_n().await;
    let g = hot(&w);
    assert_eq!(
        g.inputs + 1,
        n,
        "the next input is the one the fence refuses"
    );
    for k in 0..3 {
        queue_ordinary(&w, &format!("ordinary-{k}")).await;
    }
    for k in 0..3 {
        let r = row(&w, &format!("ordinary-{k}")).await;
        assert_ne!(
            r.status,
            Status::Decided,
            "ordinary-{k} is stuck behind the fence"
        );
    }
    // A caller who may not take the code in hand: logged behind them, with no priority.
    let (s, _) = call(
        &w.d.router,
        "POST",
        "/v1/ops/upgrade/cancel",
        Some(&alice.token),
        &[("client-key", "alice-cancel")],
        Some(json!({ "justification": "T-037-04: not mine to cancel" })),
    )
    .await;
    assert_eq!(s.as_u16(), 503, "it waits behind the fence");
    let a = row(&w, "alice-cancel").await;
    assert!(!a.new.overtakes, "a caller who may not gets no priority");
    assert_ne!(a.status, Status::Decided);
    // The operator's way out overtakes all four.
    let (s, v) = call(
        &w.d.router,
        "POST",
        "/v1/ops/upgrade/cancel",
        Some(&operator(&w)),
        &[("client-key", "operator-cancel")],
        Some(json!({ "justification": "T-037-04: the switch cannot be taken" })),
    )
    .await;
    assert_eq!(s.as_u16(), 200, "decided, not malformed: {v}");
    let way_out = row(&w, "operator-cancel").await;
    assert!(way_out.new.overtakes);
    assert_eq!(way_out.status, Status::Decided);
    // It took the fenced input's own number, ahead of every input logged before it.
    assert_eq!(way_out.core_input_seq, Some(n));
    // The first-call rule through the real executor (amendment 0035; review of #44, N5): the
    // input the Agent committed holds exactly the one call, so nothing the executor does for a
    // command runs a library call ahead of it.
    let entry = w
        .core
        .with_port(|p| p.entry(n))
        .and_then(Result::ok)
        .flatten()
        .expect("input n is in the Agent's ring");
    assert_eq!(
        entry.calls.iter().map(|c| c.method()).collect::<Vec<_>>(),
        ["cancel_upgrade"]
    );
    assert!(entry.deliveries.is_empty());
    // It took effect: the upgrade is cancelled, the code in force is the old one.
    let h = hot(&w);
    assert_eq!((h.code, h.generation, h.upgrade), (g.code, 0, None));
    // The ordinary inputs, logged before it, run after it; so does the refusal.
    until("the queue to drain", || {
        w.core
            .with_port(|p| p.hot())
            .is_some_and(|h| h.ok().flatten().is_some_and(|h| h.inputs >= n + 4))
    })
    .await;
    for k in 0..3 {
        let r = row(&w, &format!("ordinary-{k}")).await;
        assert_eq!(r.status, Status::Decided, "ordinary-{k}");
        assert!(
            r.core_input_seq > Some(n),
            "ordinary-{k} ran after the way out"
        );
    }
    let a = row(&w, "alice-cancel").await;
    assert_eq!(a.status, Status::Decided);
    assert!(a.core_input_seq > Some(n));
    let (s, v) = call(
        &w.d.router,
        "POST",
        "/v1/ops/upgrade/cancel",
        Some(&alice.token),
        &[("client-key", "alice-cancel")],
        Some(json!({ "justification": "T-037-04: not mine to cancel" })),
    )
    .await;
    assert_eq!(s.as_u16(), 403, "recorded as a refusal: {v}");
    assert!(condition(&v).contains("ERR-NotAuthorised"));
}

/// **B1 of the review of #47; `T-037-18`; amendment 0036; `AC-K7.21`.** The same fence as above,
/// but the first feed after it is answered ambiguous (the core ran it and refused it as fenced,
/// then the link failed): the head row is *carried* with its feed sequence. `F-038` §3 resends
/// such a row first, with that sequence. The core now answers the fence's refusal `NotRun`, so the
/// resend gives the carried sequence back (and raises the epoch, because the row was in flight),
/// the operator's way out, which overtakes in the store, takes the first sequence returned, and
/// the writer is unfenced. The row it overtook is then decided once, after it.
#[tokio::test(flavor = "multi_thread")]
async fn ac_k7_12_a_way_out_overtakes_a_row_carried_from_an_ambiguous_send() {
    let (w, _h1, n, _alice) = fenced_at_n().await;
    assert_eq!(hot(&w).inputs + 1, n);
    w.ambiguous_next.store(true, Ordering::SeqCst);
    queue_ordinary(&w, "carried").await;
    let carried = row(&w, "carried").await;
    assert!(
        carried.feed_seq.is_some(),
        "it was sent, and carries its sequence"
    );
    let (s, v) = call(
        &w.d.router,
        "POST",
        "/v1/ops/upgrade/cancel",
        Some(&operator(&w)),
        &[("client-key", "operator-cancel")],
        Some(json!({ "justification": "T-037-04: the switch cannot be taken" })),
    )
    .await;
    assert_eq!(
        s.as_u16(),
        200,
        "the way out reaches the fenced writer: {v}"
    );
    assert_eq!(hot(&w).upgrade, None);
    // It took the fenced input's own number, ahead of the row it overtook.
    let way_out = row(&w, "operator-cancel").await;
    assert_eq!(way_out.status, Status::Decided);
    assert_eq!(way_out.core_input_seq, Some(n));
    // The row it overtook was given back and is decided once, after it, under the old code.
    until("the carried row to be decided", || {
        w.core
            .with_port(|p| p.hot())
            .is_some_and(|h| h.ok().flatten().is_some_and(|h| h.inputs > n))
    })
    .await;
    let carried = row(&w, "carried").await;
    assert_eq!(carried.status, Status::Decided);
    assert_eq!(carried.core_input_seq, Some(n + 1));
    assert_eq!(hot(&w).inputs, n + 1, "each was decided once, none twice");
}

/// An input the fenced writer cannot decide, and the core can: a tick under key `k`.
fn tick(k: u8) -> CoreInput {
    CoreInput {
        key: InputKey([k; 16]),
        body: InputBody::Tick {
            now: T0 + u64::from(k),
        },
    }
}

/// `T-037-18`, amendment 0036, N1 to N4; the plan criterion kept out of `SPEC.md` (ruling w8): through the real Agent, the fence's refusal
/// is `NotRun` for the input it refused and every input after it in the call, whether the call
/// holds one input or several, and an input the core decided is answered from the record of
/// replies, never `NotRun`, though the writer is fenced. Nothing is committed by a refusal: the
/// writer's input number does not move, and the same inputs run once the way out is taken.
#[tokio::test(flavor = "multi_thread")]
async fn t037_18_a_fenced_refusal_is_not_run_for_the_tail_and_a_decided_input_is_answered() {
    let w = world().await;
    // Decided before the fence: its reply is in the record.
    let decided = tick(1);
    let reply = w
        .core
        .apply(&decided)
        .await
        .expect("decided before the fence");
    let (w, _h1, n, _alice) = fence(w).await;
    let inputs_before = hot(&w).inputs;
    assert_eq!(inputs_before + 1, n);
    // One call: the decided input first (answered, though fenced), then three the fence refuses.
    let batch = [decided.clone(), tick(2), tick(3), tick(4)];
    let answers = w.core.apply_batch(&batch).await;
    assert_eq!(
        answers[0],
        Ok(reply.clone()),
        "a decided input is answered from the record"
    );
    for (k, a) in answers.iter().enumerate().skip(1) {
        assert_eq!(*a, Err(CoreError::NotRun), "input {k} of the call");
    }
    // One input alone: the same.
    assert_eq!(w.core.apply(&tick(5)).await, Err(CoreError::NotRun));
    assert_eq!(w.core.apply(&decided).await, Ok(reply));
    // Nothing was committed, and the number was not taken.
    assert_eq!(hot(&w).inputs, inputs_before);
    // The way out unfences the writer, and the same inputs then run once, in order.
    let (s, v) = call(
        &w.d.router,
        "POST",
        "/v1/ops/upgrade/cancel",
        Some(&operator(&w)),
        &[("client-key", "operator-cancel")],
        Some(json!({ "justification": "T-037-18: the switch cannot be taken" })),
    )
    .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let answers = w.core.apply_batch(&[tick(2), tick(3), tick(4)]).await;
    let numbers: Vec<u64> = answers
        .iter()
        .map(|a| a.as_ref().unwrap().input_seq)
        .collect();
    assert_eq!(
        numbers.windows(2).map(|w| w[1] - w[0]).collect::<Vec<_>>(),
        [1, 1]
    );
}
