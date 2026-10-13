// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What the cores hold in memory per command (`T-031-14`, finding F6 of `F-032`).
//!
//! The stress suite's soak saw the in-process core grow by about 27 KB per command and the core
//! host by about 10 KB. This test measures the same thing natively, under a counting allocator:
//! a production-shaped workload (onboarding, transfers between holders, reservations and their
//! captures, issuance, ticks and delivery records) is recorded once through a deployment over the
//! in-process core, then replayed input by input into a fresh core of each kind, and the live
//! heap is read as it goes. What the core keeps for ever, divided by the commands that made it,
//! is the figure F6 reported.
//!
//! The counting allocator counts the whole process: this binary holds this one test only.
//! `ALEDGENT_MEMORY_ROUNDS` runs a longer workload by hand (`NOTES-stress-reads.md` reports one).

use std::collections::HashMap;
use std::time::Duration;

use core_binding::core::{AgentCore, FileLog, Founding};
use core_binding::host::{NativePort, ReplicaStores};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::boundary::{Core as _, CoreInput, CoreReply, InputBody, InputKey};
use ingress::inprocess::{InProcessCore, Journal};
use ingress::testkit::{Deployment, PATIENCE, T0, USD, genesis, test_clerk};
use peak_alloc::PeakAlloc;
use serde_json::{Value, json};

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

/// A journal that keeps nothing: the measurement is of the core, not of a test journal.
struct NoJournal;

impl Journal for NoJournal {
    fn entries(&self) -> Result<Vec<(u64, CoreInput)>, String> {
        Ok(Vec::new())
    }
    fn append(&mut self, _: u64, _: &CoreInput) -> Result<(), String> {
        Ok(())
    }
}

