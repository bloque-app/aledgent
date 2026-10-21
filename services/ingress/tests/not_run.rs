// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The fence's definite "not run" and the feed sequences it gives back (`T-037-18`; amendment
//! 0036; `F-038` §3, §4; `REQ-RT-20`, `REQ-CS-7`, `INV-18`; `AC-K7.21`, `AC-K7.22`; the answer to a
//! decided or refused input is `T-037-18`'s own criterion, ruling w8).
//!
//! A real forwarder and a real [`Sequencer`] over a core whose fence is a switch: while it is on
//! the core runs only the inputs the test lets through (the way out, or a chosen prefix) and
//! answers every other `NotRun`, never one it decided. A feed whose answer is lost is reported
//! `Ambiguous`, as a link that fails after the core ran it would be.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ingress::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, InputBody, InputKey,
    OutboxPage, Query, QueryReply, Staged,
};
use ingress::feed::{Feed, FeedReply, Sequencer};
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::store::mem::MemStore;
use ingress::store::{InputKind, NewRow, Status, Store};
use ingress::testkit::{Deployment, T0, TestCore, fast_config, genesis, test_clerk};
use ledger_core::account::Time;
use ledger_core::lifecycle::keys::Surface;

/// The core under the sequencer: the in-process core behind a fence that is a switch.
#[derive(Clone)]
struct Gate {
    inner: TestCore,
    /// While set, only the inputs in `pass` run; every other input not yet decided is `NotRun`.
    fenced: Arc<AtomicBool>,
    /// What the fence lets through.
    pass: Arc<Mutex<HashSet<InputKey>>>,
    /// The inputs whose decision lifts the fence: the way out.
    lifts: Arc<Mutex<HashSet<InputKey>>>,
    /// Every input the core ran, in order, once each.
    ran: Arc<Mutex<Vec<InputKey>>>,
    /// The record of replies: a decided input is always answered from it.
    replies: Arc<Mutex<HashMap<InputKey, CoreReply>>>,
}

impl Gate {
    fn new(inner: TestCore) -> Gate {
        Gate {
            inner,
            // Off while the deployment is set up; each test turns it on.
            fenced: Arc::new(AtomicBool::new(false)),
            pass: Arc::default(),
            lifts: Arc::default(),
            ran: Arc::default(),
            replies: Arc::default(),
        }
    }

    fn ran(&self) -> Vec<InputKey> {
        self.ran.lock().unwrap().clone()
    }
}

impl Core for Gate {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        if let Some(r) = self.replies.lock().unwrap().get(&input.key) {
            return Ok(r.clone());
        }
        if self.fenced.load(Ordering::SeqCst) && !self.pass.lock().unwrap().contains(&input.key) {
            return Err(CoreError::NotRun);
        }
        let reply = self.inner.apply(input).await?;
        self.ran.lock().unwrap().push(input.key);
        self.replies
            .lock()
            .unwrap()
            .insert(input.key, reply.clone());
        if self.lifts.lock().unwrap().contains(&input.key) {
            self.fenced.store(false, Ordering::SeqCst);
        }
        Ok(reply)
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        let mut out = Vec::with_capacity(inputs.len());
        for input in inputs {
            let r = self.apply(input).await;
            let failed = r.clone().err();
            out.push(r);
            if let Some(e) = failed {
                out.resize(inputs.len(), Err(e));
                break;
            }
        }
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

/// One feed as the core saw it, and what the sequencer answered.
#[derive(Clone)]
struct Sent {
    feed: Feed,
    reply: FeedReply,
}

impl Sent {
    fn seqs(&self) -> Vec<u64> {
        self.feed.items.iter().map(|f| f.seq).collect()
    }

