// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `aledgent-core-host shards`: the book in shards, on VOS (`F-031`, `DEC-22`, `T-031-20`).
//!
//! N book-shard Agents and one principals Agent, all Local Agents of one Space under the standard
//! runtime, each installed with the `aledgent-shard` actor (`agents/shard`) and driven through
//! the runtime ABI on its own thread, as separate Agents run. `shards::host::Cluster` is the
//! host: the replicas that check every reply, the relay that carries credits and the principals
//! feed in each shard's next round, the router that resolves authority and routes each command
//! to its payer's shard. This file is only the VOS port.
//!
//! The workload, for each shard count: `--accounts` accounts opened through the principals
//! Agent and funded; `--principals` agent principals, each granted on a slice of the payers;
//! then `--ops` transfers among the accounts, `--principal-pct` of them by a principal (stamped),
//! `--fee-pct` of them paying a fee collector held as a pool (`REQ-RT-12`), fed in a closed
//! loop of one batch per shard per step. Half way, principal 0 is revoked and granted again
//! (`REQ-AU-3`). Measured: decisions per second over the timed phase, the credit lag
//! (`NFR-19`), the revocation lag (`NFR-20`), the actor's gas, the Agents' state; checked: the
//! trial balance with credits in transit (`INV-5`), and the anchor over every shard root.
//!
//! `--runs` (`F-034`): per count, `verified` (the Agents decide, the PVM on the commit's critical
//! path), `native` (native decide, VOS verify: the host commits each round natively and a worker
//! replays it through the Agent; closed loop) and `native@R` (native decide at an offered rate of
//! R transfers a second; `v90` is 90 % of the count's verified rate). Native runs report the
//! replay's lag and backlog and whether it keeps up; they need the `native-decide` feature.
//!
//! Output: one CSV row per run in `<out.csv>` and a markdown table on stdout.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::Instant;

use shards::host::{Cluster, HostError, Port, PortStats, accounts, open_all};
use shards::pool::Pool;
use shards::principals::PrincipalsCommand;
use shards::provenance::Mode;
use shards::{Account, Command};
use vos::agent_sdk::{InvocationOrigin, MethodMode, PrincipalId};

use crate::driver::{self, message};
use crate::{package, port};

/// The principal the host drives every shard Agent as.
pub(crate) const DRIVER: [u8; 32] = [0xD5; 32];
/// The key ring's root of this throwaway book: a test value.
pub(crate) const ROOT: [u8; 32] = [0x5D; 32];
const USD: u32 = 840;
/// How many of a grown book's accounts one set-up step opens and funds.
const GROWN_SLICE: usize = 128;

enum Req {
    Invoke(Vec<u8>, Vec<Vec<u8>>),
    Read(Vec<u8>),
}

type Answer = Result<(Vec<u8>, PortStats), String>;

/// One Agent on its own thread: the thread owns its runtime; the port only holds a channel.
pub(crate) struct VosPort {
    tx: Sender<(Req, Sender<Answer>)>,
    stats: PortStats,
}

