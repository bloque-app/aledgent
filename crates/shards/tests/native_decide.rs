// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Native decide, VOS verify (`F-034`, feature `native-decide`): each shard's host decides and
//! commits its rounds natively and a worker replays them through the shard Agent afterwards.
//! The Agents here run through [`NativePort`] (the guest's code over rows in memory), behind a
//! gate that holds their replays back and, in one test, a fault in their rows.
//!
//! - The book a native cluster reaches equals one book fed what it accepted, the trial balance
//!   holds, credits apply exactly once, and every committed round is confirmed (`REQ-RT-3`,
//!   `INV-5`, `INV-18`, `NFR-9`).
//! - Nothing but the caller's answer leaves a shard before its round is confirmed: no credit,
//!   no trim, no audit record, so no anchor (`INV-19`, `REQ-RT-11`).
//! - A native divergence halts the shard, raises an alert, leaves the other shards running, and
//!   is recovered from the decision log with the reversals listed; a divergence the Agent's side
//!   caused keeps the shard halted.
#![cfg(feature = "native-decide")]

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use core_binding::envelope::BlobKey;
use shards::agent::{Answer, Query};
use shards::decide::State;
use shards::host::{Cluster, HostError, Mode, NativePort, Port, accounts, open_all};
use shards::{Account, Command, Faults, Order, Outcome, Refusal, Relay, ShardId};

const USD: u32 = 840;
const ROOT: [u8; 32] = [0x44; 32];
const DRIVER: [u8; 32] = [0xD1; 32];

/// A gate the test opens and closes: a closed gate holds every invocation of the ports behind it.
#[derive(Default)]
struct Gate {
    closed: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn set(&self, closed: bool) {
        *self.closed.lock().expect("the gate") = closed;
        self.cv.notify_all();
    }
    fn pass(&self) {
        let mut closed = self.closed.lock().expect("the gate");
        while *closed {
            closed = self.cv.wait(closed).expect("the gate");
        }
    }
}

/// A shard Agent's port behind a gate, with an optional fault in its rows before its `n`-th
/// invocation (the Agent's side of a divergence).
struct Gated {
    inner: NativePort,
    gate: Arc<Gate>,
    invocations: u64,
    corrupt_at: Option<u64>,
}

impl Port for Gated {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
        self.gate.pass();
        self.invocations += 1;
        if self.corrupt_at == Some(self.invocations) {
            // The feed version this shard has applied, in its rows only: every later reply's
            // stream heads differ from the native shard's.
            self.inner
                .rows_mut()
                .0
                .insert(b"f".to_vec(), 9_999u64.to_be_bytes().to_vec());
        }
        self.inner.invoke(message, blobs)
    }
    fn read(&mut self, query: Vec<u8>) -> Result<Vec<u8>, String> {
        self.inner.read(query)
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.inner.blob_key(bytes)
    }
}

fn port(gate: &Arc<Gate>) -> Gated {
    Gated {
        inner: NativePort::new(DRIVER),
        gate: Arc::clone(gate),
        invocations: 0,
        corrupt_at: None,
    }
}

fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0x34;
    o
}

/// `n` shards, each behind its own gate, every account opened and funded through the Agents
/// (verified), then native decide on with `batch` rounds a replay.
fn funded(n: u16, accts: &[Account], funds: u64, batch: usize) -> (Cluster<Gated>, Vec<Arc<Gate>>) {
    let gates: Vec<Arc<Gate>> = (0..n).map(|_| Arc::new(Gate::default())).collect();
    let open = Arc::new(Gate::default());
    let ports = gates.iter().map(port).collect();
    let mut c = Cluster::found(ports, port(&open), ROOT, [7; 32]).expect("founded");
    let all: Vec<(Account, u32)> = accts.iter().map(|a| (*a, USD)).collect();
    open_all(&mut c, &all).expect("opened");
    for (i, a) in accts.iter().enumerate() {
        c.operator(Command::Issue {
            op: op(1 << 40 | i as u64),
            account: *a,
            asset: USD,
            amount: funds,
        });
    }
    c.drain().expect("funded");
    c.enable_native(batch).expect("native decide");
    (c, gates)
}

