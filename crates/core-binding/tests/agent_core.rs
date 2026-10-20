// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! ingress over the core Agent (`T-005-08`): a real deployment — surface, replica, delivery —
//! whose `boundary::Core` is [`AgentCore`] over the native Agent. The same `AgentCore` over VOS
//! is what `aledgent-core-host serve` runs.

use std::sync::atomic::Ordering;
use std::time::Duration;

use core_binding::core::{AgentCore, FAILED_ATTEMPTS, Founding, HostLog as _, MemLog};
use core_binding::envelope::{BlobKey, Envelope, decode, encode};
use core_binding::host::{AgentPort, BatchLimits, NativePort, PortError};
use core_binding::hot::{Hot, Limits, MAX_BUDGET, RingEntry};
use ingress::boundary::{Core as _, CoreError, CoreInput, InputBody, InputKey};
use ingress::inprocess::MemJournal;
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, T0, USD, fast_config, genesis};
use serde_json::{Value, json};

type AgentDeployment = Deployment<MemStore, AgentCore<NativePort>>;

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

async fn deployment() -> (AgentDeployment, AgentCore<NativePort>, MemLog) {
    let log = MemLog::default();
    let core = AgentCore::open(NativePort::new(), founding(), log.clone())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        core.founded(),
        MemJournal::new(),
    )
    .await;
    (d, core, log)
}

/// How many reservations the Agent committed, from the host's archive.
fn reservations(log: &MemLog) -> usize {
    log.entries()
        .unwrap()
        .iter()
        .flat_map(|e| e.calls.iter())
        .filter(|c| c.method().starts_with("reserve"))
        .count()
}

fn reference(v: &Value) -> String {
    v["operation_reference"]
        .as_str()
        .unwrap_or_else(|| panic!("no operation reference in {v}"))
        .to_owned()
}

fn inputs(core: &AgentCore<NativePort>) -> u64 {
    core.with_port(|p| {
        use core_binding::host::AgentPort as _;
        p.hot()
    })
    .and_then(Result::ok)
    .flatten()
    .map(|h| h.inputs)
    .unwrap_or_else(|| panic!("the Agent is founded"))
}

/// `AC-M1.1`, `INV-18` through the Agent: parties open, a reservation is taken and captured, and
/// every notification is delivered; each committed input reached the Agent, and a retried
/// request is answered from the log without a second commit.
#[tokio::test(flavor = "multi_thread")]
async fn ac_m1_1_a_deployment_runs_over_the_agent_core() {
    let (d, core, log) = deployment().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let a = Some(alice.token.as_str());
    let reserve = json!({ "payer": alice.account, "asset": USD, "amount": "7" });
    // A reservation is acknowledged at `Received` (`REQ-CS-3`): ask again until it is decided.
    let (s, r) = d
        .call_decided(
            "POST",
            "/v1/reservations",
            a,
            &[("client-key", "r1")],
            Some(reserve.clone()),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "reserve: {r}");
    let after_reserve = inputs(&core);
    assert_eq!(reservations(&log), 1);
    let (s, again) = d
        .call(
            "POST",
            "/v1/reservations",
            a,
            &[("client-key", "r1")],
            Some(reserve),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "reserve again: {again}");
    assert_eq!(reference(&again), reference(&r), "the same operation");
    // Ticks and delivery records keep committing in the background: count reservations.
    assert_eq!(reservations(&log), 1, "a retry commits nothing");
    let (s, c) = d
        .call(
            "POST",
            &format!("/v1/reservations/{}/capture", reference(&r)),
            a,
            &[],
            Some(json!({ "destination": bob.account, "cleared": "7" })),
        )
        .await;
    assert!(
        s.as_u16() == 200 && c["state"] == "Settled",
        "capture: {s} {c}"
    );
    assert!(d.all_delivered(Duration::from_secs(20)).await, "delivered");
    assert!(
        inputs(&core) > after_reserve,
        "the capture reached the Agent"
    );
    let delivered = core
        .with_port(|p| {
            use core_binding::host::AgentPort as _;
            p.hot()
        })
        .and_then(Result::ok)
        .flatten()
        .map(|h| h.delivered)
        .unwrap_or_default();
    assert!(delivered > 0, "deliveries are recorded in the Agent");
    assert_eq!(core.stopped(), None);
}

/// `T-005-08`: the Agent fails an invocation before running it (unavailable) and after running
/// it (ambiguous). Neither stops the core: the first is refused and retried, the second is
/// recognised as committed when the replica is rebuilt from the Agent. Either way the request
/// succeeds once, and the replica and the Agent still agree (a second commit, or a lost one,
/// would diverge them and stop the core).
#[tokio::test(flavor = "multi_thread")]
async fn t_005_08_agent_failures_are_retried_not_doubled() {
    let (d, core, log) = deployment().await;
    let alice = d.party("alice", 1_000).await;
    let a = Some(alice.token.as_str());
    let reserve = |n: u32| json!({ "payer": alice.account, "asset": USD, "amount": n.to_string() });
    for (n, fail) in [(1, false), (2, true)] {
        let before = inputs(&core);
        core.with_port(|p| p.fail_next = Some(fail));
        let key = format!("r{n}");
        let (s, r) = d
            .call_decided(
                "POST",
                "/v1/reservations",
                a,
                &[("client-key", &key)],
                Some(reserve(n)),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "reserve after a {fail} failure: {r}");
        assert!(inputs(&core) > before, "committed");
        let (s, again) = d
            .call(
                "POST",
                "/v1/reservations",
                a,
                &[("client-key", &key)],
                Some(reserve(n)),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "retried: {again}");
        assert_eq!(reference(&again), reference(&r), "the same operation");
    }
    assert_eq!(reservations(&log), 2, "each reservation committed once");
    assert_eq!(core.stopped(), None);
}