impl VosPort {
    pub(crate) fn spawn(
        runtime_pvm: Arc<Vec<u8>>,
        package: Arc<Vec<u8>>,
        discriminator: u8,
        core: Option<core_affinity::CoreId>,
    ) -> Result<Self, String> {
        let (tx, rx) = channel::<(Req, Sender<Answer>)>();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name(format!("agent-{discriminator}"))
            .spawn(move || {
                // One Agent per core where the host has the cores (`pinning`).
                if let Some(c) = core {
                    core_affinity::set_for_current(c);
                }
                let keys = driver::AgentKeys {
                    package: crate::keys::test_key(0x71),
                    authority: crate::keys::test_key(0x72),
                    discriminator,
                };
                let setup = (|| {
                    let mut rt = driver::Runtime::create(&runtime_pvm, &keys)
                        .map_err(|e| format!("create: {e}"))?;
                    let pkg = package::admit_actor(&package).map_err(|e| format!("admit: {e}"))?;
                    let actor = rt
                        .install(&pkg, "aledgent-shard", None)
                        .map_err(|e| format!("install: {e}"))?;
                    Ok::<_, String>((rt, actor))
                })();
                let (mut rt, actor) = match setup {
                    Ok(x) => {
                        let _ = ready_tx.send(Ok(()));
                        x
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let origin = InvocationOrigin {
                    principal: Some(PrincipalId(DRIVER)),
                    ..InvocationOrigin::anonymous()
                };
                let mut stats = PortStats::default();
                while let Ok((req, back)) = rx.recv() {
                    let (mode, msg, blobs) = match req {
                        Req::Invoke(envelope, blobs) => (
                            MethodMode::Linear,
                            message(vos::value::Msg::new("apply").with("envelope", envelope)),
                            blobs,
                        ),
                        Req::Read(q) => (
                            MethodMode::Query,
                            message(vos::value::Msg::new("read").with("query", q)),
                            Vec::new(),
                        ),
                    };
                    let id = rt.next_invocation(mode);
                    let w = rt.work(&actor, id, mode, origin, msg, &blobs);
                    let answer = rt.invoke(&w).map_err(|e| e.to_string()).and_then(|i| {
                        if matches!(mode, MethodMode::Linear) {
                            stats.invocations += 1;
                            stats.actor_gas += i.actor_gas;
                            stats.invoke_gas += i.invoke.gas_used;
                        }
                        stats.state_bytes = rt.state_bytes() as u64;
                        port::reply_bytes(&i.reply)
                            .map(|b| (b, stats))
                            .map_err(|e| format!("{e:?}"))
                    });
                    let _ = back.send(answer);
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx
            .recv()
            .map_err(|_| String::from("the Agent's thread ended"))??;
        Ok(VosPort {
            tx,
            stats: PortStats::default(),
        })
    }

    fn ask(&mut self, req: Req) -> Result<Vec<u8>, String> {
        let (back, rx) = channel();
        self.tx
            .send((req, back))
            .map_err(|_| String::from("the Agent's thread ended"))?;
        let (bytes, stats) = rx
            .recv()
            .map_err(|_| String::from("the Agent's thread ended"))??;
        self.stats = stats;
        Ok(bytes)
    }
}

impl Port for VosPort {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
        self.ask(Req::Invoke(message, blobs))
    }
    fn read(&mut self, query: Vec<u8>) -> Result<Vec<u8>, String> {
        self.ask(Req::Read(query))
    }
    fn blob_key(&self, bytes: &[u8]) -> core_binding::envelope::BlobKey {
        let r = vos::agent_sdk::BlobRef::of_bytes(bytes);
        core_binding::envelope::BlobKey {
            hash: r.hash.0,
            len: r.len,
        }
    }
    fn stats(&self) -> PortStats {
        self.stats
    }
}

pub(crate) fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.windows(2)
        .find(|w| w[0] == name)
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(default)
}

/// `--grown-max N`: how many of the chainspec's accounts the grown state takes.
pub(crate) fn args_max(args: &[String]) -> Option<u64> {
    args.windows(2)
        .find(|w| w[0] == "--grown-max")
        .and_then(|w| w[1].parse().ok())
}

pub(crate) fn arg_str<'a>(args: &'a [String], name: &str, default: &'a str) -> &'a str {
    args.windows(2)
        .find(|w| w[0] == name)
        .map_or(default, |w| w[1].as_str())
}

pub(crate) fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0x5A;
    o
}

/// A deterministic stream: splitmix64.
pub(crate) struct Mix(pub(crate) u64);

impl Mix {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// How one run decides and how it is loaded (`--runs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The Agents decide, the replicas check: the PVM on the commit's critical path.
    Verified,
    /// Native decide, VOS verify (`F-034`, feature `native-decide`), closed loop.
    Native,
    /// Native decide at an offered rate (transfers a second over every shard) for `--paced-secs`.
    Paced(u64),
}

impl Kind {
    fn label(self) -> String {
        match self {
            Kind::Verified => "verified".into(),
            Kind::Native => "native".into(),
            Kind::Paced(r) => format!("native@{r}"),
        }
    }
}

/// `--r03 off|report|enforce` (default `report`, `rq-01`).
fn r03_mode(args: &[String]) -> Result<Mode, HostError> {
    let s = arg_str(args, "--r03", Mode::Report.as_str());
    Mode::parse(s).ok_or_else(|| HostError::Refused(format!("--r03 {s}: off, report or enforce")))
}

/// One run's results.
struct Row {
    /// R-03's mode.
    r03: Mode,
    /// The Agents' actor gas over the timed phase, exact.
    actor_gas: u64,
    /// R-03: the receivers' checks, those that verified a certificate, those that verified a
    /// head, and the failures.
    r03_checks: u64,
    r03_certificates: u64,
    r03_verified: u64,
    r03_failures: u64,
    shards: u16,
    kind: Kind,
    ops: u64,
    decided: u64,
    accepted: u64,
    cross: u64,
    /// From the load's start to its last decision.
    decide_us: u64,
    /// Commands the replay confirmed per second, from the load's start to the last confirmation.
    confirmed_per_s: u64,
    ack: shards::host::Lags,
    replay: shards::host::Lags,
    backlog_max: u64,
    /// From the last decision to the last confirmation, ms; `None` when the replay was cut off.
    drain_ms: Option<u64>,
    steps: u64,
    credit: shards::host::Lags,
    revocation: shards::host::Lags,
    round_p50_us: u64,
    round_p99_us: u64,
    gas_per_round_m: u64,
    state_max: u64,
    balanced: bool,
    exactly_once: Option<bool>,
    anchor: [u8; 32],
}

impl Row {
    fn rate(&self) -> u64 {
        (u128::from(self.decided) * 1_000_000 / u128::from(self.decide_us.max(1))) as u64
    }
    /// The replay keeps up when it confirmed everything within 5 s of the last decision and its
    /// lag stayed under 5 s at p99. Not asked of a verified run.
    fn keeps_up(&self) -> Option<bool> {
        match self.kind {
            Kind::Verified => None,
            _ => Some(self.drain_ms.is_some_and(|d| d <= 5_000) && self.replay.p99_us <= 5_000_000),
        }
    }
}

/// The cores the principals Agent and `n` shard Agents are pinned to, one each, if the host has
/// that many and `SHARDS_PIN` is not `0`. The host's own threads (the replicas, the relay) are
/// left to the scheduler.
fn pinning(n: u16) -> Option<Vec<core_affinity::CoreId>> {
    if std::env::var("SHARDS_PIN").is_ok_and(|v| v == "0") {
        return None;
    }
    let cores = core_affinity::get_core_ids()?;
    (cores.len() > usize::from(n)).then_some(cores)
}

pub(crate) fn rank(v: &mut [u64], p: usize) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() * p).div_ceil(100)).max(1) - 1]
}