fn balance(c: &Cluster<Gated>, a: &Account) -> i128 {
    let s = c.directory().shard_of(a);
    c.shards()[usize::from(s.0)]
        .replica()
        .balance(a, USD)
        .expect("open in its shard")
}

fn in_shard(c: &Cluster<Gated>, accts: &[Account], s: u16) -> Vec<Account> {
    accts
        .iter()
        .copied()
        .filter(|a| c.directory().shard_of(a) == ShardId(s))
        .collect()
}

/// One book, fed the transfers the shards accepted in the order they decided them.
fn one_book(accts: &[Account], funds: u64, accepted: &[Command]) -> BTreeMap<Account, i128> {
    let mut one = shards::book(1);
    let setup: Vec<Command> = accts
        .iter()
        .map(|a| Command::Open {
            account: *a,
            asset: USD,
        })
        .collect();
    shards::run(&mut one, &mut Relay::default(), &setup, 64);
    shards::feed(
        &mut one,
        &accts.iter().map(|a| (*a, USD)).collect::<Vec<_>>(),
    );
    let issues: Vec<Command> = accts
        .iter()
        .enumerate()
        .map(|(i, a)| Command::Issue {
            op: op(1 << 41 | i as u64),
            account: *a,
            asset: USD,
            amount: funds,
        })
        .collect();
    shards::run(&mut one, &mut Relay::default(), &issues, 64);
    let r = shards::run(&mut one, &mut Relay::default(), accepted, 1);
    assert!(r.outcomes.iter().all(|o| *o == Outcome::Final));
    accts
        .iter()
        .map(|a| (*a, one[0].balance(a, USD).expect("open")))
        .collect()
}

/// Every shard's Agent holds the root its native shard holds.
fn agents_agree(c: &mut Cluster<Gated>) {
    for s in 0..c.directory().shards() {
        let want = c.shards()[usize::from(s)].replica().root();
        match c.shard_mut(ShardId(s)).read(&Query::Hot).expect("a read") {
            Answer::Hot(Some(h)) => assert_eq!(h.last_root, Some(want), "shard {s}"),
            other => panic!("{other:?}"),
        }
    }
}

fn step_until(c: &mut Cluster<Gated>, done: impl Fn(&Cluster<Gated>) -> bool) {
    let t = Instant::now();
    while !done(c) {
        c.step().expect("a step");
        assert!(t.elapsed() < Duration::from_secs(60), "never reached");
    }
}

#[test]
fn f034_native_decide_reaches_one_book_and_every_round_is_confirmed() {
    for (faults, batch, wait) in [
        (Faults::default(), 1, 0),
        (
            Faults {
                duplicate: true,
                reorder: true,
                drop_head: 1,
                lose_acks: true,
            },
            4,
            0,
        ),
        (Faults::default(), 4, 20),
    ] {
        let accts = accounts(24);
        let (mut c, _gates) = funded(3, &accts, 1_000, batch);
        c.faults = faults;
        c.batch = 8;
        c.round_wait = Duration::from_millis(wait);
        let mut orders = BTreeMap::new();
        for k in 0..300u64 {
            let (t, o) = c
                .transfer(
                    None,
                    accts[(k * 7 % 24) as usize],
                    accts[(k * 11 % 24) as usize],
                    USD,
                    1 + k % 90,
                )
                .expect("routed");
            orders.insert(t, o);
            if k % 5 == 4 {
                c.step().expect("a step");
            }
        }
        c.drain().expect("drained");
        c.settle().expect("settled");
        assert_eq!(c.unconfirmed(), 0);
        assert!(c.metrics.alerts.is_empty(), "{:?}", c.metrics.alerts);
        assert_eq!(
            c.metrics.replay_lag_us.len() as u64,
            c.metrics.confirmed_rounds
        );
        assert!(c.shards().iter().all(|s| s.mode() == Mode::Native));
        assert!(c.trial_balance(USD).holds());
        assert!(c.verify_exactly_once().expect("read").is_empty());
        agents_agree(&mut c);
        let accepted: Vec<Command> = c
            .decided
            .iter()
            .filter(|t| orders.contains_key(t))
            .filter(|t| c.outcome(**t).is_some_and(|o| o.accepted()))
            .map(|t| orders[t].command)
            .collect();
        let want = one_book(&accts, 1_000, &accepted);
        for a in &accts {
            assert_eq!(balance(&c, a), want[a], "zero divergence");
        }
        // Every record reached the assembler: the anchor's cut is the newest round of each shard.
        let (_, cut) = c.anchor();
        for (s, h) in cut.shards.iter().enumerate() {
            assert_eq!(h.seq, c.shards()[s].replica().state().audit_seq());
        }
    }
}

