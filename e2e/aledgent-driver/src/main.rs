// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The zombienet suite's **aledgent driver** (`T-026-04`, issue #75; `T-027-06`, issues #76 and
//! #96): step 3 and the fault cases through aledgent instead of the script. Ingress and the
//! in-process core take the commands, here; **the settlement adapter runs as it runs in
//! production**: the `aledgent-settle` binary (`services/settle`, `F-016`/`F-027`), its own process
//! with its own PostgreSQL store, reaching Kreivo **over its light client** and the core over
//! ingress's HTTP surface. It executes the call outbox on the settlement contract as the
//! deployment's community PASS account and observes the contract's inbox at finality.
//!
//! ```text
//! aledgent-e2e-driver keys   # the PASS accounts, device keys and clerk key the chain must know
//! aledgent-e2e-driver run    # step 3, then (on `faults.go`) the fault cases; see below
//! ```
//!
//! `keys` prints what `e2e.mjs aledgent-setup` registers on the chain: each lane's PASS account
//! (its hashed user id, its device's secp256k1 secret, device id and account), the anchor key's,
//! and the core's clerk key the contract publishes as its attestation key. They are the adapter's
//! test constants (`settle::testkit`): public, worth nothing anywhere.
//!
//! `run` is one long-lived process, since the core it hosts is in memory (`e2e/drivers/aledgent.sh`
//! starts it and hands it the phases): step 3, then `step3.done` (or `step3.failed`) in its state
//! directory; then, once `faults.go` appears there, the fault cases, then `faults.done` (or
//! `faults.failed`). It stops the adapter and removes its containers on the way out.
//!
//! ## Where the adapter runs
//!
//! In a container (`ALEDGENT_IMAGE`, the image the binary was built in), in a network namespace of
//! its own held by a long-lived container that also carries `iptables` (`NET_ADMIN` in that
//! namespace only). The namespace sits on a Docker network of the run's own, with the adapter's
//! PostgreSQL. So the harness can, without touching the runner's own network:
//!
//! - **starve the light client of peers**: reject every packet leaving the namespace except to
//!   PostgreSQL and to ingress; and let them through again;
//! - **kill the adapter** where it stands, and start it again over the same store;
//! - point its light client at **the pruned node only**, blocking the collator's parachain port.
//!
//! The nodes listen on every interface, so the chain specs' boot nodes (one per chain, F-026 NOTES
//! finding 5) are rewritten to the Docker network's gateway.
//!
//! ## Environment
//!
//! - `E2E_OUT`: `aledgent-env.json` (from `e2e.mjs aledgent-setup`); `expect.json` goes there, and
//!   the state directory `aledgent/`.
//! - `E2E_SCRIPTS`: `contracts/settlement/scripts`, where it runs `node e2e.mjs <helper>`: the
//!   customer's deposits (a customer signs those, not aledgent), the chain-side proofs, and the
//!   fault cases' records.
//! - `LIGHT_KREIVO_CHAIN_SPEC`, `LIGHT_RELAY_CHAIN_SPEC`: the raw chain specs `spawn_network`
//!   wrote with one local node per chain as boot node. `KREIVO_PRUNED_P2P`: the pruned node's
//!   multiaddr. `KREIVO_WS`: the collator's RPC (only with `ALEDGENT_CONNECTION=rpc`).
//! - `SETTLE_BIN`: the `aledgent-settle` binary, built with `fault-injection`.
//! - `ALEDGENT_IMAGE` (`rust:1-bookworm`), `ALEDGENT_PG_IMAGE` (`postgres:16-alpine`),
//!   `ALEDGENT_TAG` (names the containers; default the run id), `INGRESS_PORT` (18089),
//!   `ALEDGENT_CONNECTION` (`light-client`, or `rpc`, the adapter's opt-in fallback).
//!
//! What it checks itself is aledgent's own view, which the chain-side check cannot see: deposits
//! credited to the ledger account their reference names, withdrawals settled exactly once, the
//! refused one reported and not released, the checkpoint final. `e2e.mjs check` then checks the
//! chain side of the same operations from `expect.json`.

mod harness;
mod stress;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ingress::testkit::{ANCHOR_INTERVAL_MS, Deployment, PATIENCE, Party, USD, test_clerk};
use ledger_core::id::{AccountId, OperationId, account_reference};
use ledger_core::settlement::envelope::anchor_reference;
use serde_json::{Value, json};
use settle::adapter::chain_ref;
use settle::testkit::{
    PAYEE, TEST_ANCHOR_PASS_USER, TEST_ANCHOR_SECRET, TEST_LANE_SECRET, TEST_PASS_USER,
    ingress_with_adapter, pass_lane,
};

use harness::{Harness, Variant};

/// The deployment's instance in the book, bound to the suite's contract.
const INSTANCE: u32 = 1;
/// What the customer deposits, in minor units of the suite's `Here(1)` (2 decimals: the book's
/// `USD`).
const DEPOSIT: u64 = 10_000;
/// What a withdrawal takes, in minor units.
const WITHDRAW: u64 = 2_500;
/// What the refused withdrawal asks for, in minor units.
const REFUSED: u64 = 20;
/// The book-only asset (F-029, amendment 0011): its id, its location on the suite's Kreivo, and
/// what the book issues of it before the binding, and moves.
const BOOK_ONLY: &str = "BKO";
const BOOK_ONLY_INDEX: u32 = 3;
const BOOK_ONLY_ISSUED: u64 = 12_345;
const BOOK_ONLY_MOVED: u64 = 2_345;
/// What each fault case's withdrawal takes.
const FAULT_WITHDRAW: u64 = 100;
/// What each fault case's deposit brings.
const FAULT_DEPOSIT: u64 = 500;
/// How long an outcome may take through the light client (a withdrawal settled in 33 to 46 s
/// over RPC; the light client resyncs after a fault).
const PATIENT: Duration = Duration::from_secs(600);
/// How long any one wait inside a fault case may take: long enough for the adapter to find itself
/// blind for three minutes, restart with a new light client and resync.
const AFTER_FAULT: Duration = Duration::from_secs(420);
/// The most one fault case may take in all; past it, it fails with the adapter's last log lines
/// rather than running into the step's own timeout.
const CASE: Duration = Duration::from_secs(660);

pub(crate) fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set (see e2e/README.md)"))
}

pub(crate) fn say(line: impl AsRef<str>) {
    println!("[aledgent] {}", line.as_ref());
}