    fn keys(&self) -> Vec<InputKey> {
        self.feed.inputs().unwrap().iter().map(|i| i.key).collect()
    }
}

/// The host's core: the sequencer in front of the gate, with the link's failures scripted.
#[derive(Clone)]
struct Node {
    seq: Arc<Sequencer<Gate>>,
    gate: Gate,
    /// The next feed is answered ambiguous (the link failed after the core ran it).
    ambiguous_next: Arc<AtomicBool>,
    /// The feed after that one never answers: the feeder is killed meanwhile.
    stall_after_ambiguous: Arc<AtomicBool>,
    stalled: Arc<AtomicBool>,
    sent: Arc<Mutex<Vec<Sent>>>,
}

impl Core for Node {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        self.seq.apply(input).await
    }
    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        self.seq.apply_batch(inputs).await
    }
    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> Result<OutboxPage, CoreError> {
        self.seq.outbox(cursor, undelivered, now).await
    }
    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        self.seq.last_tick().await
    }
    async fn next_due(&self) -> Result<Option<Time>, CoreError> {
        self.seq.next_due().await
    }
    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        self.seq.query(caller, query, now).await
    }
    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        self.seq.credential(credential).await
    }
    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        self.seq.party_of(principal).await
    }
    async fn genesis_open(&self) -> Result<bool, CoreError> {
        self.seq.genesis_open().await
    }
    async fn stage_code(&self, code: &[u8; 32]) -> Result<Staged, CoreError> {
        self.seq.stage_code(code).await
    }
    async fn feed(&self, feed: &Feed) -> FeedReply {
        if self.stalled.load(Ordering::SeqCst) {
            // The feeder was killed with this feed in hand: it never answers.
            self.sent.lock().unwrap().push(Sent {
                feed: feed.clone(),
                reply: FeedReply::Ambiguous,
            });
            return std::future::pending().await;
        }
        let reply = self.seq.feed(feed).await;
        self.sent.lock().unwrap().push(Sent {
            feed: feed.clone(),
            reply: reply.clone(),
        });
        if self.ambiguous_next.swap(false, Ordering::SeqCst) {
            if self.stall_after_ambiguous.load(Ordering::SeqCst) {
                self.stalled.store(true, Ordering::SeqCst);
            }
            return FeedReply::Ambiguous;
        }
        reply
    }
}

impl Node {
    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
}

async fn world() -> (Deployment<MemStore, Node>, Node) {
    let journal = MemJournal::new();
    let inner = InProcessCore::in_memory(genesis(T0), journal.clone()).with_clerk(test_clerk());
    inner.start().unwrap();
    let founded = inner.founded().unwrap();
    let gate = Gate::new(inner);
    let node = Node {
        seq: Arc::new(Sequencer::new(gate.clone(), None)),
        gate,
        ambiguous_next: Arc::default(),
        stall_after_ambiguous: Arc::default(),
        stalled: Arc::default(),
        sent: Arc::default(),
    };
    let mut config = fast_config("ingress-0");
    // The rows are the test's own.
    config.tick_interval = Duration::from_secs(365 * 86_400);
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        config,
        node.clone(),
        founded,
        journal,
    )
    .await;
    // The setup's own rows are decided before the test starts, and its feeds forgotten.
    assert!(
        d.quiesce(Duration::from_secs(30)).await,
        "the setup settles"
    );
    node.sent.lock().unwrap().clear();
    node.gate.ran.lock().unwrap().clear();
    (d, node)
}

/// A tick row under key `n`: an input the core decides without a caller. `overtakes` makes it the
/// way out for the test: read before every row not yet fed.
fn tick_row(n: u8, overtakes: bool) -> NewRow {
    let body = serde_json::to_vec(&InputBody::Tick {
        now: T0 + u64::from(n),
    })
    .unwrap();
    NewRow {
        op_ref: key(n),
        kind: InputKind::Tick,
        caller: None,
        surface: Surface::Command,
        client_key: None,
        key_domain: None,
        key_derived: false,
        body_digest: [n; 32],
        body,
        pass_through: None,
        reserves: false,
        overtakes,
        synchronous: false,
        received_at: T0 + u64::from(n),
        arrived_at: None,
    }
}

fn key(n: u8) -> InputKey {
    InputKey([n; 16])
}

async fn until<F: FnMut() -> bool>(what: &str, mut f: F) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn decided(d: &Deployment<MemStore, Node>, n: u8) -> bool {
    d.store
        .get(key(n))
        .await
        .unwrap()
        .is_some_and(|r| r.status == Status::Decided)
}