// T-031-03 -------------------------------------------------------------------------------------

/// The native Agent, counting invocations, and refusing one input number's invocation: its
/// kernel witness is named but not given, so the Agent refuses it (`AgentRefusal::Malformed`).
/// In a batch only, or wherever the input travels.
struct Faulty {
    inner: NativePort,
    refuse: Option<u64>,
    /// An input number whose every invocation the runtime aborts, as VOS reports an actor that
    /// panicked (finding F18 of `F-032`).
    abort: Option<u64>,
    only_batched: bool,
    invocations: usize,
    /// How long each invocation takes, on top of the native run (an invocation in VOS).
    delay: Duration,
}

impl Faulty {
    fn new() -> Self {
        Faulty {
            inner: NativePort::new(),
            refuse: None,
            abort: None,
            only_batched: false,
            invocations: 0,
            delay: Duration::ZERO,
        }
    }
}

const NOWHERE: BlobKey = BlobKey {
    hash: [0xEE; 32],
    len: 1,
};

impl AgentPort for Faulty {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        self.invocations += 1;
        std::thread::sleep(self.delay);
        if let Some(t) = self.abort {
            let aborted = match decode::<Envelope>(&message) {
                Ok(Envelope::Input(i)) => i.seq == t,
                Ok(Envelope::Batch(b)) => b.inputs.iter().any(|i| i.seq == t),
                _ => false,
            };
            if aborted {
                return Err(PortError::Aborted("status: Panicked".into()));
            }
        }
        let message = match (self.refuse, decode::<Envelope>(&message)) {
            (Some(t), Ok(Envelope::Input(mut i))) if i.seq == t && !self.only_batched => {
                i.witness = NOWHERE;
                encode(&Envelope::Input(i))
            }
            (Some(t), Ok(Envelope::Batch(mut b))) if b.inputs.iter().any(|i| i.seq == t) => {
                for i in b.inputs.iter_mut().filter(|i| i.seq == t) {
                    i.witness = NOWHERE;
                }
                encode(&Envelope::Batch(b))
            }
            _ => message,
        };
        self.inner.invoke(message, blobs)
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.inner.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.inner.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.inner.entry(seq)
    }
}

/// Ticks: inputs every core takes, each a library call the Agent replays.
fn ticks(from: u8, n: u8) -> Vec<CoreInput> {
    (from..from + n)
        .map(|k| CoreInput {
            key: InputKey([k; 16]),
            body: InputBody::Tick {
                now: T0 + 1_000 * u64::from(k),
            },
        })
        .collect()
}

fn open<P: AgentPort + Send + 'static>(port: P) -> AgentCore<P> {
    AgentCore::open(port, founding(), MemLog::default())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"))
}

fn agent_inputs<P: AgentPort + Send + 'static>(core: &AgentCore<P>) -> u64 {
    core.with_port(|p| p.hot())
        .and_then(Result::ok)
        .flatten()
        .map(|h| h.inputs)
        .unwrap_or_else(|| panic!("the Agent is founded"))
}

/// `REQ-CS-7`, `INV-18`, `T-031-03`: a batch answers exactly as its inputs applied one by one —
/// the same outcomes under the same input numbers — in one invocation, and a batch applied again
/// is answered from the replies without a commit.
#[tokio::test(flavor = "multi_thread")]
async fn req_cs_7_inv_18_a_batch_answers_as_its_inputs_one_by_one() {
    let one = open(Faulty::new());
    let many = open(Faulty::new());
    let inputs = ticks(1, 12);
    let mut singles = Vec::new();
    for i in &inputs {
        singles.push(one.apply(i).await);
    }
    let before = many.with_port(|p| p.invocations).unwrap();
    let batched = many.apply_batch(&inputs).await;
    assert_eq!(batched, singles);
    assert!(batched.iter().all(Result::is_ok));
    assert_eq!(
        many.with_port(|p| p.invocations).unwrap() - before,
        1,
        "one invocation"
    );
    assert_eq!(agent_inputs(&one), agent_inputs(&many));
    let roots = |c: &AgentCore<Faulty>| c.with_port(|p| p.hot().unwrap().unwrap().roots()).unwrap();
    assert_eq!(roots(&one), roots(&many));
    // Again: answered from the replies, nothing sent.
    let at = agent_inputs(&many);
    assert_eq!(many.apply_batch(&inputs).await, singles);
    assert_eq!(agent_inputs(&many), at);
    // With one input per invocation the answers are the same too. A call runs one invocation
    // and answers the rest `Unavailable` without running them (`T-031-18`): sent again, as the
    // forwarder sends them, they commit in order.
    let single = open(Faulty::new());
    single.set_batch_limits(BatchLimits::SINGLE);
    let first = single.apply_batch(&inputs).await;
    assert_eq!(first[0], singles[0]);
    assert!(first[1..].iter().all(|a| *a == Err(CoreError::Unavailable)));
    let mut answers = vec![first[0].clone()];
    while answers.len() < inputs.len() {
        let a = single.apply_batch(&inputs[answers.len()..]).await;
        assert!(a[0].is_ok(), "{a:?}");
        answers.push(a[0].clone());
    }
    assert_eq!(answers, singles);
    assert_eq!(single.with_port(|p| p.invocations).unwrap() - before, 12);
}