fn hex32(v: &Value, what: &str) -> [u8; 32] {
    v.as_str()
        .and_then(|s| hex::decode(s.trim_start_matches("0x")).ok())
        .and_then(|b| b.try_into().ok())
        .unwrap_or_else(|| panic!("{what}: 32 bytes of hex, got {v}"))
}

/// A PASS account as `keys` prints it.
fn lane_json(secret: &[u8; 32], user: [u8; 32]) -> Value {
    let lane = pass_lane(secret, user);
    json!({
        "user": hex::encode(user),
        "secret": hex::encode(secret),
        "device": hex::encode(lane.device()),
        "account": hex::encode(lane.account()),
    })
}

fn keys() {
    let out = json!({
        "instance": INSTANCE,
        "lanes": [lane_json(&TEST_LANE_SECRET, TEST_PASS_USER)],
        "anchor": lane_json(&TEST_ANCHOR_SECRET, TEST_ANCHOR_PASS_USER),
        "attestationKey": hex::encode(test_clerk().public_key()),
    });
    println!("{out}");
}

// ------------------------------------------------------------------------------ ingress

async fn state(d: &Deployment, party: &Party, op: &str) -> String {
    let (_, v) = d
        .call(
            "GET",
            &format!("/v1/operations/{op}"),
            Some(&party.token),
            &[],
            None,
        )
        .await;
    v["state"].as_str().unwrap_or_default().to_owned()
}

/// `party`'s (available, reserved) `USD`.
async fn balances(d: &Deployment, party: &Party) -> (u64, u64) {
    let account = party.account.clone().unwrap_or_default();
    let (_, v) = d
        .call(
            "GET",
            &format!("/v1/balances?account={account}&asset={USD}"),
            Some(&party.token),
            &[],
            None,
        )
        .await;
    (amount(&v, "available"), amount(&v, "reserved"))
}

fn amount(v: &Value, kind: &str) -> u64 {
    match v {
        Value::Object(m) => m
            .get(kind)
            .and_then(|x| {
                x.as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| x.as_u64())
            })
            .or_else(|| m.values().map(|x| amount(x, kind)).find(|a| *a > 0))
            .unwrap_or(0),
        Value::Array(a) => a
            .iter()
            .map(|x| amount(x, kind))
            .find(|a| *a > 0)
            .unwrap_or(0),
        _ => 0,
    }
}

/// [`withdrawal`], as a client would: refused because the adapter does not serve for the moment
/// (`ERR-SettlementLayerUnavailable`, `REQ-SA-2`), asked again, under a new client key (the
/// refusal reserved nothing), for up to two minutes. Over the light client the adapter loses its
/// only peer for about twenty seconds now and then on the suite's network (F-027 NOTES, "Open"),
/// and a withdrawal made then is refused: seen once in step 3 (run 36622867191).
async fn patient_withdrawal(
    d: &Deployment,
    party: &Party,
    amount: u64,
    to: [u8; 32],
) -> Result<String, String> {
    let start = Instant::now();
    loop {
        match withdrawal(d, party, amount, to).await {
            Err(e)
                if e.contains("ERR-SettlementLayerUnavailable")
                    && start.elapsed() < Duration::from_secs(120) =>
            {
                say(format!("a withdrawal was refused, asked again in 5 s: {e}"));
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            other => return other,
        }
    }
}

/// Reserve `amount` and submit it to `to`: the operation reference, or what ingress answered.
async fn withdrawal(
    d: &Deployment,
    party: &Party,
    amount: u64,
    to: [u8; 32],
) -> Result<String, String> {
    // A client key of its own for each: two withdrawals of the same amount under one key are one
    // operation (`REQ-OP-10`).
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = format!(
        "e2e-withdrawal-{}",
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    // Acknowledged at `Received` (amendment 0007, `OQ-61` as ruled on 2026-09-29): wait for the
    // decision before submitting.
    let (s, v) = d
        .call_decided(
            "POST",
            "/v1/withdrawals",
            Some(&party.token),
            &[("client-key", key.as_str())],
            Some(json!({ "payer": party.account, "asset": USD, "amount": amount.to_string() })),
        )
        .await;
    if !s.is_success() {
        return Err(format!("reserving a withdrawal: {s} {v}"));
    }
    let op = v["operation_reference"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/withdrawals/{op}/submit"),
            Some(&party.token),
            &[],
            Some(json!({ "destination": { "account": hex::encode(to) } })),
        )
        .await;
    if !s.is_success() {
        return Err(format!("submitting {op}: {s} {v}"));
    }
    Ok(op)
}

fn op_ref(op: &str) -> String {
    let id = OperationId::parse(op).unwrap_or_else(|e| panic!("operation {op}: {e:?}"));
    format!("0x{}", hex::encode(chain_ref(&id)))
}

/// Poll `done` every three seconds until it answers, or fail with `what` after `limit`.
async fn wait<F, Fut>(what: &str, limit: Duration, done: F) -> Result<String, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    wait_every(what, limit, Duration::from_secs(3), done).await
}

/// Poll `done` every `every` until it answers, or fail with `what` after `limit`.
async fn wait_every<F, Fut>(
    what: &str,
    limit: Duration,
    every: Duration,
    mut done: F,
) -> Result<String, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let start = Instant::now();
    loop {
        if let Some(note) = done().await {
            say(format!("{what}: {note} ({}s)", start.elapsed().as_secs()));
            return Ok(note);
        }
        if start.elapsed() > limit {
            return Err(format!("{what}: not reached within {}s", limit.as_secs()));
        }
        tokio::time::sleep(every).await;
    }
}

/// Milliseconds since the Unix epoch: the adapter's log (UTC, same host) and the chain clock
/// (`heads.jsonl`) are read against it (`NFR-15`).
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `op` reaches `want`.
async fn until_state(
    d: &Deployment,
    p: &Party,
    op: &str,
    want: &str,
    limit: Duration,
) -> Result<(), String> {
    wait(&format!("{op} {want}"), limit, || async move {
        let s = state(d, p, op).await;
        (s == want).then_some(s)
    })
    .await
    .map(|_| ())
}

// ------------------------------------------------------------------------------ e2e.mjs

/// `node e2e.mjs <args>`: its `RESULT` line, parsed.
pub(crate) async fn node(args: &[&str]) -> Result<Value, String> {
    let out = tokio::process::Command::new("node")
        .arg("e2e.mjs")
        .args(args)
        .current_dir(PathBuf::from(env("E2E_SCRIPTS")))
        .output()
        .await
        .map_err(|e| format!("node: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        return Err(format!(
            "node e2e.mjs {}: {}\n{}{}",
            args.join(" "),
            out.status,
            tail(&text, 20),
            tail(&String::from_utf8_lossy(&out.stderr), 20)
        ));
    }
    Ok(text
        .lines()
        .find_map(|l| l.strip_prefix("RESULT "))
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or(Value::Null))
}