async fn until_decided(d: &Deployment<MemStore, Node>, what: &str, ns: &[u8]) {
    let start = Instant::now();
    loop {
        let mut all = true;
        for n in ns {
            all &= decided(d, *n).await;
        }
        if all {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `AC-K7.21`, `AC-K7.22`, `T-037-18`: two rows are sent to a fenced writer and the answer is
/// lost, so they are carried with their sequences. The core's definite `NotRun` gives their
/// sequences back, in one step that raises the epoch; the way out, logged meanwhile, takes the
/// first sequence returned; once it lifts the fence the rows it overtook are decided once each,
/// in the order they were logged, none twice. A late copy of the first, ambiguous send is refused
/// as stale by the epoch, and runs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn ac_k7_21_a_not_run_gives_back_carried_sequences_and_the_way_out_takes_the_first() {
    let (d, node) = world().await;
    node.gate.fenced.store(true, Ordering::SeqCst);
    node.ambiguous_next.store(true, Ordering::SeqCst);
    for n in [1, 2] {
        d.store.append(tick_row(n, false)).await.unwrap();
    }
    d.replica.shared.wake.notify_one();
    // The ambiguous send and its resend, which the core also answered `NotRun`.
    until("the carried rows to be resent", || node.sent().len() >= 2).await;
    // The way out is logged behind them, and lifts the fence when it is decided.
    node.gate.pass.lock().unwrap().insert(key(9));
    node.gate.lifts.lock().unwrap().insert(key(9));
    d.store.append(tick_row(9, true)).await.unwrap();
    d.replica.shared.wake.notify_one();
    until_decided(&d, "the way out and the rows it overtook", &[9, 1, 2]).await;

    let sent = node.sent();
    let first = &sent[0];
    let base = first.seqs()[0];
    assert_eq!(first.keys()[0], key(1), "the first row was sent first");
    assert!(
        matches!(&first.reply, FeedReply::Answers { answers }
            if answers.iter().all(|a| *a == ingress::remote::BatchAnswer::NotRun)),
        "the fence's refusal is a definite not run: {:?}",
        first.reply
    );
    // Gapless: nothing was taken before the way out, so every feed starts at the same sequence
    // and is contiguous, however the rows were batched.
    for s in &sent {
        assert_eq!(
            s.seqs()[0],
            base,
            "{:?}",
            sent.iter().map(Sent::seqs).collect::<Vec<_>>()
        );
        assert!(s.seqs().windows(2).all(|w| w[1] == w[0] + 1));
    }
    // The way out took the first sequence returned, ahead of the rows it overtook.
    let with_way_out = sent
        .iter()
        .find(|s| s.keys().contains(&key(9)))
        .expect("the way out was sent");
    assert_eq!(with_way_out.keys()[0], key(9));
    assert_eq!(with_way_out.seqs()[0], base);
    // Decided once each, the way out first, then the rows in the order they were logged.
    assert_eq!(node.gate.ran(), vec![key(9), key(1), key(2)]);
    let seq_of = |n: u8| {
        let store = d.store.clone();
        async move {
            store
                .get(key(n))
                .await
                .unwrap()
                .unwrap()
                .core_input_seq
                .unwrap()
        }
    };
    let (w, a, b) = (seq_of(9).await, seq_of(1).await, seq_of(2).await);
    assert!(w < a && a < b, "{w} {a} {b}");
    assert!(
        !node.gate.fenced.load(Ordering::SeqCst),
        "the fence is gone"
    );
    // The epoch rose once, when in-flight rows were given back; giving back rows that had only
    // been answered `NotRun` (not in flight) does not raise it again.
    let epochs: Vec<u64> = sent.iter().map(|s| s.feed.epoch).collect();
    assert_eq!(epochs[0], epochs[1], "the resend keeps the epoch");
    let top = *epochs.iter().max().unwrap();
    assert_eq!(top, epochs[0] + 1, "{epochs:?}");
    // A late copy of the first send, still in the core's hands, is stale: refused, runs nothing.
    let ran = node.gate.ran();
    assert_eq!(
        node.seq.feed(&first.feed).await,
        FeedReply::Fenced { epoch: top }
    );
    assert_eq!(node.gate.ran(), ran);
}

/// `AC-K7.22`: the writer decided a prefix of the feed and the fence refused the rest. The prefix
/// is decided and its sequence is never given back; the rest, carried, is answered `NotRun` and
/// given back from its first sequence, with the epoch raised, and is decided once the fence is
/// gone.
#[tokio::test(flavor = "multi_thread")]
async fn ac_k7_22_only_the_refused_tail_is_given_back() {
    let (d, node) = world().await;
    node.gate.fenced.store(true, Ordering::SeqCst);
    node.gate.pass.lock().unwrap().insert(key(1));
    node.ambiguous_next.store(true, Ordering::SeqCst);
    for n in [1, 2, 3] {
        d.store.append(tick_row(n, false)).await.unwrap();
    }
    d.replica.shared.wake.notify_one();
    until_decided(&d, "the prefix", &[1]).await;
    until("the tail to be given back and sent again", || {
        let sent = node.sent();
        let base = sent[0].seqs()[0];
        sent.iter()
            .any(|s| s.keys() == vec![key(2), key(3)] && s.seqs() == vec![base + 1, base + 2])
    })
    .await;
    // The prefix kept its sequence; the tail was fed again from the first of its own.
    let base = node.sent()[0].seqs()[0];
    let first = d.store.get(key(1)).await.unwrap().unwrap();
    assert_eq!(first.feed_seq, Some(base));
    for s in node.sent() {
        if let Some(i) = s.keys().iter().position(|k| *k == key(1)) {
            assert_eq!(s.seqs()[i], base, "the decided prefix is never refed");
        }
    }
    node.gate.fenced.store(false, Ordering::SeqCst);
    until_decided(&d, "the tail", &[2, 3]).await;
    assert_eq!(node.gate.ran(), vec![key(1), key(2), key(3)], "each once");
    let epochs: Vec<u64> = node.sent().iter().map(|s| s.feed.epoch).collect();
    assert_eq!(epochs.iter().max(), Some(&(epochs[0] + 1)), "{epochs:?}");
}

/// `AC-K7.21`, the feeder that takes over: a feeder is killed with the rows carried (it sent them,
/// the answer was lost, and its resend never came back). The feeder that takes the lease reads
/// them carried, resends them at their sequences, is told `NotRun`, and gives them back; the way
/// out, logged meanwhile, takes the first sequence, and the rows are decided once, after it. The
/// epoch is strictly higher after every step that could leave a copy behind.
#[tokio::test(flavor = "multi_thread")]
async fn ac_k7_21_a_feeder_that_takes_over_gives_back_what_the_last_one_carried() {
    let (mut d, node) = world().await;
    node.gate.fenced.store(true, Ordering::SeqCst);
    node.ambiguous_next.store(true, Ordering::SeqCst);
    node.stall_after_ambiguous.store(true, Ordering::SeqCst);
    for n in [1, 2] {
        d.store.append(tick_row(n, false)).await.unwrap();
    }
    d.replica.shared.wake.notify_one();
    until("the resend to stall", || node.sent().len() >= 2).await;
    let before = d.store.get(key(1)).await.unwrap().unwrap();
    assert_eq!(
        before.feed_seq,
        Some(node.sent()[0].seqs()[0]),
        "carried with its sequence"
    );
    // The feeder dies; another takes the lease once it lapses and the core answers again.
    d.replica.kill();
    node.stalled.store(false, Ordering::SeqCst);
    node.gate.pass.lock().unwrap().insert(key(9));
    node.gate.lifts.lock().unwrap().insert(key(9));
    d.store.append(tick_row(9, true)).await.unwrap();
    let mut next = fast_config("ingress-1");
    next.tick_interval = Duration::from_secs(365 * 86_400);
    let (_next, _surface) = d.replica_on(d.store.clone(), next);
    until_decided(&d, "the way out and the rows it overtook", &[9, 1, 2]).await;
    assert_eq!(node.gate.ran(), vec![key(9), key(1), key(2)], "each once");
    let sent = node.sent();
    let top = sent.iter().map(|s| s.feed.epoch).max().unwrap();
    let carried_epoch = sent[0].feed.epoch;
    assert_eq!(
        top,
        carried_epoch + 2,
        "one for the change of holder, one for giving back rows that were in flight"
    );
    // A late copy of the carried send is stale.
    assert!(matches!(
        node.seq.feed(&sent[0].feed).await,
        FeedReply::Fenced { .. }
    ));
}

/// `T-037-18`: the store's release of rows in flight, in memory: a stale holder's is refused and
/// changes nothing; the current holder's gives the tail back, keeps the rows before it, lowers
/// the next sequence so the next mark takes the first returned, and raises the epoch by exactly
/// one in the same step, returning it; the old epoch can no longer mark.
#[tokio::test(flavor = "multi_thread")]
async fn t_037_18_a_release_of_rows_in_flight_raises_the_epoch_in_memory() {
    use ingress::feed::BOOK;
    let s = MemStore::new(Duration::from_millis(50));
    let long = Duration::from_secs(60);
    assert_eq!(s.try_feed(BOOK, "a", long).await.unwrap(), Some(1));
    let mut keys = Vec::new();
    for n in 1..=4u8 {
        s.append(tick_row(n, false)).await.unwrap();
        keys.push(key(n));
    }
    let fed = s.mark_fed(BOOK, "a", 1, &keys, 7).await.unwrap();
    assert_eq!(
        fed.iter().map(|r| r.feed_seq).collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3), Some(4)]
    );
    // Not the holder, or not at that epoch: refused, nothing changes.
    assert!(
        s.unfeed_in_flight(BOOK, "b", 1, 2)
            .await
            .unwrap_err()
            .is_fenced()
    );
    assert!(
        s.unfeed_in_flight(BOOK, "a", 2, 2)
            .await
            .unwrap_err()
            .is_fenced()
    );
    assert_eq!(s.get(keys[1]).await.unwrap().unwrap().feed_seq, Some(2));
    // The holder releases from 3: rows 1 and 2 keep their sequences, 3 and 4 are unfed, and the
    // epoch is 2.
    assert_eq!(s.unfeed_in_flight(BOOK, "a", 1, 3).await.unwrap(), 2);
    let seqs: Vec<Option<u64>> = {
        let mut v = Vec::new();
        for k in &keys {
            v.push(s.get(*k).await.unwrap().unwrap().feed_seq);
        }
        v
    };
    assert_eq!(seqs, vec![Some(1), Some(2), None, None]);
    // The old epoch can no longer mark; the new one marks from the sequence returned.
    assert!(
        s.mark_fed(BOOK, "a", 1, &keys[2..], 8)
            .await
            .unwrap_err()
            .is_fenced()
    );
    let fed = s.mark_fed(BOOK, "a", 2, &keys[2..], 8).await.unwrap();
    assert_eq!(
        fed.iter().map(|r| r.feed_seq).collect::<Vec<_>>(),
        vec![Some(3), Some(4)]
    );
    // A renewal keeps the epoch; the plain give back does not raise it.
    assert_eq!(s.try_feed(BOOK, "a", long).await.unwrap(), Some(2));
    s.unfeed(BOOK, "a", 2, 4).await.unwrap();
    assert_eq!(s.try_feed(BOOK, "a", long).await.unwrap(), Some(2));
}