/// `T-031-03`: the Agent refuses one input of a batch. The inputs before it commit in the batch;
/// it and the rest are re-driven one at a time, and commit: every answer is the one-by-one
/// answer, and the core does not stop.
#[tokio::test(flavor = "multi_thread")]
async fn req_cs_7_a_failed_batch_commits_its_prefix_then_the_rest_one_at_a_time() {
    let reference = open(Faulty::new());
    let core = open(Faulty::new());
    let inputs = ticks(1, 10);
    let mut singles = Vec::new();
    for i in &inputs {
        singles.push(reference.apply(i).await);
    }
    let s = agent_inputs(&core);
    core.with_port(|p| {
        p.refuse = Some(s + 4);
        p.only_batched = true;
    });
    let before = core.with_port(|p| p.invocations).unwrap();
    assert_eq!(core.apply_batch(&inputs).await, singles);
    assert_eq!(agent_inputs(&core), s + 10);
    assert_eq!(
        core.with_port(|p| p.invocations).unwrap() - before,
        1 + 7,
        "the batch, then inputs 4 to 10 alone"
    );
    assert_eq!(core.stopped(), None);
}

/// `T-031-03`, `REQ-CS-7`: an input the Agent refuses wherever it travels fails alone: the inputs
/// before it commit, it and every input after it answer `Unavailable` and run nowhere, and once
/// the fault clears the rest commit in order, under the numbers one-by-one application gives.
#[tokio::test(flavor = "multi_thread")]
async fn req_cs_7_a_faulty_input_fails_alone_and_nothing_after_it_runs() {
    let reference = open(Faulty::new());
    let log = MemLog::default();
    let core = AgentCore::open(Faulty::new(), founding(), log.clone()).unwrap();
    let inputs = ticks(1, 10);
    let mut singles = Vec::new();
    for i in &inputs {
        singles.push(reference.apply(i).await);
    }
    let s = agent_inputs(&core);
    core.with_port(|p| p.refuse = Some(s + 4));
    let r = core.apply_batch(&inputs).await;
    assert_eq!(r[..3], singles[..3]);
    assert!(r[3..].iter().all(|r| *r == Err(CoreError::Unavailable)));
    assert_eq!(agent_inputs(&core), s + 3);
    assert_eq!(core.stopped(), None);
    core.with_port(|p| p.refuse = None);
    assert_eq!(core.apply_batch(&inputs[3..]).await, singles[3..]);
    assert_eq!(agent_inputs(&core), s + 10);
    // The host's archive is the Agent's chain, each input once: a restart rebuilds from it and
    // answers every input from its replies.
    let port = core
        .with_port(|p| std::mem::replace(p, Faulty::new()))
        .unwrap();
    drop(core);
    assert_eq!(log.entries().unwrap().len() as u64, s + 10);
    let again = AgentCore::open(port, founding(), log).unwrap();
    assert_eq!(again.apply_batch(&inputs).await, singles);
    assert_eq!(agent_inputs(&again), s + 10);
}