#[test]
fn f034_nothing_but_the_answer_leaves_a_shard_before_its_round_is_confirmed() {
    let accts = accounts(24);
    let (mut c, gates) = funded(2, &accts, 1_000, 8);
    let (from, to) = (in_shard(&c, &accts, 0), in_shard(&c, &accts, 1));
    let before_payee = balance(&c, &to[0]);
    let confirmed = c.shards()[0].replica().state().audit_seq();
    gates[0].set(true);
    let mut tickets = Vec::new();
    for _ in 0..3 {
        let (t, _) = c.transfer(None, from[0], to[0], USD, 10).expect("routed");
        tickets.push(t);
        c.step().expect("a step");
    }
    // Answered: final for the payer, at its native commit.
    for t in &tickets {
        assert!(matches!(c.outcome(*t), Some(Outcome::Debited { .. })));
    }
    assert!(c.unconfirmed() >= 3);
    for _ in 0..3 {
        c.step().expect("a step");
    }
    // Not credited, and not in any cut: the round is not confirmed.
    assert_eq!(balance(&c, &to[0]), before_payee);
    assert_eq!(c.in_flight(), 0, "no credit has left shard 0");
    let (_, cut) = c.anchor();
    assert!(
        cut.shards[0].seq <= confirmed,
        "INV-19: no anchor past the replay"
    );
    gates[0].set(false);
    c.drain().expect("drained");
    assert_eq!(balance(&c, &to[0]), before_payee + 30);
    assert!(c.trial_balance(USD).holds());
    assert!(c.metrics.replay_lag().p99_us > 0);
}