/// A feed of the ticks `ks` at sequences from `first`, under `epoch`.
fn feed_of(epoch: u64, first: u64, ks: &[u8]) -> Feed {
    let inputs: Vec<CoreInput> = ks
        .iter()
        .map(|k| CoreInput {
            key: key(*k),
            body: InputBody::Tick {
                now: T0 + u64::from(*k),
            },
        })
        .collect();
    let items: Vec<(u64, &CoreInput)> = (first..).zip(inputs.iter()).collect();
    Feed::build("book", epoch, &items, None).unwrap()
}

/// `T-037-18`, amendment 0036, fact 2 (review of #51, C6): the case the epoch rise exists for. A
/// copy of a send is still in the core's hands when the fence lifts, before any feed under the
/// raised epoch has reached the core. It is under the epoch the sequencer still holds, so it runs
/// in full, in queue order; the feeder's next feed, under the raised epoch, rebases and finds the
/// same rows answered from the record. Each row runs once, in log order, and a second copy after
/// that feed is `Fenced`.
#[tokio::test(flavor = "multi_thread")]
async fn t_037_18_a_late_copy_after_the_fence_lifts_runs_in_log_order_and_nothing_twice() {
    let journal = MemJournal::new();
    let inner = InProcessCore::in_memory(genesis(T0), journal).with_clerk(test_clerk());
    inner.start().unwrap();
    let gate = Gate::new(inner);
    gate.fenced.store(true, Ordering::SeqCst);
    let seq = Sequencer::new(gate.clone(), None);
    let answers = |r: FeedReply| match r {
        FeedReply::Answers { answers } => answers,
        other => panic!("not answered: {other:?}"),
    };
    // The first send: refused by the fence, `NotRun` for both, answer lost to the feeder.
    let first = feed_of(1, 1, &[1, 2]);
    assert!(
        answers(seq.feed(&first).await)
            .iter()
            .all(|a| *a == ingress::remote::BatchAnswer::NotRun)
    );
    // The feeder gave the sequences back and raised its epoch; the fence lifts before its next
    // feed reaches the core, and the late copy of the first send arrives.
    gate.fenced.store(false, Ordering::SeqCst);
    let late = answers(seq.feed(&first).await);
    assert!(
        late.iter()
            .all(|a| matches!(a, ingress::remote::BatchAnswer::Applied { .. }))
    );
    assert_eq!(gate.ran(), vec![key(1), key(2)], "in log order");
    // The next feed, under the raised epoch, resends the same rows with a row logged behind
    // them: the first two come from the record, the third runs.
    let next = answers(seq.feed(&feed_of(2, 1, &[1, 2, 3])).await);
    assert!(
        next.iter()
            .all(|a| matches!(a, ingress::remote::BatchAnswer::Applied { .. }))
    );
    assert_eq!(
        gate.ran(),
        vec![key(1), key(2), key(3)],
        "each row ran once"
    );
    // Another copy of the first send is stale now.
    assert_eq!(seq.feed(&first).await, FeedReply::Fenced { epoch: 2 });
    assert_eq!(gate.ran().len(), 3);
}