fn lags_of(mut v: Vec<u64>) -> shards::host::Lags {
    if v.is_empty() {
        return shards::host::Lags::default();
    }
    shards::host::Lags {
        n: v.len() as u64,
        p50_us: rank(&mut v, 50),
        p99_us: rank(&mut v, 99),
        max_us: *v.iter().max().unwrap_or(&0),
    }
}

#[cfg(feature = "native-decide")]
fn go_native(c: &mut Cluster<VosPort>, batch: usize) -> Result<(), HostError> {
    c.enable_native(batch)
}

#[cfg(not(feature = "native-decide"))]
fn go_native(_: &mut Cluster<VosPort>, _: usize) -> Result<(), HostError> {
    Err(HostError::Refused(
        "native decide needs the core host's `native-decide` feature (F-034)".into(),
    ))
}

#[cfg(feature = "native-decide")]
fn cut_off(c: &mut Cluster<VosPort>) {
    c.abort_replays();
}

#[cfg(not(feature = "native-decide"))]
fn cut_off(_: &mut Cluster<VosPort>) {}

fn one(
    runtime: &Arc<Vec<u8>>,
    package: &Arc<Vec<u8>>,
    n: u16,
    kind: Kind,
    args: &[String],
) -> Result<Row, HostError> {
    let n_accounts = arg(args, "--accounts", 256) as u32;
    let per_shard = arg(args, "--ops-per-shard", 0);
    let ops = if per_shard > 0 {
        per_shard * u64::from(n)
    } else {
        arg(args, "--ops", 2_000)
    };
    let batch = arg(args, "--batch", 64) as usize;
    let replay_batch = arg(args, "--replay-batch", 2) as usize;
    let round_wait = std::time::Duration::from_millis(arg(args, "--round-wait-ms", 0));
    let paced_secs = arg(args, "--paced-secs", 20);
    let drain_cap = std::time::Duration::from_secs(arg(args, "--drain-cap-secs", 60));
    let n_principals = arg(args, "--principals", 8).max(1);
    let principal_pct = arg(args, "--principal-pct", 50);
    let fee_pct = arg(args, "--fee-pct", 10);
    let cores = pinning(n);
    let core = |i: usize| cores.as_ref().map(|c| c[i]);
    let spawn = |d: u8, c: Option<core_affinity::CoreId>| {
        VosPort::spawn(Arc::clone(runtime), Arc::clone(package), d, c).map_err(HostError::Port)
    };
    let mut ports = Vec::new();
    for i in 0..n {
        ports.push(spawn(0x50 + i as u8, core(usize::from(i) + 1))?);
    }
    let principals_port = spawn(0x4F, core(0))?;
    eprintln!(
        "{n} shards, {}: Agents {}",
        kind.label(),
        if cores.is_some() {
            "pinned one per core (principals on core 0, shard i on core i + 1)"
        } else {
            "not pinned"
        }
    );
    let t = Instant::now();
    let mut c = Cluster::found(ports, principals_port, ROOT, [0x77; 32])?;
    // R-03 (`rq-01`): `report` unless `--r03` says otherwise; commits are certified as made under
    // this runtime, by the bench's test voters (a deployment's are the signer's, `rq-02`).
    let r03 = r03_mode(args)?;
    c.set_r03(
        r03,
        shards::attest::program_of(runtime),
        shards::attest::test_voters(&[0x77; 32], usize::from(n)),
    )?;
    c.batch = batch;
    c.parallel = true;

    // Set-up, through the Agents: accounts, the fee pool, grants, funds.
    let accts = accounts(n_accounts);
    let dir = c.directory().clone();
    let fees = Pool::new([0xFE; 16], &dir);
    let mut opens: Vec<(Account, u32)> = accts.iter().map(|a| (*a, USD)).collect();
    opens.extend(fees.members.iter().map(|m| (*m, USD)));
    // The grown state (`--chainspec`): the chainspec's ledger accounts, routed by the router's
    // own rule, opened and funded before the timed phases (`crate::chainspec::ballast`).
    let grown = match arg_str(args, "--chainspec", "") {
        "" => Vec::new(),
        path => crate::chainspec::ballast(path, args_max(args)).map_err(HostError::Port)?,
    };
    open_all(&mut c, &opens)?;
    // In slices: one principals invocation carries a bounded feed.
    for (k, slice) in grown.chunks(GROWN_SLICE).enumerate() {
        let opens: Vec<(Account, u32)> = slice.iter().map(|(a, _)| (*a, USD)).collect();
        open_all(&mut c, &opens)?;
        for (j, (a, amount)) in slice.iter().enumerate() {
            if *amount > 0 {
                c.operator(Command::Issue {
                    op: op(1 << 41 | (k * GROWN_SLICE + j) as u64),
                    account: *a,
                    asset: USD,
                    amount: *amount,
                });
            }
        }
        c.drain()?;
    }
    if !grown.is_empty() {
        let mut per: BTreeMap<u16, u64> = BTreeMap::new();
        for (a, _) in &grown {
            *per.entry(dir.shard_of(a).0).or_default() += 1;
        }
        eprintln!(
            "{n} shards: grown state of {} accounts from the chainspec, per shard {:?}",
            grown.len(),
            per.values().collect::<Vec<_>>()
        );
    }
    c.router.add_pool(fees.clone());
    let agents: Vec<[u8; 16]> = (0..n_principals)
        .map(|i| {
            let mut p = [0xA0u8; 16];
            p[8..].copy_from_slice(&i.to_be_bytes());
            p
        })
        .collect();
    for (i, a) in accts.iter().enumerate() {
        c.principal_command(PrincipalsCommand::Grant {
            principal: agents[i % agents.len()],
            payer: *a,
            limit: 1_000,
        });
        c.operator(Command::Issue {
            op: op(1 << 40 | i as u64),
            account: *a,
            asset: USD,
            amount: 1_000_000_000,
        });
    }
    c.drain()?;
    if kind != Kind::Verified {
        go_native(&mut c, replay_batch)?;
        c.round_wait = round_wait;
    }
    eprintln!("{n} shards: set up in {} ms", t.elapsed().as_millis());
    let gas_before: u64 = c.port_stats().iter().map(|s| s.actor_gas).sum();
    let r03_before = c.r03().report.clone();
    let rounds_before = c.metrics.rounds;
    let steps_before = c.metrics.steps;
    let decided_before = c.metrics.decided;
    let accepted_before = c.metrics.accepted;
    let cross_before = c.metrics.cross;
    let confirmed_before = c.metrics.confirmed;
    c.metrics.credit_lag_us.clear();
    c.metrics.step_us.clear();
    c.metrics.replay_lag_us.clear();
    c.metrics.backlog_max = 0;

    // The load: a closed loop of one batch per shard per step, or an offered rate.
    let target = match kind {
        Kind::Paced(r) => r * paced_secs,
        _ => ops,
    };
    let window_us = paced_secs * 1_000_000;
    let mut mix = Mix(0xC0FF_EE00 + u64::from(n));
    let mut sent = 0u64;
    let per_step = batch as u64 * u64::from(n);
    let mut submitted: BTreeMap<u64, Instant> = BTreeMap::new();
    let mut seen = c.decided.len();
    let mut ack = Vec::new();
    let start = Instant::now();
    let mut revoked = false;
    let mut decided_at: Option<Instant> = None;
    let mut cut = false;
    loop {
        let allowance = match kind {
            Kind::Paced(r) => {
                let el = (start.elapsed().as_micros() as u64).min(window_us);
                (u128::from(r) * u128::from(el) / 1_000_000) as u64
            }
            _ => sent + per_step,
        }
        .min(target)
        .saturating_sub(sent);
        let mut k = 0;
        while k < allowance {
            let payer_i = (mix.next() % u64::from(n_accounts)) as usize;
            let payer = accts[payer_i];
            let payee = if mix.next() % 100 < fee_pct {
                fees.id
            } else {
                accts[(mix.next() % u64::from(n_accounts)) as usize]
            };
            let by = (mix.next() % 100 < principal_pct).then(|| agents[payer_i % agents.len()]);
            let amount = 1 + mix.next() % 500;
            // A refusal at ingress (a revoked principal) is decided there; count it as decided.
            match c.transfer(by, payer, payee, USD, amount) {
                Ok((ticket, _)) => {
                    submitted.insert(ticket, Instant::now());
                }
                Err(_) => c.metrics.decided += 1,
            }
            sent += 1;
            k += 1;
        }
        if !revoked && sent >= target / 2 {
            c.principal_command(PrincipalsCommand::Revoke {
                principal: agents[0],
            });
            for a in accts.iter().step_by(agents.len()) {
                c.principal_command(PrincipalsCommand::Grant {
                    principal: agents[0],
                    payer: *a,
                    limit: 1_000,
                });
            }
            revoked = true;
        }
        let ran = c.step()?;
        let now = Instant::now();
        for ticket in &c.decided[seen..] {
            if let Some(t0) = submitted.remove(ticket) {
                ack.push(now.duration_since(t0).as_micros() as u64);
            }
        }
        seen = c.decided.len();
        if decided_at.is_none() && sent >= target && c.queued() == 0 {
            decided_at = Some(now);
        }
        if decided_at.is_some() && !c.busy() {
            break;
        }
        if let Some(d) = decided_at {
            if now.duration_since(d) > drain_cap {
                cut = true;
                break;
            }
        }
        if ran == 0 && matches!(kind, Kind::Paced(_)) && sent < target {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    let decided_at = decided_at.unwrap_or_else(Instant::now);
    let decide_us = decided_at.duration_since(start).as_micros() as u64;
    let confirmed = c.metrics.confirmed - confirmed_before;
    let last = c.metrics.last_confirmed.unwrap_or(decided_at);
    let (confirmed_per_s, drain_ms) = if kind == Kind::Verified {
        let d = c.metrics.decided - decided_before;
        (
            (u128::from(d) * 1_000_000 / u128::from(decide_us.max(1))) as u64,
            Some(0),
        )
    } else {
        let span = last.saturating_duration_since(start).as_micros().max(1);
        (
            (u128::from(confirmed) * 1_000_000 / span) as u64,
            (!cut).then(|| last.saturating_duration_since(decided_at).as_millis() as u64),
        )
    };
    let backlog_max = c.metrics.backlog_max;
    let replay = c.metrics.replay_lag();
    if cut {
        eprintln!(
            "{n} shards, {}: the replay was {} rounds behind {} s after the last decision; cut off",
            kind.label(),
            c.unconfirmed(),
            drain_cap.as_secs()
        );
        cut_off(&mut c);
    }
    let stats = c.port_stats();
    let gas: u64 = stats.iter().map(|s| s.actor_gas).sum::<u64>() - gas_before;
    let rounds = (c.metrics.rounds - rounds_before).max(1);
    let balanced = c.trial_balance(USD).holds();
    let exactly_once = if cut {
        None
    } else {
        let wrong = c.verify_exactly_once()?;
        if !wrong.is_empty() {
            eprintln!("{n} shards: credits not applied exactly once between {wrong:?}");
        }
        Some(wrong.is_empty())
    };
    let (anchor, _) = c.anchor();
    let mut step_us = c.metrics.step_us.clone();
    let rep = &c.r03().report;
    Ok(Row {
        r03,
        actor_gas: gas,
        r03_checks: rep.checks - r03_before.checks,
        r03_certificates: rep.certificates() - r03_before.certificates(),
        r03_verified: rep.verified - r03_before.verified,
        r03_failures: rep.failures - r03_before.failures,
        shards: n,
        kind,
        ops: sent,
        decided: c.metrics.decided - decided_before,
        accepted: c.metrics.accepted - accepted_before,
        cross: c.metrics.cross - cross_before,
        decide_us,
        confirmed_per_s,
        ack: lags_of(ack),
        replay,
        backlog_max,
        drain_ms,
        steps: c.metrics.steps - steps_before,
        credit: c.metrics.credit_lag(),
        revocation: c.metrics.revocation_lag(),
        round_p50_us: rank(&mut step_us, 50),
        round_p99_us: rank(&mut step_us, 99),
        gas_per_round_m: gas / rounds / 1_000_000,
        state_max: stats.iter().map(|s| s.state_bytes).max().unwrap_or(0),
        balanced,
        exactly_once,
        anchor,
    })
}

/// `aledgent-core-host shards <runtime.pvm> <shard.vos> <out.csv> [--counts 1,2,4,8]
/// [--runs verified,native,native@1500,native@v90] ...`
///
/// `--runs`: per shard count, in order: `verified` (the PVM on the critical path), `native`
/// (native decide, closed loop) and `native@R` (native decide at R transfers a second offered
/// for `--paced-secs`; `v90` is 90 % of this count's verified rate, measured before it).
/// `--r03 off|report|enforce` (default `report`): R-03's mode for every shard (`rq-01`); the
/// table and the CSV give the exact actor gas and the checks, so that two runs differing only in
/// it measure the checks' gas; `--r03-gas 1,16,64` then measures the check alone, the same deliveries
/// to a receiver pinned `off` and one pinned `report` (`r03gas.rs`). `--ops-per-shard` sizes the closed loops by the shard count; `--replay-batch` caps the rounds
/// a replay invocation carries; `--round-wait-ms` lets a native shard's round fill up to a batch
/// for that long; `--drain-cap-secs` how long the replay may run after the load.
pub fn shards(args: &[String]) {
    let runtime = Arc::new(std::fs::read(&args[2]).expect("the runtime"));
    let package = Arc::new(std::fs::read(&args[3]).expect("the shard package"));
    let out = &args[4];
    let counts: Vec<u16> = arg_str(args, "--counts", "1,2,4,8")
        .split(',')
        .map(|s| s.trim().parse().expect("a shard count"))
        .collect();
    let runs: Vec<String> = arg_str(args, "--runs", "verified")
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let mut csv = String::from(
        "shards,run,ops,decided,accepted,cross,decide_micros,decided_per_s,confirmed_per_s,ack_p50_us,ack_p99_us,replay_lag_p50_us,replay_lag_p99_us,replay_lag_max_us,backlog_max_rounds,drain_ms,keeps_up,steps,round_p50_us,round_p99_us,credit_lag_p50_us,credit_lag_p99_us,credit_lag_max_us,revocation_lag_us,actor_gas_per_round_m,agent_state_max_bytes,trial_balance,exactly_once,anchor,r03,actor_gas,r03_checks,r03_certificates,r03_verified,r03_failures\n",
    );
    let cpus = core_affinity::get_core_ids().map_or(0, |c| c.len());
    println!(
        "Host CPUs: {cpus}; Agents pinned one per core where the host has a core per Agent{}.",
        if std::env::var("SHARDS_PIN").is_ok_and(|v| v == "0") {
            " (disabled: SHARDS_PIN=0)"
        } else {
            ""
        }
    );
    println!();
    println!(
        "| Shard Agents | Run | Transfers | Decided/s | Confirmed/s | Ack p50 / p99 ms | Replay lag p50 / p99 / max ms | Backlog max (rounds) | Drain after load ms | Replay keeps up | Credit lag p99 ms | Revocation lag ms | Step p50 / p99 ms | Actor gas per round (M) | Trial balance | Exactly once | R-03 | Actor gas (exact) | R-03 checks / certificates / verified / failures |"
    );
    println!(
        "|---:|---|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|---|---|---|---:|---:|"
    );
    let mut failed = false;
    for n in counts {
        let mut verified_rate = 0u64;
        for spec in &runs {
            let kind = match spec.as_str() {
                "verified" => Kind::Verified,
                "native" => Kind::Native,
                s if s.starts_with("native@v") => {
                    let pct: u64 = s["native@v".len()..].parse().expect("a percentage");
                    Kind::Paced((verified_rate * pct / 100).max(1))
                }
                s if s.starts_with("native@") => {
                    Kind::Paced(s["native@".len()..].parse().expect("a rate"))
                }
                other => panic!("unknown run {other}"),
            };
            match one(&runtime, &package, n, kind, args) {
                Ok(r) => {
                    let rate = r.rate();
                    if kind == Kind::Verified {
                        verified_rate = rate;
                    }
                    let keeps = r.keeps_up();
                    let yn = |b: Option<bool>| match b {
                        None => "",
                        Some(true) => "yes",
                        Some(false) => "no",
                    };
                    let drain = r.drain_ms.map_or("cut".to_string(), |d| d.to_string());
                    let _ = writeln!(
                        csv,
                        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                        r.shards,
                        kind.label(),
                        r.ops,
                        r.decided,
                        r.accepted,
                        r.cross,
                        r.decide_us,
                        rate,
                        r.confirmed_per_s,
                        r.ack.p50_us,
                        r.ack.p99_us,
                        r.replay.p50_us,
                        r.replay.p99_us,
                        r.replay.max_us,
                        r.backlog_max,
                        drain,
                        yn(keeps),
                        r.steps,
                        r.round_p50_us,
                        r.round_p99_us,
                        r.credit.p50_us,
                        r.credit.p99_us,
                        r.credit.max_us,
                        r.revocation.max_us,
                        r.gas_per_round_m,
                        r.state_max,
                        if r.balanced { "holds" } else { "BROKEN" },
                        match r.exactly_once {
                            None => "unchecked",
                            Some(true) => "yes",
                            Some(false) => "NO",
                        },
                        r.anchor
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                        r.r03.as_str(),
                        r.actor_gas,
                        r.r03_checks,
                        r.r03_certificates,
                        r.r03_verified,
                        r.r03_failures,
                    );
                    println!(
                        "| {} | {} | {} | **{}** | {} | {} / {} | {} / {} / {} | {} | {} | {} | {} | {} | {} / {} | {} | {} | {} | {} | {} | {} / {} / {} / {} |",
                        r.shards,
                        kind.label(),
                        r.ops,
                        rate,
                        r.confirmed_per_s,
                        r.ack.p50_us / 1_000,
                        r.ack.p99_us / 1_000,
                        r.replay.p50_us / 1_000,
                        r.replay.p99_us / 1_000,
                        r.replay.max_us / 1_000,
                        r.backlog_max,
                        drain,
                        yn(keeps),
                        r.credit.p99_us / 1_000,
                        r.revocation.max_us / 1_000,
                        r.round_p50_us / 1_000,
                        r.round_p99_us / 1_000,
                        r.gas_per_round_m,
                        if r.balanced { "holds" } else { "**broken**" },
                        match r.exactly_once {
                            None => "unchecked (cut off)",
                            Some(true) => "yes",
                            Some(false) => "**no**",
                        },
                        r.r03.as_str(),
                        r.actor_gas,
                        r.r03_checks,
                        r.r03_certificates,
                        r.r03_verified,
                        r.r03_failures,
                    );
                    failed |= !r.balanced || r.exactly_once == Some(false);
                }
                Err(e) => {
                    println!(
                        "| {n} | {} | failed: {e} | | | | | | | | | | | | | | | | |",
                        kind.label()
                    );
                    eprintln!("{n} shards, {}: {e}", kind.label());
                    failed = true;
                }
            }
        }
    }
    std::fs::write(out, csv).expect("write the CSV");
    // `--r03-gas 1,16,64`: R-03's check measured exactly, receiver against receiver (`r03gas.rs`).
    if args.iter().any(|a| a == "--r03-gas") {
        let sizes: Vec<usize> = arg_str(args, "--r03-gas", "1,16,64")
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        let sizes = if sizes.is_empty() {
            vec![1, 16, 64]
        } else {
            sizes
        };
        failed |= !crate::r03gas::probe(&runtime, &package, &sizes);
    }
    if failed {
        std::process::exit(1);
    }
}