fn rounds() -> usize {
    std::env::var("ALEDGENT_MEMORY_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12)
}

/// The holders funded before the rounds.
const HOLDERS: usize = 12;

/// Record a workload: every input the in-process core applied, in order.
async fn record(rounds: usize) -> Vec<CoreInput> {
    let mut d = Deployment::start().await;
    let mut holders = Vec::new();
    for i in 0..HOLDERS {
        holders.push(d.party(&format!("h{i}"), 1_000_000).await);
    }
    for r in 0..rounds {
        // Transfers between holders, acknowledged at `Received`.
        for (i, h) in holders.iter().enumerate() {
            let to = holders[(i + 1) % holders.len()].account.clone();
            let (s, v) = d
                .call(
                    "POST",
                    "/v1/transfers",
                    Some(&h.token),
                    &[("client-key", &format!("t-{r}-{i}"))],
                    Some(json!({ "legs": [{ "from": h.account, "to": to, "asset": USD, "amount": "3" }] })),
                )
                .await;
            assert!(s.as_u16() == 202 || s.as_u16() == 200, "transfer: {s} {v}");
        }
        // Reservations (a card's authorisation), then their captures.
        let mut refs = Vec::new();
        for (i, h) in holders.iter().enumerate() {
            let (s, v) = d
                .call(
                    "POST",
                    "/v1/reservations",
                    Some(&h.token),
                    &[("client-key", &format!("r-{r}-{i}"))],
                    Some(json!({ "payer": h.account, "asset": USD, "amount": "5" })),
                )
                .await;
            assert!(s.as_u16() == 202 || s.as_u16() == 200, "reserve: {s} {v}");
            refs.push(reference(&v));
        }
        assert!(d.quiesce(PATIENCE).await, "the round is decided");
        for (i, (h, op)) in holders.iter().zip(&refs).enumerate() {
            let to = holders[(i + 2) % holders.len()].account.clone();
            let (s, v) = d
                .call(
                    "POST",
                    &format!("/v1/reservations/{op}/capture"),
                    Some(&h.token),
                    &[],
                    Some(json!({ "destination": to, "cleared": "5" })),
                )
                .await;
            assert_eq!(s.as_u16(), 200, "capture: {v}");
        }
        // Onboarding: an identity, an account, an issuance.
        if r % 3 == 0 {
            d.party(&format!("new{r}"), 100).await;
        }
        // Time passes; deliveries are acknowledged.
        assert!(d.all_delivered(PATIENCE).await, "delivered");
    }
    assert!(d.quiesce(PATIENCE).await, "every row is decided");
    let inputs = d
        .journal
        .entries()
        .unwrap_or_else(|e| panic!("the journal reads: {e}"))
        .into_iter()
        .map(|(_, i)| i)
        .collect();
    d.replica.kill();
    inputs
}

fn reference(v: &Value) -> String {
    v["operation_reference"]
        .as_str()
        .unwrap_or_else(|| panic!("no operation reference in {v}"))
        .to_owned()
}

fn commands(inputs: &[CoreInput]) -> usize {
    inputs
        .iter()
        .filter(|i| matches!(i.body, InputBody::Command { .. }))
        .count()
}

/// The live heap after each tenth of the inputs.
struct Run {
    samples: Vec<(usize, usize)>,
}

impl Run {
    /// Bytes held per command over the second half of the run: the slope, not the start-up.
    fn per_command(&self, inputs: &[CoreInput]) -> usize {
        let n = self.samples.len();
        let (a, b) = (self.samples[n / 2], self.samples[n - 1]);
        let cmds = commands(&inputs[a.0..b.0]).max(1);
        b.1.saturating_sub(a.1) / cmds
    }
}

async fn replay<C: ingress::boundary::Core>(core: &C, inputs: &[CoreInput]) -> Run {
    let step = (inputs.len() / 10).max(1);
    let mut samples = vec![(0, PEAK.current_usage())];
    for (at, input) in inputs.iter().enumerate() {
        core.apply(input)
            .await
            .unwrap_or_else(|e| panic!("input {at} applies: {e:?}"));
        if (at + 1) % step == 0 || at + 1 == inputs.len() {
            samples.push((at + 1, PEAK.current_usage()));
        }
    }
    Run { samples }
}

/// What a map of every reply holds: what a core that keeps them all for ever spends on them.
fn replies_bytes(replies: &[(InputKey, CoreReply)]) -> usize {
    let before = PEAK.current_usage();
    let map: HashMap<InputKey, CoreReply> = replies.iter().cloned().collect();
    let held = PEAK.current_usage().saturating_sub(before);
    drop(map);
    held
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

/// The page cache the measurement gives each file: small, so that a short run shows what a long
/// one does once the cache is full (deployments use 32 to 64 MiB).
const CACHE: usize = 1024 * 1024;

/// One core, measured: bytes held at the end, bytes per command over the second half.
struct Measured {
    held: usize,
    per_command: usize,
}

async fn measure<C: ingress::boundary::Core>(
    core: &C,
    base: usize,
    inputs: &[CoreInput],
) -> Measured {
    let run = replay(core, inputs).await;
    Measured {
        held: PEAK.current_usage().saturating_sub(base),
        per_command: run.per_command(inputs),
    }
}

/// Bytes under `dir`.
fn du(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => du(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    })
}

fn agent_core(dir: &std::path::Path, stores: ReplicaStores) -> AgentCore<NativePort> {
    AgentCore::open_in(
        NativePort::new(),
        founding(),
        FileLog::open(dir.join("log")).unwrap_or_else(|e| panic!("the log opens: {e}")),
        stores,
    )
    .unwrap_or_else(|e| panic!("the Agent core opens: {e}"))
}

/// `T-031-14`, F6: what the in-process core and the Agent core's host hold per command, over a
/// production-shaped workload, before and after the leaves moved to disk and the Agent core's
/// replies were bounded. Reported for `NOTES-stress-reads.md`; the after figures are asserted
/// with room for allocator noise and the machine.
#[tokio::test(flavor = "multi_thread")]
async fn t_031_14_memory_held_per_command() {
    let inputs = record(rounds()).await;
    let cmds = commands(&inputs).max(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let dir = std::env::temp_dir().join(format!("aledgent-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // The in-process core with its leaves in memory, as before `T-031-14`.
    let base = PEAK.current_usage();
    let core = InProcessCore::in_memory(genesis(T0), NoJournal).with_clerk(test_clerk());
    core.start()
        .unwrap_or_else(|e| panic!("the core starts: {e}"));
    let mem = measure(&core, base, &inputs).await;
    // What the service store holds, canonically: a floor under its share of memory.
    let service = core
        .with_ledger(|l| {
            l.state()
                .all_leaves()
                .iter()
                .map(|(_, k, v)| k.len() + v.len())
                .sum::<usize>()
        })
        .unwrap_or(0);
    let store_bytes = core
        .with_ledger(|l| {
            let before = PEAK.current_usage();
            let c = l.book().store().clone();
            let held = PEAK.current_usage().saturating_sub(before);
            drop(c);
            held
        })
        .unwrap_or(0);
    let mut replies = Vec::new();
    for input in &inputs {
        if let Ok(r) = core.apply(input).await {
            replies.push((input.key, r));
        }
    }
    let replies_held = replies_bytes(&replies);
    drop(replies);
    drop(core);

    // The in-process core with its leaves on disk (`aledgent-ingress serve` since `T-031-14`).
    let base = PEAK.current_usage();
    let core = InProcessCore::on_disk(genesis(T0), NoJournal, dir.join("book.redb"), CACHE)
        .with_clerk(test_clerk());
    core.start()
        .unwrap_or_else(|e| panic!("the core starts: {e}"));
    let disk = measure(&core, base, &inputs).await;
    drop(core);
    let file = std::fs::metadata(dir.join("book.redb")).map_or(0, |m| m.len());

    // The Agent core's host, its replica in memory, as before `T-031-14`: the replica, the log's
    // index and the Agent's own state (the native port's; bounded, `tests/heap.rs`).
    let base = PEAK.current_usage();
    let core = agent_core(&dir.join("a"), ReplicaStores::Memory);
    let agent_mem = measure(&core, base, &inputs).await;
    drop(core);

    // Its replica on disk (`aledgent-core-host serve` since `T-031-14`).
    let base = PEAK.current_usage();
    let core = agent_core(
        &dir.join("b"),
        ReplicaStores::Disk {
            dir: dir.join("b").join("replica"),
            cache_bytes: CACHE,
        },
    );
    let agent_disk = measure(&core, base, &inputs).await;
    drop(core);
    let replica = du(&dir.join("b").join("replica"));
    let _ = std::fs::remove_dir_all(&dir);

    println!(
        "F6: {} inputs, {cmds} commands.\n\
         in-process core, leaves in memory: {} B per command ({} B held)\n\
         in-process core, leaves on disk:   {} B per command ({} B held, {CACHE} B cache)\n\
         Agent core host, replica in memory: {} B per command ({} B held)\n\
         Agent core host, replica on disk:   {} B per command ({} B held, {CACHE} B cache each)\n\
         on disk: the in-process core's leaf file {} B per command; the replica {} B per command\n\
         the kernel's leaf store (a copy of it): {} B per command\n\
         every reply in a map: {} B per command; the service store's leaves, canonically: {} B \
         per command",
        inputs.len(),
        mem.per_command,
        mem.held,
        disk.per_command,
        disk.held,
        agent_mem.per_command,
        agent_mem.held,
        agent_disk.per_command,
        agent_disk.held,
        file / cmds as u64,
        replica / cmds as u64,
        store_bytes / cmds,
        replies_held / cmds,
        service / cmds,
    );
    // The in-process core with its leaves in memory, the default: its node tables are pruned
    // (`ledger_kernel::store::smt::MEM_NODES_DEPTH`), about 32 KB a command before, 15 KB after.
    assert!(
        mem.per_command < 20_480,
        "in-process core, leaves in memory: {} B per command",
        mem.per_command
    );
    // The opt-in disk modes: the leaves' Merkle nodes no longer accumulate in memory.
    for (what, before, after) in [
        ("in-process core", mem.per_command, disk.per_command),
        (
            "Agent core host",
            agent_mem.per_command,
            agent_disk.per_command,
        ),
    ] {
        assert!(
            after < before && after < 16_384,
            "{what}: {after} B per command on disk against {before} B in memory"
        );
    }
}