pub(crate) fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Record a fault case in `report.json` (`e2e.mjs record-fault`).
async fn record_fault(name: &str, outcome: &Result<String, String>) {
    let (status, detail) = match outcome {
        Ok(d) => ("pass", d.clone()),
        Err(e) => ("fail", e.clone()),
    };
    if let Err(e) = node(&["record-fault", name, status, &detail]).await {
        say(format!("recording {name}: {e}"));
    }
}

fn push_expect(out: &Path, e: Value) {
    let path = out.join("expect.json");
    let mut all: Vec<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    all.push(e);
    std::fs::write(&path, serde_json::to_string_pretty(&all).expect("JSON"))
        .expect("expect.json is written");
}

// ------------------------------------------------------------------------------ run

/// Everything a case needs.
struct Ctx {
    d: Deployment,
    alice: Party,
    reference: String,
    contract: [u8; 32],
    depositor: String,
    out: PathBuf,
    h: Harness,
}

impl Ctx {
    /// A customer's deposit to alice's account reference: the block it landed in.
    async fn deposit(&self, amount: u64) -> Result<u64, String> {
        let r = node(&["deposit-to", &self.reference, &amount.to_string()]).await?;
        push_expect(
            &self.out,
            json!({ "kind": "Deposited", "accountRef": self.reference, "from": self.depositor,
            "asset": { "Here": 1 }, "amount": amount.to_string() }),
        );
        r["block"]
            .as_u64()
            .ok_or_else(|| format!("deposit-to answered {r}"))
    }

    fn expect_withdrawn(&self, op: &str, amount: u64) {
        push_expect(
            &self.out,
            json!({ "kind": "Withdrawn", "operationRef": op_ref(op),
            "to": format!("0x{}", hex::encode(PAYEE)), "asset": { "Here": 1 },
            "amount": amount.to_string() }),
        );
    }

    /// Wait until alice's available balance is `want`.
    async fn available(&self, want: u64, limit: Duration) -> Result<(), String> {
        let (d, a) = (&self.d, &self.alice);
        wait(&format!("alice holds {want}"), limit, || async move {
            let (available, _) = balances(d, a).await;
            (available == want).then(|| format!("alice holds {available} available"))
        })
        .await
        .map(|_| ())
    }

    async fn withdraw(&self, amount: u64) -> Result<String, String> {
        patient_withdrawal(&self.d, &self.alice, amount, PAYEE).await
    }

    async fn settled(&self, op: &str, limit: Duration) -> Result<(), String> {
        until_state(&self.d, &self.alice, op, "Settled", limit).await
    }

    /// [`Ctx::settled`], polled every 100 ms: scenario c measures the settlement time, and a
    /// poll every three seconds added up to three of them (`NFR-15`, run 37166637190).
    async fn settled_promptly(&self, op: &str, limit: Duration) -> Result<(), String> {
        let (d, p) = (&self.d, &self.alice);
        wait_every(
            &format!("{op} Settled"),
            limit,
            Duration::from_millis(100),
            || async move {
                let s = state(d, p, op).await;
                (s == "Settled").then_some(s)
            },
        )
        .await
        .map(|_| ())
    }
}