/// Finding F18 of `F-032`, `REQ-CS-7`, `REQ-AT-3`, `REQ-RT-13`: an input the Agent aborts wherever
/// it travels (the runtime reports a panic) is tried alone a bounded number of times
/// (`FAILED_ATTEMPTS`), then the core halts on it and says why. Halted, it answers every input
/// `Unavailable` without reaching the Agent or rebuilding its replica, reads go on at what the
/// Agent committed, and nothing skips the input. Restarted with an Agent that applies it, the core
/// resumes in order. Before, every retry rebuilt the replica from the whole archive, for ever.
#[tokio::test(flavor = "multi_thread")]
async fn req_cs_7_an_input_the_agent_aborts_halts_the_core_after_bounded_retries() {
    let reference = open(Faulty::new());
    let log = MemLog::default();
    let core = AgentCore::open(Faulty::new(), founding(), log.clone()).unwrap();
    let inputs = ticks(1, 10);
    let mut singles = Vec::new();
    for i in &inputs {
        singles.push(reference.apply(i).await);
    }
    let s = agent_inputs(&core);
    core.with_port(|p| p.abort = Some(s + 4));
    // The batch aborts; its inputs go again one at a time: three commit, the fourth fails alone.
    let r = core.apply_batch(&inputs).await;
    assert_eq!(r[..3], singles[..3]);
    assert!(r[3..].iter().all(|r| *r == Err(CoreError::Unavailable)));
    assert_eq!(core.stopped(), None, "one failure alone is retried");
    // The forwarder's retry: the failed input goes alone at once, fails again, and the core halts.
    let invocations = |c: &AgentCore<Faulty>| c.with_port(|p| p.invocations).unwrap();
    let before = invocations(&core);
    let r = core.apply_batch(&inputs[3..]).await;
    assert!(r.iter().all(|r| *r == Err(CoreError::Unavailable)));
    assert_eq!(
        invocations(&core) - before,
        1,
        "alone, without a batch first"
    );
    assert_eq!(FAILED_ATTEMPTS, 2);
    let why = core.stopped().expect("halted");
    assert!(
        why.starts_with("halted: ") && why.contains("tick at"),
        "{why}"
    );
    assert_eq!(core.stats().halted.load(Ordering::Relaxed), 1);
    // Halted: nothing reaches the Agent, nothing is rebuilt, nothing is skipped.
    let before = invocations(&core);
    for _ in 0..5 {
        let r = core.apply_batch(&inputs[3..]).await;
        assert!(r.iter().all(|r| *r == Err(CoreError::Unavailable)));
    }
    assert_eq!(invocations(&core), before);
    assert_eq!(agent_inputs(&core), s + 3);
    // Reads answer at what the Agent committed: the third tick.
    assert_eq!(core.last_tick().await, Ok(Some(T0 + 3_000)));
    // Restarted with an Agent that applies it: every input, in order, as one by one.
    let mut port = core
        .with_port(|p| std::mem::replace(p, Faulty::new()))
        .unwrap();
    port.abort = None;
    drop(core);
    let again = AgentCore::open(port, founding(), log).unwrap();
    assert_eq!(again.apply_batch(&inputs).await, singles);
    assert_eq!(agent_inputs(&again), s + 10);
    assert_eq!(again.stopped(), None);
}

/// `T-031-03`, `INV-18`: the Agent commits a batch and the answer is lost. The driver re-sends it,
/// the Agent answers every input as a duplicate, and each commits once.
#[tokio::test(flavor = "multi_thread")]
async fn inv_18_a_batch_whose_answer_is_lost_commits_once() {
    let reference = open(Faulty::new());
    let core = open(Faulty::new());
    let inputs = ticks(1, 6);
    let mut singles = Vec::new();
    for i in &inputs {
        singles.push(reference.apply(i).await);
    }
    let s = agent_inputs(&core);
    core.with_port(|p| p.inner.fail_next = Some(true));
    assert_eq!(core.apply_batch(&inputs).await, singles);
    assert_eq!(agent_inputs(&core), s + 6);
}

// T-031-12 -------------------------------------------------------------------------------------