#[test]
fn f034_a_native_divergence_halts_the_shard_and_is_recovered_with_its_reversals() {
    let accts = accounts(24);
    let (mut c, gates) = funded(2, &accts, 1_000, 4);
    let (zero, one) = (in_shard(&c, &accts, 0), in_shard(&c, &accts, 1));
    let (a, b, payee) = (zero[0], zero[1], one[0]);
    let payee_before = balance(&c, &payee);
    gates[0].set(true);
    // Round k of shard 0: its native shard also issues a million to `a` (a fault the Agent never
    // sees), after the round's own transfer.
    let k = c.shards()[0].replica().state().audit_seq() + 1;
    c.shard_mut(ShardId(0)).inject = Some((
        k,
        vec![Order::from(Command::Issue {
            op: op(0xBAD),
            account: a,
            asset: USD,
            amount: 1_000_000,
        })],
    ));
    let (small, _) = c.transfer(None, b, a, USD, 1).expect("routed");
    c.step().expect("round k");
    // Round k + 1: `a` pays out what only the faulty native book holds. Acknowledged.
    let (big, _) = c.transfer(None, a, payee, USD, 500_000).expect("routed");
    c.step().expect("round k + 1");
    assert_eq!(c.outcome(small), Some(Outcome::Final));
    assert!(matches!(c.outcome(big), Some(Outcome::Debited { .. })));
    // The replay runs: the Agent finds the divergence at round k and fences itself.
    gates[0].set(false);
    step_until(&mut c, |c| c.halted(ShardId(0)).is_some());
    let alert = c.halted(ShardId(0)).expect("halted").clone();
    assert_eq!(alert.fence.expect("a divergence").round, k);
    assert_eq!(c.metrics.alerts.len(), 1);
    match c.shard_mut(ShardId(0)).read(&Query::Fence).expect("a read") {
        Answer::Fence(Some(f)) => assert_eq!(f.round, k),
        other => panic!("the Agent is not fenced: {other:?}"),
    }
    // Shard 0 decides nothing more; shard 1 goes on.
    let (held, _) = c.transfer(None, b, a, USD, 2).expect("routed");
    let (other, _) = c.transfer(None, one[1], one[2], USD, 3).expect("routed");
    c.drain().expect("drained what is not halted");
    assert_eq!(c.outcome(held), None, "the halted shard decided nothing");
    assert_eq!(c.outcome(other), Some(Outcome::Final));
    // The overdraft's credit never left shard 0, and no cut passes the confirmed rounds.
    assert_eq!(balance(&c, &payee), payee_before);
    let (_, cut) = c.anchor();
    assert!(cut.shards[0].seq < k);

    // Recovery.
    let r = c.recover_shard(ShardId(0)).expect("recovered");
    assert_eq!(r.fence.round, k);
    assert_eq!(r.rebuilt, k - 1);
    assert_eq!(r.redecided, 1);
    assert_eq!(r.reversals.len(), 1, "{:?}", r.reversals);
    assert_eq!(r.reversals[0].ticket, big);
    assert!(matches!(
        r.reversals[0].acknowledged,
        Outcome::Debited { .. }
    ));
    assert_eq!(
        r.reversals[0].authoritative,
        Outcome::Refused(Refusal::Insufficient)
    );
    assert_eq!(
        c.outcome(big),
        Some(Outcome::Refused(Refusal::Insufficient))
    );
    assert_eq!(c.shards()[0].mode(), Mode::Verified);
    c.drain().expect("drained");
    assert_eq!(c.outcome(held), Some(Outcome::Final));
    assert_eq!(balance(&c, &payee), payee_before);
    assert_eq!(balance(&c, &a), 1_000 + 1 + 2);
    assert!(c.trial_balance(USD).holds());
    assert!(c.verify_exactly_once().expect("read").is_empty());
    agents_agree(&mut c);

    // The operator lifts the fence: native decide again.
    c.resume_native(ShardId(0)).expect("resumed");
    assert!(matches!(
        c.shard_mut(ShardId(0)).read(&Query::Fence).expect("a read"),
        Answer::Fence(None)
    ));
    let (again, _) = c.transfer(None, a, b, USD, 1).expect("routed");
    c.drain().expect("drained");
    c.settle().expect("settled");
    assert_eq!(c.outcome(again), Some(Outcome::Final));
    assert_eq!(c.shards()[0].mode(), Mode::Native);
    assert_eq!(c.metrics.alerts.len(), 1);
    agents_agree(&mut c);
}

#[test]
fn f034_a_divergence_on_the_agents_side_keeps_the_shard_halted() {
    let accts = accounts(24);
    let gates: Vec<Arc<Gate>> = (0..2).map(|_| Arc::new(Gate::default())).collect();
    let open = Arc::new(Gate::default());
    let mut ports: Vec<Gated> = gates.iter().map(port).collect();
    // Founding, 24 openings and 24 issues take a few invocations; then a fault in the rows.
    ports[0].corrupt_at = Some(40);
    let mut c = Cluster::found(ports, port(&open), ROOT, [7; 32]).expect("founded");
    c.batch = 4;
    let all: Vec<(Account, u32)> = accts.iter().map(|a| (*a, USD)).collect();
    open_all(&mut c, &all).expect("opened");
    for (i, a) in accts.iter().enumerate() {
        c.operator(Command::Issue {
            op: op(1 << 40 | i as u64),
            account: *a,
            asset: USD,
            amount: 1_000,
        });
    }
    c.drain().expect("funded");
    c.enable_native(1).expect("native decide");
    let zero = in_shard(&c, &accts, 0);
    for k in 0..400u64 {
        if c.halted(ShardId(0)).is_some() {
            break;
        }
        c.transfer(None, zero[0], zero[1 + (k as usize % 3)], USD, 1)
            .expect("routed");
        c.step().expect("a step");
    }
    step_until(&mut c, |c| c.halted(ShardId(0)).is_some());
    match c.recover_shard(ShardId(0)) {
        Err(HostError::Divergence(why)) => assert!(why.contains("stays halted"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(c.halted(ShardId(0)).is_some());
    assert!(c.resume_native(ShardId(0)).is_err());
}