async fn run() {
    let out = PathBuf::from(env("E2E_OUT"));
    let dir = out.join("aledgent");
    std::fs::create_dir_all(&dir).expect("the state directory");
    for marker in [
        "step3.done",
        "step3.failed",
        "faults.go",
        "faults.done",
        "faults.failed",
    ] {
        let _ = std::fs::remove_file(dir.join(marker));
    }
    let setup: Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("aledgent-env.json"))
            .expect("aledgent-env.json: run `e2e.mjs aledgent-setup` first"),
    )
    .expect("aledgent-env.json is JSON");
    let contract = hex32(&setup["contract"], "contract");
    let genesis = setup["genesis"]
        .as_str()
        .expect("genesis")
        .trim_start_matches("0x")
        .to_owned();
    let spec = setup["specVersion"].as_u64().expect("specVersion") as u32;
    let depositor = setup["depositor"].as_str().expect("depositor").to_owned();

    // Ingress and the core, here; its surface over HTTP for the adapter's process. Heights of six
    // seconds, and a withdrawal valid for 80 of them (inside the operation's own deadline): an era
    // of 64 blocks fits in that.
    let (d, _) = ingress_with_adapter(INSTANCE, 80, 6_000).await;
    let port: u16 = std::env::var("INGRESS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(18_089);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("ingress on port {port}: {e}"));
    let router = d.router.clone();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            say(format!("ingress stopped: {e}"));
        }
    });
    say(format!("ingress serves the adapter on port {port}"));

    let h = match Harness::up(&dir, port, &genesis, spec, contract).await {
        Ok(h) => h,
        Err(e) => {
            say(format!("the adapter's harness did not come up: {e}"));
            std::fs::write(dir.join("step3.failed"), e).ok();
            return;
        }
    };
    let alice = d.party("alice", 0).await;
    let account = AccountId::parse(alice.account.as_deref().unwrap_or_default())
        .unwrap_or_else(|e| panic!("alice's account: {e:?}"));
    let reference = format!("0x{}", hex::encode(account_reference(&account)));
    let mut cx = Ctx {
        d,
        alice,
        reference,
        contract,
        depositor,
        out: out.clone(),
        h,
    };
    // expect.json is this driver's from here on.
    std::fs::write(out.join("expect.json"), "[]").expect("expect.json");

    let step3 = match step3(&mut cx).await {
        Ok(()) => book_only(&mut cx).await,
        Err(e) => Err(e),
    };
    match &step3 {
        Ok(()) => {
            say("step 3 done through aledgent");
            std::fs::write(dir.join("step3.done"), "").ok();
        }
        Err(e) => {
            say(format!("step 3 failed: {e}"));
            cx.h.save_logs().await;
            std::fs::write(dir.join("step3.failed"), e).ok();
        }
    }
    // Scenario c of the stress suite (F-032), when the workflow asks for it: the book under load
    // while withdrawals settle through the adapter. Then the fault cases, when the workflow
    // reaches step 4.
    let mut go = Err(String::new());
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3_600) {
        if dir.join("stress.go").exists() {
            // The phases are the marker's content (`aledgent.sh stress`).
            let phases = std::fs::read_to_string(dir.join("stress.go"))
                .unwrap_or_default()
                .trim()
                .to_owned();
            let _ = std::fs::remove_file(dir.join("stress.go"));
            match stress_with_settlement(&mut cx, &phases, &dir.join("stress")).await {
                Ok(note) => {
                    say(format!("stress: {note}"));
                    std::fs::write(dir.join("stress.done"), note).ok();
                }
                Err(e) => {
                    say(format!("stress failed: {e}"));
                    std::fs::write(dir.join("stress.failed"), e).ok();
                }
            }
        }
        if dir.join("faults.go").exists() {
            go = Ok(());
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if go.is_ok() {
        let ok = faults(&mut cx).await;
        cx.h.save_logs().await;
        std::fs::write(
            dir.join(if ok { "faults.done" } else { "faults.failed" }),
            "",
        )
        .ok();
    }
    cx.h.down().await;
}

/// Scenario c of the stress suite (`F-032`, `T-032-05`): the stress harness drives the book
/// through ingress's router, in this process (`phases`, as `stress --phases` takes them), while a
/// withdrawal is submitted every twenty seconds and settled through the adapter's own process on
/// the suite's Kreivo. Writes the harness's report into `out`, and the withdrawals' latencies
/// beside it; each withdrawal is also expected on the chain (`expect.json`).
async fn stress_with_settlement(cx: &mut Ctx, phases: &str, out: &Path) -> Result<String, String> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    cx.h.ensure(Variant::Normal).await?;
    let phases = stress::parse_phases(if phases.is_empty() {
        "c-1x:300:300,c-5x:1500:300"
    } else {
        phases
    })?;
    let done = Arc::new(AtomicBool::new(false));
    let load = {
        let done = Arc::clone(&done);
        let o = stress::RunOptions {
            target: stress::target::Target::Router(cx.d.router.clone()),
            token: cx.d.operator.token.clone(),
            operator_identity: cx.d.operator.identity.clone(),
            phases,
            ladder: false,
            setup: stress::world::SetupOptions {
                holders: 100,
                passkey_pct: 0,
                concurrency: 8,
                patience: Duration::from_secs(600),
                pollers: 1000,
                service_pct: 50,
            },
            pids: Vec::new(),
            max_inflight: 4096,
            drain: Duration::from_secs(600),
            seed: 20_261_003,
            out: out.to_path_buf(),
            label: "c: the book under load, withdrawals through the adapter (zombienet)".into(),
            events: None,
            delivered: Arc::new(AtomicU64::new(0)),
        };
        async move {
            let r = stress::run(o).await;
            done.store(true, Ordering::Relaxed);
            r
        }
    };
    let cxr = &*cx;
    let withdrawals = async {
        let mut latencies: Vec<Value> = Vec::new();
        // Let the population form first.
        tokio::time::sleep(Duration::from_secs(30)).await;
        while !done.load(Ordering::Relaxed) {
            let t = Instant::now();
            // Wall-clock marks (`NFR-15`): read against the adapter's log and the chain clock.
            let requested_at = epoch_ms();
            match cxr.withdraw(FAULT_WITHDRAW).await {
                Ok(op) => {
                    let submitted_at = epoch_ms();
                    match cxr.settled_promptly(&op, AFTER_FAULT).await {
                        Ok(()) => {
                            cxr.expect_withdrawn(&op, FAULT_WITHDRAW);
                            latencies.push(json!({
                                "operation": op,
                                "settled_ms": t.elapsed().as_millis() as u64,
                                "requested_at": requested_at,
                                "submitted_at": submitted_at,
                                "settled_at": epoch_ms(),
                            }));
                        }
                        Err(e) => latencies.push(json!({ "operation": op, "error": e })),
                    }
                }
                Err(e) => latencies.push(json!({ "error": e })),
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
        latencies
    };
    let (r, latencies) = tokio::join!(load, withdrawals);
    let settled: Vec<u64> = latencies
        .iter()
        .filter_map(|l| l["settled_ms"].as_u64())
        .collect();
    std::fs::create_dir_all(out).ok();
    std::fs::write(
        out.join("withdrawals.json"),
        serde_json::to_string_pretty(&json!({
            "withdrawals": latencies,
            "settled_ms": stress::record::percentiles(settled.clone()),
        }))
        .unwrap_or_default(),
    )
    .ok();
    r?;
    Ok(format!(
        "{} withdrawals settled under load of {} tried; settled_ms {}",
        settled.len(),
        latencies.len(),
        stress::record::percentiles(settled)
    ))
}

/// Step 3 through aledgent: a deposit with an account reference, credited at finality; a
/// withdrawal, settled; a withdrawal the contract refuses (paid to the contract itself), reported
/// with its reason and not released; the core's checkpoint, anchored and final.
async fn step3(cx: &mut Ctx) -> Result<(), String> {
    cx.h.start(Variant::Normal, None).await?;
    cx.h.serving(PATIENT).await?;

    // 1. A deposit with an account reference.
    cx.deposit(DEPOSIT).await?;
    cx.available(DEPOSIT, PATIENT).await?;

    // 2. A withdrawal, observed at finality and captured.
    let op = cx.withdraw(WITHDRAW).await?;
    cx.settled(&op, PATIENT).await?;
    let b = balances(&cx.d, &cx.alice).await;
    if b != (DEPOSIT - WITHDRAW, 0) {
        return Err(format!("after the capture alice holds {b:?}"));
    }
    cx.expect_withdrawn(&op, WITHDRAW);

    // 3. A withdrawal the contract refuses (paid to the contract itself), recorded with its
    // reason. Final, with its transaction's nonce consumed, it is a proved rejection: the
    // reservation is released at once (amendment 0006 item 3, `REQ-OP-9`).
    let refused = patient_withdrawal(&cx.d, &cx.alice, REFUSED, cx.contract).await?;
    let h = &cx.h;
    let r = refused.clone();
    let reported = wait("refusal observed", PATIENT, || {
        let r = r.clone();
        async move {
            h.reports("observe").await.into_iter().find_map(|body| {
                (body["kind"] == "withdraw_refused" && body["operation_reference"] == r.as_str())
                    .then(|| format!("reported: {}", body["payload"]))
            })
        }
    })
    .await?;
    let (d, alice, r) = (&cx.d, &cx.alice, &refused);
    wait("refusal released", PATIENT, || async move {
        let s = state(d, alice, r).await;
        (s == "Released").then(|| format!("the refused withdrawal is {s}"))
    })
    .await
    .map_err(|e| format!("{e} ({reported})"))?;
    let b = balances(&cx.d, &cx.alice).await;
    if b != (DEPOSIT - WITHDRAW, 0) {
        return Err(format!(
            "a proved rejection did not release: alice holds {b:?}"
        ));
    }
    push_expect(
        &cx.out,
        json!({ "kind": "Refused", "operationRef": op_ref(&refused), "reason": "SelfTransfer" }),
    );

    // 4. An anchor: the core's next checkpoint, anchored with the adapter's anchor key, final.
    cx.d.pass(ANCHOR_INTERVAL_MS, PATIENCE)
        .await
        .map_err(|why| format!("the core did not tick: {why}"))?;
    let d = &cx.d;
    wait("checkpoint final", PATIENT, || async move {
        d.core
            .with_ledger(|l| l.state().anchoring().latest_final().map(|c| c.number))
            .flatten()
            .map(|n| format!("checkpoint {n} is final"))
    })
    .await?;
    let checkpoint =
        cx.d.core
            .with_ledger(|l| l.state().anchoring().latest_final().cloned())
            .flatten()
            .ok_or("no final checkpoint")?;
    push_expect(
        &cx.out,
        json!({
            "kind": "Anchored",
            "operationRef": format!("0x{}", hex::encode(anchor_reference(INSTANCE, checkpoint.number))),
            "number": checkpoint.number,
            "commitment": format!("0x{}", hex::encode(checkpoint.commitment)),
        }),
    );
    Ok(())
}

/// `node e2e/papi/fund-contract.mjs <contract> <index> <amount>`: the issuer's plain transfer to
/// the contract's account, in polkadot-api. Its `RESULT` line, parsed.
async fn fund_contract(contract: &[u8; 32], index: u32, amount: u64) -> Result<Value, String> {
    let out = tokio::process::Command::new("node")
        .arg("fund-contract.mjs")
        .args([hex::encode(contract), index.to_string(), amount.to_string()])
        .current_dir(PathBuf::from(env("E2E_PAPI")))
        .output()
        .await
        .map_err(|e| format!("node: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        return Err(format!(
            "fund-contract.mjs: {}\n{}{}",
            out.status,
            tail(&text, 20),
            tail(&String::from_utf8_lossy(&out.stderr), 20)
        ));
    }
    text.lines()
        .find_map(|l| l.strip_prefix("RESULT "))
        .and_then(|j| serde_json::from_str(j).ok())
        .ok_or_else(|| format!("fund-contract.mjs printed no RESULT: {}", tail(&text, 5)))
}

/// A decided operator call; its reply, or why it failed.
async fn operator_call(
    d: &Deployment,
    path: &str,
    key: &str,
    body: Value,
) -> Result<Value, String> {
    let (s, v) = d
        .call_decided(
            "POST",
            path,
            Some(&d.operator.token),
            &[("client-key", key)],
            Some(body),
        )
        .await;
    if s.as_u16() == 200 {
        Ok(v)
    } else {
        Err(format!("POST {path}: {s} {v}"))
    }
}

/// `bind_asset` of the book-only asset at its location, under `key`.
async fn bind_book_only(d: &Deployment, key: &str) -> Result<Value, String> {
    operator_call(
        d,
        &format!("/v1/ops/assets/{BOOK_ONLY}/bind"),
        key,
        json!({ "location": format!("Here({BOOK_ONLY_INDEX})"), "justification": "e2e: bind BKO" }),
    )
    .await
}

/// The book-only asset's binding as the registry shows it: `book_only`, `binding` or `bound`.
async fn binding_of(d: &Deployment, asset: &str) -> String {
    let (_, v) = d
        .call("GET", "/v1/assets", Some(&d.operator.token), &[], None)
        .await;
    v.as_array()
        .and_then(|a| a.iter().find(|a| a["id"] == asset))
        .and_then(|a| a["binding"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Step 3b (F-029, amendment 0011, `US-F5`): a book-only asset registered, transacted, then bound
/// on the suite's Kreivo at zero divergence, and anchored.
///
/// 1. `BKO` is registered book-only at 2 decimals; the operator gets issuance authority over it,
///    issues to alice, and alice moves part of it to bob. A withdrawal of it is refused
///    (`AC-F5.1`).
/// 2. The issuer puts one minor unit less than the book's outstanding balance in the contract's
///    account (a plain transfer, in polkadot-api), and asks for the binding. While it is pending,
///    issuance is refused; the contract refuses it on chain, and once that is final `BKO` is
///    book-only again (`AC-F5.3`).
/// 3. The issuer puts the missing unit in and asks again: the contract records the binding at
///    zero divergence, and once final `BKO` is bound (`AC-F5.4`). Its name on Kreivo, `BKO/9`,
///    disagrees with its decimals field: only the field is read (`AC-F5.2`).
/// 4. The next checkpoint, covering the binding, is anchored and final; the settlement comparison
///    at its cut finds `BKO` at zero difference (`NFR-9`'s third axis).
async fn book_only(cx: &mut Ctx) -> Result<(), String> {
    let d = &cx.d;
    // 1. Registered book-only, issued, moved.
    let (s, v) = d
        .call(
            "POST",
            "/v1/ops/operator-grants",
            Some(&d.operator.token),
            &[],
            Some(json!({ "principal": d.operator.identity, "kinds": ["register_asset"], "justification": "e2e" })),
        )
        .await;
    if s.as_u16() != 200 {
        return Err(format!("operator grant: {s} {v}"));
    }
    operator_call(
        d,
        "/v1/ops/assets",
        "bko-register",
        json!({ "id": BOOK_ONLY, "precision": 2, "kind": "stable", "review": "b0".repeat(32),
                "justification": "e2e: BKO/2, book-only" }),
    )
    .await?;
    let (s, v) = d
        .call(
            "POST",
            "/v1/grants",
            Some(&d.operator.token),
            &[],
            Some(json!({
                "kind": "issuance", "principal": d.operator.identity,
                "subject": { "asset": BOOK_ONLY }, "ops": ["issue", "redeem"],
                // Room for scenario c's stress world, which issues BKO as its main asset (F-032):
                // 10^14 to its pool and treasury, and an issuing client's issuance under load.
                "per_window": { "length": 86_400_000u64, "limit": "1000000000000000000" },
                "assets": [BOOK_ONLY],
            })),
        )
        .await;
    if s.as_u16() != 200 {
        return Err(format!("issuance grant: {s} {v}"));
    }
    let alice = cx.alice.account.clone().unwrap_or_default();
    operator_call(
        d,
        "/v1/issuances",
        "bko-issue",
        json!({ "asset": BOOK_ONLY, "destination": alice, "amount": BOOK_ONLY_ISSUED.to_string() }),
    )
    .await?;
    let bob = d.party("bob", 0).await;
    let (s, v) = d
        .call_decided(
            "POST",
            "/v1/transfers",
            Some(&cx.alice.token),
            &[("client-key", "bko-move")],
            Some(
                json!({ "legs": [{ "from": alice, "to": bob.account, "asset": BOOK_ONLY,
                         "amount": BOOK_ONLY_MOVED.to_string() }] }),
            ),
        )
        .await;
    if s.as_u16() != 200 {
        return Err(format!("transfer: {s} {v}"));
    }
    let (s, v) = d
        .call_decided(
            "POST",
            "/v1/withdrawals",
            Some(&cx.alice.token),
            &[("client-key", "bko-withdraw-book-only")],
            Some(json!({ "payer": alice, "asset": BOOK_ONLY, "amount": "10" })),
        )
        .await;
    if v["error"] != "ERR-UnsupportedLeg" {
        return Err(format!(
            "a book-only withdrawal: {s} {v}, expected ERR-UnsupportedLeg"
        ));
    }
    say(format!(
        "{BOOK_ONLY}: registered book-only, {BOOK_ONLY_ISSUED} issued, {BOOK_ONLY_MOVED} moved; its withdrawal refused"
    ));

    // 2. One minor unit short: refused on chain, book-only again.
    let short = fund_contract(&cx.contract, BOOK_ONLY_INDEX, BOOK_ONLY_ISSUED - 1).await?;
    say(format!(
        "the issuer put {} in the contract: {short}",
        BOOK_ONLY_ISSUED - 1
    ));
    bind_book_only(&cx.d, "bko-bind-short").await?;
    let (s, v) = d
        .call_decided(
            "POST",
            "/v1/issuances",
            Some(&d.operator.token),
            &[("client-key", "bko-issue-while-binding")],
            Some(json!({ "asset": BOOK_ONLY, "destination": alice, "amount": "1" })),
        )
        .await;
    if v["error"] != "ERR-NotAuthorised" {
        return Err(format!(
            "issuance while binding: {s} {v}, expected ERR-NotAuthorised"
        ));
    }
    wait("the short binding refused", PATIENT, || async {
        (binding_of(&cx.d, BOOK_ONLY).await == "book_only").then(|| "book-only again".to_owned())
    })
    .await?;
    let refused =
        cx.d.core
            .with_ledger(|l| {
                l.state()
                    .settlement()
                    .incidents()
                    .iter()
                    .any(|i| matches!(i, ledger_core::settlement::Incident::BindRefused { .. }))
            })
            .unwrap_or(false);
    if !refused {
        return Err("the refused binding raised no incident".into());
    }

    // 3. At zero divergence: bound.
    fund_contract(&cx.contract, BOOK_ONLY_INDEX, 1).await?;
    let accepted = bind_book_only(&cx.d, "bko-bind").await?;
    say(format!("binding asked: {accepted}"));
    wait("the binding final", PATIENT, || async {
        (binding_of(&cx.d, BOOK_ONLY).await == "bound").then(|| "bound".to_owned())
    })
    .await?;
    let bound =
        cx.d.core
            .with_ledger(|l| {
                l.state()
                    .settlement()
                    .asset_bind(
                        &ledger_core::asset::AssetId::parse(BOOK_ONLY)
                            .unwrap_or_else(|_| unreachable!()),
                    )
                    .copied()
            })
            .flatten()
            .ok_or("no binding recorded")?;
    if bound.amount.get() != BOOK_ONLY_ISSUED {
        return Err(format!(
            "bound at {}, expected {BOOK_ONLY_ISSUED}",
            bound.amount.get()
        ));
    }
    push_expect(
        &cx.out,
        json!({ "kind": "Bound", "operationRef": format!("0x{}", hex::encode(chain_ref(&bound.reference))),
                "asset": { "Here": BOOK_ONLY_INDEX }, "amount": BOOK_ONLY_ISSUED.to_string() }),
    );
    say(format!(
        "{BOOK_ONLY} bound at zero divergence: {}",
        bound.amount.get()
    ));

    // 4. Anchored after the binding, and compared at zero difference.
    let before =
        cx.d.core
            .with_ledger(|l| l.state().anchoring().latest_final().map(|c| c.number))
            .flatten()
            .unwrap_or(0);
    cx.d.pass(ANCHOR_INTERVAL_MS, PATIENCE)
        .await
        .map_err(|why| format!("the core did not tick: {why}"))?;
    let d = &cx.d;
    wait(
        "a checkpoint after the binding final",
        PATIENT,
        || async move {
            d.core
                .with_ledger(|l| l.state().anchoring().latest_final().map(|c| c.number))
                .flatten()
                .filter(|n| *n > before)
                .map(|n| format!("checkpoint {n} is final"))
        },
    )
    .await?;
    let asset = ledger_core::asset::AssetId::parse(BOOK_ONLY).unwrap_or_else(|_| unreachable!());
    let compared = wait(
        "a comparison with BKO at zero difference",
        PATIENT,
        || async move {
            d.core
                .with_ledger(|l| {
                    l.state().anchoring().latest_comparison().and_then(|c| {
                        let row = c.rows.iter().find(|r| r.asset == asset)?;
                        (row.difference() == 0 && c.rows.iter().all(|r| r.difference() == 0))
                            .then(|| format!("cut {}: {:?}", c.cut, row))
                    })
                })
                .flatten()
        },
    )
    .await?;
    say(format!("zero divergence after the binding: {compared}"));
    let checkpoint = d
        .core
        .with_ledger(|l| l.state().anchoring().latest_final().cloned())
        .flatten()
        .ok_or("no final checkpoint")?;
    push_expect(
        &cx.out,
        json!({
            "kind": "Anchored",
            "operationRef": format!("0x{}", hex::encode(anchor_reference(INSTANCE, checkpoint.number))),
            "number": checkpoint.number,
            "commitment": format!("0x{}", hex::encode(checkpoint.commitment)),
        }),
    );
    Ok(())
}

// ------------------------------------------------------------------------------ faults

/// The fault cases (`T-027-06`), each through the adapter. Each checks that nothing is taken as
/// "not executed" without both proofs, that nothing is signed twice, and that the outcome is
/// right; `e2e.mjs check final` then checks the chain side. Returns whether all passed.
async fn faults(cx: &mut Ctx) -> bool {
    let mut ok = true;
    // Funds for the cases: one deposit up front.
    let (available, _) = balances(&cx.d, &cx.alice).await;
    if let Err(e) = async {
        cx.h.ensure(Variant::Normal).await?;
        cx.deposit(10 * FAULT_DEPOSIT).await?;
        cx.available(available + 10 * FAULT_DEPOSIT, AFTER_FAULT)
            .await
    }
    .await
    {
        say(format!("the fault cases could not be funded: {e}"));
        return false;
    }
    const CASES: [&str; 5] = [
        "drop the connection mid-submit",
        "kill the adapter between signing and submitting",
        "starve the light client of peers",
        "prune node state",
        "a deposit in the same gap as a withdrawal",
    ];
    for (i, name) in CASES.iter().enumerate() {
        let started = Instant::now();
        let run = async {
            match i {
                0 => drop_mid_submit(cx).await,
                1 => kill_between_signing_and_submitting(cx).await,
                2 => starve(cx).await,
                3 => prune(cx).await,
                _ => same_gap(cx).await,
            }
        };
        let outcome = match tokio::time::timeout(CASE, run).await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!(
                "not done within {}s; the adapter's log ends:\n{}",
                CASE.as_secs(),
                cx.h.log_tail(30).await
            )),
        };
        let outcome = outcome.map(|d| format!("{d} ({}s)", started.elapsed().as_secs()));
        say(format!("fault case '{name}': {outcome:?}"));
        ok &= outcome.is_ok();
        record_fault(name, &outcome).await;
        // The next case starts with every peer reachable, whatever this one left.
        cx.h.feed().await.ok();
    }
    ok
}

/// The adapter signs a batch and its connection drops while it submits: every peer is cut right
/// after the batch is stored, for half a minute. Recovery is from the chain alone: the same bytes
/// again once it is back, and exactly one execution.
async fn drop_mid_submit(cx: &mut Ctx) -> Result<String, String> {
    cx.h.ensure(Variant::Normal).await?;
    let signed_before = cx.h.count_all("batch signed and stored").await;
    let op = cx.withdraw(FAULT_WITHDRAW).await?;
    cx.h.wait_log("batch signed and stored", signed_before + 1, AFTER_FAULT)
        .await?;
    cx.h.starve().await?;
    let cut = Instant::now();
    tokio::time::sleep(Duration::from_secs(30)).await;
    let during = state(&cx.d, &cx.alice, &op).await;
    cx.h.feed().await?;
    cx.settled(&op, AFTER_FAULT).await?;
    let signed = cx.h.count_all("batch signed and stored").await - signed_before;
    if signed != 1 {
        return Err(format!("{signed} batches signed for one withdrawal"));
    }
    cx.expect_withdrawn(&op, FAULT_WITHDRAW);
    Ok(format!(
        "peers cut right after the batch was stored, for 30 s (the operation stayed {during}); \
         settled {}s after the cut, from one signed batch",
        cut.elapsed().as_secs()
    ))
}

/// The adapter's process dies right after it stores a signed batch and before it records the
/// signing with the core or broadcasts it. Both proofs of "not executed" hold, and the core
/// releases nothing; a new process over the same store broadcasts the same stored bytes (never a
/// second transaction), and the withdrawal settles once.
async fn kill_between_signing_and_submitting(cx: &mut Ctx) -> Result<String, String> {
    cx.h.stop().await;
    cx.h.start(Variant::Normal, Some("after-persist")).await?;
    cx.h.serving(AFTER_FAULT).await?;
    let op = cx.withdraw(FAULT_WITHDRAW).await?;
    let code = cx.h.wait_exit(AFTER_FAULT).await?;
    if !cx.h.log_contains("fault injection: dying here").await {
        return Err(format!(
            "the adapter exited ({code}) without reaching its fault"
        ));
    }
    let stored =
        cx.h.extrinsic_of(&op)
            .await
            .ok_or("no stored transaction for the withdrawal")?;
    // Both proofs hold: nothing left the process. The core releases nothing meanwhile.
    let lane = hex::encode(pass_lane(&TEST_LANE_SECRET, TEST_PASS_USER).account());
    let proofs = node(&[
        "nonexecution",
        &op_ref(&op),
        &format!("0x{lane}"),
        &stored.nonce.to_string(),
    ])
    .await?;
    if !proofs["entry"].is_null() || proofs["nonceConsumed"] != false {
        return Err(format!("the killed adapter's batch executed: {proofs}"));
    }
    let s = state(&cx.d, &cx.alice, &op).await;
    if s == "Released" || s == "Settled" {
        return Err(format!(
            "the core moved the withdrawal to {s} with nothing executed"
        ));
    }
    // A new process over the same store: it records the stored signing, then sends the same
    // bytes. Settled if they land within their era; if the era ends first, released on both
    // proofs. Never a second transaction either way.
    cx.h.start(Variant::Normal, None).await?;
    let (d, a, o) = (&cx.d, &cx.alice, op.clone());
    let end = wait(&format!("{op} settled or released"), AFTER_FAULT, || {
        let o = o.clone();
        async move {
            let s = state(d, a, &o).await;
            (s == "Settled" || s == "Released").then_some(s)
        }
    })
    .await?;
    if cx.h.count_current("batch signed and stored").await != 0 {
        return Err("the new process signed a batch: it should have sent the stored one".into());
    }
    let after =
        cx.h.extrinsic_of(&op)
            .await
            .ok_or("the stored transaction is gone")?;
    if after.hash != stored.hash {
        return Err(format!(
            "a second transaction: {} then {}",
            stored.hash, after.hash
        ));
    }
    let outcome = if end == "Settled" {
        cx.expect_withdrawn(&op, FAULT_WITHDRAW);
        "settled once from them".to_owned()
    } else {
        let p = node(&[
            "nonexecution",
            &op_ref(&op),
            &format!("0x{lane}"),
            &stored.nonce.to_string(),
        ])
        .await?;
        if !p["entry"].is_null() {
            return Err(format!("released with an inbox entry: {p}"));
        }
        format!(
            "their era ended first: released with no inbox entry (nonce at #{}: {})",
            p["finalized"], p["finalizedNonce"]
        )
    };
    Ok(format!(
        "died (exit {code}) with nonce {} stored and unsent; both proofs held at #{} and the core \
         released nothing; the next process sent the same bytes ({}) and signed nothing new: {outcome}",
        stored.nonce,
        proofs["finalized"],
        &stored.hash[..16]
    ))
}

/// The light client loses every peer while the chain goes on. Blind, the adapter decides nothing:
/// a withdrawal submitted meanwhile is neither settled nor released. With its peers back it
/// resyncs, signs, and the withdrawal settles once.
async fn starve(cx: &mut Ctx) -> Result<String, String> {
    cx.h.ensure(Variant::Normal).await?;
    let signed_before = cx.h.count_all("batch signed and stored").await;
    cx.h.starve().await?;
    let cut = Instant::now();
    // Blind, the adapter may already have told the core the layer is unavailable, and the core
    // then refuses the withdrawal: that is right too, and it is submitted again once fed.
    let submitted = withdrawal(&cx.d, &cx.alice, FAULT_WITHDRAW, PAYEE).await;
    tokio::time::sleep(Duration::from_secs(90)).await;
    let during = match &submitted {
        Ok(op) => {
            let s = state(&cx.d, &cx.alice, op).await;
            if s == "Settled" || s == "Released" {
                return Err(format!("starved, the withdrawal still reached {s}"));
            }
            format!("stayed {s}")
        }
        Err(e) => format!("was refused while the layer was unavailable ({e})"),
    };
    let signed_starved = cx.h.count_all("batch signed and stored").await - signed_before;
    let serving_before = cx.h.count_current("serving=true").await;
    cx.h.feed().await?;
    let fed = Instant::now();
    let op = match submitted {
        Ok(op) => op,
        Err(_) => {
            cx.h.wait_log("serving=true", serving_before + 1, AFTER_FAULT)
                .await?;
            cx.withdraw(FAULT_WITHDRAW).await?
        }
    };
    cx.settled(&op, AFTER_FAULT).await?;
    let signed = cx.h.count_all("batch signed and stored").await - signed_before;
    if signed != 1 {
        return Err(format!("{signed} batches signed for one withdrawal"));
    }
    cx.expect_withdrawn(&op, FAULT_WITHDRAW);
    Ok(format!(
        "no peers for {}s: the withdrawal {during} ({signed_starved} batch signed while \
         starved); peers back, settled {}s later from one signed batch",
        fed.duration_since(cut).as_secs(),
        fed.elapsed().as_secs()
    ))
}

/// The adapter reaches Kreivo through the pruned node alone (the collator's parachain port
/// blocked), which keeps 16 blocks of state. A deposit is made while the adapter is down, and the
/// pruned node no longer serves its block's events. The contract stores no entry (amendment 0040):
/// the entries are the events, so the pruned node can say the chain's head moved but not with
/// what. Without a history endpoint the adapter credits nothing and raises `unreadable` (`T-043-04`);
/// with the collator's RPC as its history endpoint it reads the block there, folds the entry to the
/// head, and credits it. A withdrawal then settles through the pruned node.
async fn prune(cx: &mut Ctx) -> Result<String, String> {
    cx.h.stop().await;
    let (before, _) = balances(&cx.d, &cx.alice).await;
    let block = cx.deposit(FAULT_DEPOSIT).await?;
    let events = node(&["pruned-events", &block.to_string()]).await?;
    if !events["eventsOnPruned"]
        .as_str()
        .is_some_and(|s| s.starts_with("unreadable"))
    {
        return Err(format!(
            "the pruned node still serves #{block}'s events: {events}"
        ));
    }
    // 1. No history endpoint: unreadable, and nothing credited.
    cx.h.start(Variant::PrunedOnly, None).await?;
    cx.h.wait_log("unreadable:", 1, AFTER_FAULT)
        .await
        .map_err(|e| format!("no history endpoint: {e}"))?;
    let (held, _) = balances(&cx.d, &cx.alice).await;
    if held != before {
        return Err(format!(
            "with no history endpoint the adapter credited {} it could not read",
            held.saturating_sub(before)
        ));
    }
    // 2. The collator's RPC as the history endpoint: caught up, credited.
    cx.h.start(Variant::PrunedWithHistory, None).await?;
    cx.available(before + FAULT_DEPOSIT, AFTER_FAULT).await?;
    let op = cx.withdraw(FAULT_WITHDRAW).await?;
    cx.settled(&op, AFTER_FAULT).await?;
    cx.expect_withdrawn(&op, FAULT_WITHDRAW);
    cx.h.stop().await;
    Ok(format!(
        "through kreivo-pruned only: the deposit at #{block} (its events {} there at #{}) was \
         unreadable with no history endpoint (nothing credited), and credited once the collator's \
         RPC was the history endpoint; a withdrawal settled",
        events["eventsOnPruned"]
            .as_str()
            .unwrap_or_default()
            .split(':')
            .next()
            .unwrap_or_default(),
        events["prunedFinalized"]
    ))
}

/// A customer's deposit lands right behind a withdrawal the adapter signed: one read from the
/// cursor reports both, and alice's balance moves by both.
async fn same_gap(cx: &mut Ctx) -> Result<String, String> {
    cx.h.ensure(Variant::Normal).await?;
    let (before, _) = balances(&cx.d, &cx.alice).await;
    let signed_before = cx.h.count_all("batch signed and stored").await;
    let op = cx.withdraw(FAULT_WITHDRAW).await?;
    cx.h.wait_log("batch signed and stored", signed_before + 1, AFTER_FAULT)
        .await?;
    let block = cx.deposit(FAULT_DEPOSIT).await?;
    cx.settled(&op, AFTER_FAULT).await?;
    cx.available(before - FAULT_WITHDRAW + FAULT_DEPOSIT, AFTER_FAULT)
        .await?;
    cx.expect_withdrawn(&op, FAULT_WITHDRAW);
    let entries: Vec<(u64, u64, String)> =
        cx.h.reports("observe")
            .await
            .into_iter()
            .filter(|b| b["observed_finality"] == "final")
            .filter_map(|b| {
                Some((
                    b["coordinate"]["seq"].as_u64()?,
                    b["at"].as_u64()?,
                    b["kind"].as_str()?.to_owned(),
                ))
            })
            .collect();
    let last: Vec<_> = {
        let mut e = entries;
        e.sort();
        e.into_iter().rev().take(2).collect()
    };
    Ok(format!(
        "deposit at #{block} behind the signed withdrawal; last two final entries {:?}; alice \
         {before} -> {}",
        last,
        before - FAULT_WITHDRAW + FAULT_DEPOSIT
    ))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
    match std::env::args().nth(1).as_deref() {
        Some("keys") => keys(),
        Some("run") => run().await,
        Some(cmd @ ("stress" | "stress-config")) => {
            let args: Vec<String> = std::env::args().collect();
            let r = if cmd == "stress" {
                stress::main(&args).await
            } else {
                stress::config_main(&args)
            };
            if let Err(e) = r {
                eprintln!("aledgent-e2e-driver {cmd}: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: aledgent-e2e-driver <keys|run|stress|stress-config>");
            std::process::exit(2);
        }
    }
}