/// `REQ-QR-2`, `DEC-4`, `T-031-12`: reads are answered at the state the Agent committed, never at
/// the replica's state while an input is in flight. An input the Agent refuses runs on the
/// replica first; a read made while it is in flight waits, and answers at the state before it,
/// which is what the Agent holds. An input the Agent commits is seen once it has committed.
#[tokio::test(flavor = "multi_thread")]
async fn req_qr_2_reads_answer_at_the_state_the_agent_committed() {
    let core = open(Faulty::new());
    for i in ticks(1, 2) {
        core.apply(&i).await.unwrap();
    }
    let committed = core.last_tick().await.unwrap();
    assert_eq!(committed, Some(T0 + 2_000));
    let s = agent_inputs(&core);
    core.with_port(|p| {
        p.refuse = Some(s + 1);
        p.delay = Duration::from_millis(300);
    });
    let flight = {
        let core = core.clone();
        tokio::spawn(async move { core.apply(&ticks(3, 1)[0]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The replica has run tick 3 by now; the Agent has not committed it, and will not.
    assert_eq!(
        core.last_tick().await.unwrap(),
        committed,
        "a read in flight"
    );
    assert_eq!(flight.await.unwrap(), Err(CoreError::Unavailable));
    assert_eq!(
        core.last_tick().await.unwrap(),
        committed,
        "after the refusal"
    );
    // Committed: seen once the Agent has it.
    core.with_port(|p| p.refuse = None);
    let flight = {
        let core = core.clone();
        tokio::spawn(async move { core.apply(&ticks(3, 1)[0]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let read = core.last_tick().await.unwrap();
    assert_eq!(read, Some(T0 + 3_000));
    assert_eq!(
        agent_inputs(&core),
        s + 1,
        "read only once the Agent committed"
    );
    assert!(flight.await.unwrap().is_ok());
    assert_eq!(core.stopped(), None);
}

/// `NFR-18`, `REQ-CS-7`, `T-031-12`: a read storm does not starve the Agent core's decisions.
/// Sixteen readers query the replica back to back while the core applies ticks, each one an
/// invocation of the native Agent with 5 ms added (an invocation's time in VOS is longer still).
/// The rate under the storm stays within a small factor of the rate without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nfr_18_req_cs_7_a_read_storm_does_not_starve_the_agent_core() {
    use ingress::boundary::{CallerBody, Query};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Instant;

    let core = open(Faulty::new());
    core.with_port(|p| p.delay = Duration::from_millis(5));
    let founded = core.founded();
    let caller = CallerBody {
        principal: founded.operator.to_did(),
        credential: founded.operator_credential.to_did(),
        on_behalf_of: None,
    };
    let mut at = 0u32;
    let mut decide = async |window: Duration| {
        let start = Instant::now();
        let mut n = 0u32;
        while start.elapsed() < window {
            at += 1;
            let mut key = [0x5A; 16];
            key[..4].copy_from_slice(&at.to_be_bytes());
            let tick = CoreInput {
                key: InputKey(key),
                body: InputBody::Tick {
                    now: T0 + u64::from(at),
                },
            };
            core.apply(&tick).await.unwrap();
            n += 1;
        }
        u64::from(n) * 1_000_000 / u64::try_from(start.elapsed().as_micros()).unwrap().max(1)
    };
    let window = Duration::from_secs(2);
    let alone = decide(window).await;
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..16u8)
        .map(|r| {
            let (core, caller, stop, reads) = (
                core.clone(),
                caller.clone(),
                Arc::clone(&stop),
                Arc::clone(&reads),
            );
            let credential = founded.operator_credential.to_did();
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let _ = if r % 2 == 0 {
                        core.query(&caller, &Query::Assets, T0).await.map(|_| ())
                    } else {
                        core.credential(&credential).await.map(|_| ())
                    };
                    reads.fetch_add(1, Ordering::Relaxed);
                    // A credential asked before is answered from the committed credentials
                    // without waiting (`T-032-13`): give the other tasks their turn.
                    tokio::task::yield_now().await;
                }
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = reads.load(Ordering::Relaxed);
    let stormed = decide(window).await;
    let storm = (reads.load(Ordering::Relaxed) - before) / window.as_secs();
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        let _ = r.await;
    }
    println!(
        "T-031-12 Agent core: {alone} decisions/s alone; {stormed} decisions/s under \
         {storm} reads/s"
    );
    assert!(storm > 100, "the storm was a storm: {storm} reads/s");
    assert!(
        stormed * 5 >= alone,
        "decisions under the storm: {stormed}/s against {alone}/s alone"
    );
}

// T-031-14 -------------------------------------------------------------------------------------

/// `REQ-RT-3`, `INV-18`, `T-031-14`: with the replica's stores on disk, the core answers as with
/// them in memory; a refused input rebuilds them (a fresh directory, replayed from the log), and
/// so does a restart, after which every input is answered from its reply and none runs twice.
#[tokio::test(flavor = "multi_thread")]
async fn req_rt_3_a_replica_on_disk_rebuilds_after_a_refusal_and_a_restart() {
    use core_binding::host::ReplicaStores;
    let dir = std::env::temp_dir().join(format!("aledgent-replica-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let stores = ReplicaStores::Disk {
        dir: dir.join("replica"),
        cache_bytes: 1024 * 1024,
    };
    let reference = open(Faulty::new());
    let log = MemLog::default();
    let core = AgentCore::open_in(Faulty::new(), founding(), log.clone(), stores.clone()).unwrap();
    let inputs = ticks(1, 8);
    let mut singles = Vec::new();
    for i in &inputs {
        singles.push(reference.apply(i).await);
    }
    let s = agent_inputs(&core);
    core.with_port(|p| p.refuse = Some(s + 4));
    let r = core.apply_batch(&inputs).await;
    assert_eq!(r[..3], singles[..3]);
    assert!(r[3..].iter().all(|r| *r == Err(CoreError::Unavailable)));
    core.with_port(|p| p.refuse = None);
    assert_eq!(core.apply_batch(&inputs[3..]).await, singles[3..]);
    assert_eq!(core.last_tick().await, reference.last_tick().await);
    assert!(
        dir.join("replica")
            .join("book")
            .join("leaves.redb")
            .exists()
    );
    let port = core
        .with_port(|p| std::mem::replace(p, Faulty::new()))
        .unwrap();
    drop(core);
    let again = AgentCore::open_in(port, founding(), log, stores).unwrap();
    assert_eq!(again.apply_batch(&inputs).await, singles);
    assert_eq!(agent_inputs(&again), s + 8);
    assert_eq!(again.stopped(), None);
    drop(again);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `REQ-AU-3`, `NFR-20` (`T-032-13`, stress finding F14): the Agent core answers a credential it
/// was asked before, or that a decision created, from what it last committed, without waiting for a decision in flight, and a
/// removal takes effect at the commit that made it: the next read after the removal's reply is
/// already "no working credential", with ingress's own cache turned off.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_3_a_credential_is_read_as_committed_and_a_removal_counts_at_its_commit() {
    let (d, core, _log) = deployment().await;
    d.auth
        .verifiers()
        .set_ttl(Duration::ZERO)
        .unwrap_or_else(|e| panic!("{e}"));
    let alice = d.party("alice", 100).await;
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[("client-key", "ak")],
            Some(json!({ "marker": "ordinary", "scheme": "api-key" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let credential = v["result"]["credential"].as_str().unwrap().to_owned();
    // Created by a decision: already among the committed credentials, so even the first read
    // does not wait for the gate; and again from there.
    let committed = || {
        core.stats()
            .committed_reads
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let before = committed();
    let first = core.credential(&credential).await.unwrap();
    assert_eq!(
        first.as_ref().map(|c| c.identity.clone()),
        Some(alice.identity.clone())
    );
    assert_eq!(core.credential(&credential).await.unwrap(), first);
    assert_eq!(committed(), before + 2);
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/credentials/{credential}/remove"),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(core.credential(&credential).await.unwrap(), None);
}

// T-013-13 -------------------------------------------------------------------------------------

mod audit {
    use super::*;
    use audit_store::{AuditAppender, AuditStoreConfig, FileAuditStore, WriterCredential};
    use core_binding::host::ReplicaStores;
    use ingress::audit_ship::Shipper;
    use ingress::boundary::{CallerBody, Query, QueryReply};
    use ledger_core::audit::{AuditQuery, AuditSink as _, AuditTrail};

    struct Store {
        _dir: tempfile::TempDir,
        config: AuditStoreConfig,
    }

    fn store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        let book = dir.path().join("state");
        std::fs::create_dir_all(&book).unwrap();
        let config = AuditStoreConfig {
            dir: dir.path().join("audit"),
            book_dir: book,
            segment_records: 16,
        };
        Store { _dir: dir, config }
    }

    fn writer() -> WriterCredential {
        WriterCredential::new(b"test-audit-writer-credential".to_vec())
    }

    fn appender(s: &Store) -> AuditAppender {
        AuditAppender::open(&s.config, &writer()).unwrap()
    }

    /// A core over `port` and `log` shipping to `s`, with a shipper the test steps itself.
    fn audited<P: AgentPort + Send + 'static>(
        port: P,
        log: MemLog,
        s: &Store,
        max: u64,
    ) -> (AgentCore<P>, Shipper) {
        let sink = appender(s);
        let core = AgentCore::open_audited_unstarted(
            port,
            founding(),
            log,
            ReplicaStores::Memory,
            sink.head().unwrap(),
            max,
        )
        .unwrap();
        let shipper = Shipper::new(Box::new(sink), core.audit().unwrap());
        (core, shipper)
    }

    async fn assemble(core: &AgentCore<Faulty>) -> Deployment<MemStore, AgentCore<Faulty>> {
        Deployment::assemble(
            MemStore::new(Duration::from_millis(50)),
            fast_config("ingress-0"),
            core.clone(),
            core.founded(),
            MemJournal::new(),
        )
        .await
    }

    fn unshipped(core: &AgentCore<Faulty>) -> u64 {
        core.audit()
            .unwrap()
            .stats
            .unshipped
            .load(Ordering::Relaxed)
    }

    /// The trail the store holds: read through the reader's own checks (every segment, the
    /// chain, `HEAD`).
    fn held(s: &Store) -> Vec<ledger_core::audit::AuditRecord> {
        FileAuditStore::read(&s.config.dir)
            .unwrap()
            .query(&AuditQuery::default())
            .into_iter()
            .cloned()
            .collect()
    }

    /// `REQ-AT-1`, `REQ-AT-2`, `NFR-6`, `T-013-13`: the core ships its audit trail to the audit
    /// store, each record exactly once and in order, and the outbox is empty once the store
    /// confirmed it. Across a restart nothing is lost and nothing doubled: the rebuilt replica
    /// drops what the store holds as it replays, and what follows continues the chain. A core
    /// reopened without knowing the store's head offers the trail again, and the store appends
    /// nothing twice.
    #[tokio::test(flavor = "multi_thread")]
    async fn req_at_2_the_core_ships_its_trail_once_in_order_and_across_restarts() {
        let s = store();
        let log = MemLog::default();
        let (core, mut shipper) = audited(Faulty::new(), log.clone(), &s, 100_000);
        let d = assemble(&core).await;
        let alice = d.party("alice", 1_000).await;
        let bob = d.party("bob", 0).await;
        for n in 0..3 {
            let (st, v) = d
                .call_decided(
                    "POST",
                    "/v1/transfers",
                    Some(&alice.token),
                    &[("client-key", &format!("t{n}"))],
                    Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "2" }] })),
                )
                .await;
            assert_eq!(st.as_u16(), 200, "transfer: {v}");
        }
        assert!(d.quiesce(Duration::from_secs(20)).await);
        let waiting = unshipped(&core);
        assert!(waiting > 0, "commands left audit records");
        assert_eq!(core.ship(&mut shipper), Some(Ok(waiting)));
        assert_eq!(
            unshipped(&core),
            0,
            "the outbox is empty once the store holds it"
        );
        assert_eq!(core.ship(&mut shipper), Some(Ok(0)), "nothing ships twice");
        let first = held(&s);
        assert_eq!(first.len() as u64, waiting);
        assert_eq!(
            AuditTrail::verify_chain(&first).map(|h| h.is_some()),
            Ok(true)
        );

        // A restart: the store's head is the floor; the replica holds nothing the store has.
        let port = core
            .with_port(|p| std::mem::replace(p, Faulty::new()))
            .unwrap();
        drop(shipper);
        let (again, mut shipper) = audited(port, log.clone(), &s, 100_000);
        assert_eq!(
            unshipped(&again),
            0,
            "the rebuild drops what the store holds"
        );
        let d2 = assemble(&again).await;
        d2.party("carol", 5).await;
        assert!(d2.quiesce(Duration::from_secs(20)).await);
        assert!(unshipped(&again) > 0);
        assert!(matches!(again.ship(&mut shipper), Some(Ok(n)) if n > 0));
        let second = held(&s);
        assert!(second.len() > first.len());
        assert_eq!(
            second[..first.len()],
            first[..],
            "the chain is continued, not rewritten"
        );
        assert_eq!(
            AuditTrail::verify_chain(&second).map(|h| h.is_some()),
            Ok(true)
        );

        // Reopened as if the store's head were lost: the whole trail is offered again, the store
        // takes it as already held, and appends nothing.
        let port = again
            .with_port(|p| std::mem::replace(p, Faulty::new()))
            .unwrap();
        drop(shipper);
        let sink = appender(&s);
        let blind = AgentCore::open_audited_unstarted(
            port,
            founding(),
            log,
            ReplicaStores::Memory,
            None,
            100_000,
        )
        .unwrap();
        let mut shipper = Shipper::new(Box::new(sink), blind.audit().unwrap());
        assert!(unshipped(&blind) >= second.len() as u64);
        assert!(matches!(blind.ship(&mut shipper), Some(Ok(_))));
        assert_eq!(unshipped(&blind), 0);
        assert_eq!(held(&s), second, "no record twice");
    }

    /// `T-013-13`, `REQ-CS-7`, `NFR-11`: while the store does not take records and the outbox is
    /// at its bound, the core decides nothing: inputs answer `Unavailable` and wait `Received`.
    /// The signals say so. Once the store takes the records again, the inputs decide, in order.
    #[tokio::test(flavor = "multi_thread")]
    async fn req_at_2_the_core_holds_inputs_back_while_the_store_does_not_take_records() {
        let s = store();
        let (core, mut shipper) = audited(Faulty::new(), MemLog::default(), &s, 100_000);
        let d = assemble(&core).await;
        let alice = d.party("alice", 1_000).await;
        // An operations principal that reads the signals (`F-018`).
        let op = Some(d.operator.token.as_str());
        let (st, v) = d
            .call_decided(
                "POST",
                "/v1/identities",
                op,
                &[("client-key", "ops-identity")],
                Some(json!({ "kind": "organisation", "verifier": "0c" })),
            )
            .await;
        assert_eq!(st.as_u16(), 200, "{v}");
        let operator = CallerBody {
            principal: v["result"]["identity"].as_str().unwrap().to_owned(),
            credential: v["result"]["credential"].as_str().unwrap().to_owned(),
            on_behalf_of: None,
        };
        let (st, v) = d
            .call_decided(
                "POST",
                "/v1/ops/operator-grants",
                op,
                &[("client-key", "ops-grant")],
                Some(json!({
                    "principal": operator.principal,
                    "kinds": ["read_signals"],
                    "justification": "T-013-13: the signals",
                })),
            )
            .await;
        assert_eq!(st.as_u16(), 200, "{v}");
        assert!(d.quiesce(Duration::from_secs(20)).await);
        let a = core.audit().unwrap();
        // The bound, lowered to what waits now: the outbox is full.
        a.stats.max_unshipped.store(1, Ordering::Relaxed);
        let before = agent_inputs(&core);
        assert_eq!(
            core.apply(&ticks(200, 1)[0]).await,
            Err(CoreError::Unavailable)
        );
        assert_eq!(agent_inputs(&core), before, "nothing reached the Agent");
        let signals = |c: &AgentCore<Faulty>| {
            let c = c.clone();
            let caller = operator.clone();
            async move {
                match c.query(&caller, &Query::Signals, T0 + 1_000_000).await {
                    Ok(QueryReply::Signals(s)) => s,
                    other => panic!("signals: {other:?}"),
                }
            }
        };
        let sig = signals(&core).await;
        let audit = sig.audit.expect("the core reports its shipping");
        assert!(audit.backpressure && audit.unshipped > 0 && audit.store_up);
        assert!(audit.oldest_unshipped_age.is_some());
        assert!(!sig.core_halted);
        // `T-018-11`: the replica counts the book for the Agent core too.
        let book = sig.book.expect("the Agent core's replica counts the book");
        assert!(book.records > 0 && book.leaves > book.records, "{book:?}");
        // A command meanwhile is acknowledged and waits.
        let (st, _) = d
            .call(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[("client-key", "held")],
                Some(json!({ "payer": alice.account, "asset": USD, "amount": "1" })),
            )
            .await;
        assert_eq!(st.as_u16(), 202, "acknowledged at Received");
        // The store takes the records: the core decides again (the bound back where it was).
        assert!(matches!(core.ship(&mut shipper), Some(Ok(n)) if n > 0));
        assert!(!a.stats.full());
        a.stats.max_unshipped.store(100_000, Ordering::Relaxed);
        assert!(
            d.quiesce(Duration::from_secs(20)).await,
            "the held input decides"
        );
        let sig = signals(&core).await;
        assert!(!sig.audit.unwrap().backpressure);
    }

    /// `T-013-13`, `REQ-AT-2`: a store that refuses the core's record (here, one that already holds
    /// another trail) stops shipping, says so in the signals, and keeps every record in the
    /// outbox: nothing is dropped that the store does not hold.
    #[tokio::test(flavor = "multi_thread")]
    async fn req_at_2_a_store_holding_another_trail_stops_shipping_and_drops_nothing() {
        let s = store();
        // Another book's trail in the store.
        {
            let (core, mut shipper) = audited(Faulty::new(), MemLog::default(), &s, 100_000);
            let d = assemble(&core).await;
            d.party("someone-else", 3).await;
            assert!(d.quiesce(Duration::from_secs(20)).await);
            assert!(matches!(core.ship(&mut shipper), Some(Ok(_))));
        }
        let held_before = held(&s);
        let sink = appender(&s);
        // This core does not know the store's head: its genesis record differs from the other
        // book's at number 0 (another genesis time).
        let mut f = founding();
        f.at += 1;
        let core = AgentCore::open_audited_unstarted(
            Faulty::new(),
            f,
            MemLog::default(),
            ReplicaStores::Memory,
            None,
            100_000,
        )
        .unwrap();
        let mut shipper = Shipper::new(Box::new(sink), core.audit().unwrap());
        let waiting = unshipped(&core);
        assert!(waiting > 0);
        assert!(matches!(core.ship(&mut shipper), Some(Err(_))));
        let a = core.audit().unwrap();
        assert!(a.stats.refused.load(Ordering::Relaxed));
        assert_eq!(unshipped(&core), waiting, "nothing left the outbox");
        assert_eq!(held(&s), held_before, "the store is unchanged");
    }

    /// `T-037-15`, `REQ-RT-20`, amendment 0033 D25, `NFR-11`: an input the fence refuses (here a
    /// package of the next generation installed while nothing is scheduled) commits nothing and is
    /// audited at the execution node, in the audit store's node trail and on standard error, never
    /// in the book: the book's trail does not move, and the Agent's input number is not taken. It
    /// never counts toward the halt, however often it is retried; the fenced clock runs from the
    /// first refused input's time and stops when an input commits.
    #[tokio::test(flavor = "multi_thread")]
    async fn t037_15_a_fenced_input_is_audited_at_the_node_never_in_the_book() {
        let s = store();
        let (core, mut shipper) = audited(NativePort::new(), MemLog::default(), &s, 100_000);
        assert!(matches!(core.ship(&mut shipper), Some(Ok(_))));
        core.set_node_trail(audit_store::NodeTrail::open(&s.config, &writer()).unwrap());
        let before = agent_inputs(&core);
        let book_before = held(&s).len();
        core.with_port(|p| p.generation = 1);
        let tick = ticks(1, 1).remove(0);
        for _ in 0..FAILED_ATTEMPTS + 2 {
            // The fence's refusal is a definite `NotRun` (`T-037-18`, amendment 0036), not the
            // `Unavailable` of an input the core never reached.
            assert_eq!(core.apply(&tick).await, Err(CoreError::NotRun));
        }
        assert_eq!(core.stopped(), None, "the fence never halts the core");
        assert_eq!(core.stats().halted.load(Ordering::Relaxed), 0);
        assert_eq!(agent_inputs(&core), before, "its number is not taken");
        assert_eq!(
            core.stats().fenced_since.load(Ordering::Relaxed),
            T0 + 1_000,
            "the clock runs from the first refused input's time"
        );
        // Audited at the node: one record per refusal, naming the input by key, kind and time.
        let node = audit_store::NodeTrail::read(&s.config.dir).records;
        assert_eq!(node.len() as u32, FAILED_ATTEMPTS + 2);
        for r in &node {
            assert_eq!(
                (r.kind.as_str(), r.input, r.at),
                ("fenced", before + 1, T0 + 1_000)
            );
            assert!(r.detail.contains("generation 1") && r.detail.contains("tick at"));
        }
        // Never in the book: its trail holds nothing more.
        assert!(matches!(core.ship(&mut shipper), Some(Ok(_))));
        assert_eq!(held(&s).len(), book_before);
        // The code the state names, installed: the same input commits, and the clock stops.
        core.with_port(|p| p.generation = 0);
        assert!(core.apply(&tick).await.is_ok());
        assert_eq!(agent_inputs(&core), before + 1);
        assert_eq!(core.stats().fenced_since.load(Ordering::Relaxed), 0);
    }
}
