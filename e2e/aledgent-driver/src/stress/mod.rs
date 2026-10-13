// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The stress harness (`F-032`): a realistic mix of commands at a planned rate against ingress,
//! and the numbers `SPEC.md` §11 bounds — the acknowledgement (`NFR-1`), the decided reply
//! (`NFR-16`), completion (`NFR-15`) — with error rates, the backlog and the processes' memory.
//!
//! ```text
//! aledgent-e2e-driver stress-config --out <dir> --database-url <url> [--listen <addr>]
//!     [--core-endpoint unix:<path>] [--anchoring-ms <ms>] [--journal <file>]
//! aledgent-e2e-driver stress --url <http://host:port> --config-dir <dir> --plan <plan> --out <dir>
//!     [--phases name:cmds_per_min:secs,...] [--holders N] [--passkey-pct P] [--pid label=PID]...
//!     [--max-inflight N] [--drain-secs S] [--seed N]
//! ```
//!
//! **Rates are commands per minute**, the unit the targets are set in (`features/F-032-stress/
//! plan.md` §2): the reference hour is 18 000 commands (300 a minute), the reference minute
//! 900. A flow sends one to three commands; the scheduler starts flows at the rate that
//! gives the planned commands, open loop: it does not wait for replies, so a slow core shows up
//! as latency and backlog, not as a slower offered load. Reads and passkey sign-ins ride on top.
//!
//! **Plans** (a multiple of the reference hour's 300 commands a minute unless said):
//! `smoke` (½× for 30 s); `ladder` (½×, 1×, 2×, 5×, 10×, two minutes each, stopping early once a
//! step's backlog keeps growing); `sustained` (10× for ten minutes); `burst` (1× for a minute,
//! 50× the reference minute — 45 000 a minute — for one minute, 1× for two); `soak` (2× for four
//! hours). `--phases` replaces the plan's.
//!
//! **After the load** it waits for the backlog to drain, then checks that no acknowledged
//! operation was lost (each one read back decided, `NFR-4`) and reads the trial balance (`INV-5`).
//! It writes `report.json` and `summary.md` into `--out`.

pub mod config;
pub mod record;
pub mod target;
pub mod world;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::Semaphore;

use record::{Recorder, Scrape, parse_metrics, summarise, verdict};
use target::Target;
use world::{Rng, SetupOptions, World, commands_per_flow_milli, draw};

/// The reference hour, in commands a minute: the load the plans are multiples of.
pub const PEAK_HOUR_PER_MIN: u64 = 300;
/// The reference minute: the burst plans' base.
pub const PEAK_MINUTE: u64 = 900;

/// One phase: a name, commands a minute, seconds.
#[derive(Clone, Debug)]
pub struct Phase {
    /// Its name.
    pub name: String,
    /// Commands a minute.
    pub per_min: u64,
    /// How long.
    pub secs: u64,
}

fn phase(name: &str, per_min: u64, secs: u64) -> Phase {
    Phase {
        name: name.into(),
        per_min,
        secs,
    }
}

/// A named plan's phases.
pub fn plan(name: &str) -> Result<Vec<Phase>, String> {
    let p = PEAK_HOUR_PER_MIN;
    Ok(match name {
        "smoke" => vec![phase("smoke-0.5x", p / 2, 30)],
        "ladder" => vec![
            phase("ladder-0.5x", p / 2, 120),
            phase("ladder-1x", p, 120),
            phase("ladder-2x", 2 * p, 120),
            phase("ladder-5x", 5 * p, 120),
            phase("ladder-10x", 10 * p, 120),
        ],
        "sustained" => vec![phase("sustained-10x", 10 * p, 600)],
        "burst" => vec![
            phase("burst-before-1x", p, 60),
            phase("burst-50x-minute", 50 * PEAK_MINUTE, 60),
            phase("burst-after-1x", p, 120),
        ],
        "soak" => vec![phase("soak-2x", 2 * p, 4 * 3600)],
        other => return Err(format!("unknown plan {other}")),
    })
}

/// `name:per_min:secs,...`.
pub fn parse_phases(s: &str) -> Result<Vec<Phase>, String> {
    s.split(',')
        .filter(|x| !x.trim().is_empty())
        .map(|x| {
            let parts: Vec<&str> = x.trim().split(':').collect();
            match parts.as_slice() {
                [n, r, d] => Ok(phase(
                    n,
                    r.parse().map_err(|_| format!("{x}: rate"))?,
                    d.parse().map_err(|_| format!("{x}: seconds"))?,
                )),
                _ => Err(format!("{x}: name:per_min:secs")),
            }
        })
        .collect()
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn args_all(args: &[String], name: &str) -> Vec<String> {
    args.windows(2)
        .filter(|w| w[0] == name)
        .map(|w| w[1].clone())
        .collect()
}

fn num(args: &[String], name: &str, default: u64) -> Result<u64, String> {
    match arg(args, name) {
        Some(v) => v.parse().map_err(|_| format!("{name}: a number")),
        None => Ok(default),
    }
}

/// `stress-config`.
pub fn config_main(args: &[String]) -> Result<(), String> {
    let out = PathBuf::from(arg(args, "--out").ok_or("--out")?);
    let url = arg(args, "--database-url").ok_or("--database-url")?;
    let listen = arg(args, "--listen").unwrap_or_else(|| "127.0.0.1:8080".into());
    let journal = arg(args, "--journal")
        .unwrap_or_else(|| out.join("core.journal").to_string_lossy().into_owned());
    let endpoint = arg(args, "--core-endpoint");
    let anchoring = match arg(args, "--anchoring-ms") {
        Some(v) => Some(v.parse().map_err(|_| "--anchoring-ms: ms")?),
        None => None,
    };
    config::write(&config::Options {
        out: &out,
        database_url: &url,
        listen: &listen,
        core_endpoint: endpoint.as_deref(),
        anchoring_ms: anchoring,
        journal: &journal,
    })
}

/// Resident memory of `pid`, KiB (`ps`, on Linux and macOS).
async fn rss_kib(pid: &str) -> Option<u64> {
    let out = tokio::process::Command::new("ps")
        .args(["-o", "rss=", "-p", pid])
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Scrape ingress's metrics and the processes' memory every `every`, until `stop`.
fn scraper(
    target: Target,
    token: String,
    pids: Vec<(String, String)>,
    rec: Arc<Recorder>,
    every: Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !stop.load(Ordering::Relaxed) {
            let at_ms = rec.now_ms();
            let metrics = target
                .metrics(&token)
                .await
                .map(|t| parse_metrics(&t))
                .unwrap_or_default();
            let mut rss = std::collections::BTreeMap::new();
            for (label, pid) in &pids {
                if let Some(k) = rss_kib(pid).await {
                    rss.insert(label.clone(), k);
                }
            }
            rec.scrape(Scrape {
                at_ms,
                metrics,
                rss_kib: rss,
            });
            tokio::time::sleep(every).await;
        }
    })
}

/// Drive `phases` against `world`; the phases as (name, rate, start ms, end ms). A ladder stops
/// early once a step ends with more backlog than the step before it ended with, by more than a
/// step's worth of a minute: the core no longer keeps up, and a higher step measures nothing new.
async fn drive(
    world: &Arc<World>,
    phases: &[Phase],
    max_inflight: usize,
    seed: u64,
    ladder: bool,
) -> Vec<(String, u64, u64, u64)> {
    let inflight = Arc::new(Semaphore::new(max_inflight));
    let per_flow = commands_per_flow_milli().max(1);
    let mut done = Vec::new();
    let mut rng = Rng::new(seed);
    let mut last_backlog: Option<u64> = None;
    for (i, p) in phases.iter().enumerate() {
        world.phase.store(i as u64, Ordering::Relaxed);
        let from = world.rec.now_ms();
        world.rec.note(format!(
            "phase {} at {} commands/min for {}s",
            p.name, p.per_min, p.secs
        ));
        // Flows a minute = commands a minute / commands per flow; the interval between flows.
        let flows_per_min_milli = u128::from(p.per_min) * 1_000_000 / u128::from(per_flow);
        let interval_us = 60_000_000_000u128
            .checked_div(flows_per_min_milli)
            .unwrap_or(u128::from(u64::MAX));
        let start = Instant::now();
        let end = Duration::from_secs(p.secs);
        let mut k: u128 = 0;
        loop {
            let due_us = k * interval_us;
            if due_us >= end.as_micros() {
                break;
            }
            let due = Duration::from_micros(u64::try_from(due_us).unwrap_or(u64::MAX));
            let elapsed = start.elapsed();
            if due > elapsed {
                tokio::time::sleep(due - elapsed).await;
            }
            k += 1;
            let flow = draw(&mut rng);
            let flow_rng = Rng::new(rng.next());
            match Arc::clone(&inflight).try_acquire_owned() {
                Ok(permit) => {
                    let w = Arc::clone(world);
                    tokio::spawn(async move {
                        let _permit = permit;
                        w.flow(flow, flow_rng).await;
                    });
                }
                Err(_) => {
                    world.rec.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let to = world.rec.now_ms();
        done.push((p.name.clone(), p.per_min, from, to));
        if ladder {
            let backlog = world
                .rec
                .scrapes()
                .last()
                .and_then(Scrape::backlog)
                .unwrap_or(0);
            if let Some(prev) = last_backlog {
                if backlog > prev + p.per_min {
                    world.rec.note(format!(
                        "ladder stops after {}: backlog {backlog} (was {prev}); the core no longer keeps up",
                        p.name
                    ));
                    break;
                }
            }
            last_backlog = Some(backlog);
        }
    }
    done
}

/// A recipient endpoint for notifications: answers `200` to every delivery and counts them. The
/// workflow registers it for the operator (as an issuing client registers its own), so the delivery worker
/// and the delivery records it sends back to the core are part of the load (`DEC-7`).
async fn receiver(addr: &str, count: Arc<std::sync::atomic::AtomicU64>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("receiver {addr}: {e}"))?;
    let app = axum::Router::new().fallback(move || {
        let count = Arc::clone(&count);
        async move {
            count.fetch_add(1, Ordering::Relaxed);
            axum::http::StatusCode::OK
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(())
}

/// Wait for the backlog to drain, up to `limit`: how long it took, or nothing. Drained means the
/// oldest undecided row, which ingress reads from its store, is younger than two seconds (a tick
/// may be pending) on three scrapes in a row. The counters are not used: they restart with
/// ingress, and a fault run restarts it.
async fn drain(world: &World, limit: Duration) -> Option<u64> {
    let started = Instant::now();
    world.rec.note("load stopped; draining");
    let mut quiet = 0;
    loop {
        let m = world
            .target
            .metrics(&world.operator)
            .await
            .map(|t| parse_metrics(&t))
            .unwrap_or_default();
        let oldest = m.get("aledgent_ingress_oldest_undecided_age_ms").copied();
        quiet = if oldest.is_some_and(|o| o < 2_000) {
            quiet + 1
        } else {
            0
        };
        if quiet >= 3 {
            let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            world.rec.note(format!("drained in {ms} ms"));
            return Some(ms);
        }
        if started.elapsed() > limit {
            world.rec.note(format!(
                "not drained within {}s: the oldest undecided row is {oldest:?} ms old",
                limit.as_secs()
            ));
            return None;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Read every acknowledged operation back: how many are decided, still `Received`, or not
/// found (`NFR-4`: none may be lost).
async fn loss_check(world: &Arc<World>) -> Value {
    let refs: Vec<String> = world
        .rec
        .acknowledged
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let sem = Arc::new(Semaphore::new(32));
    let mut tasks = Vec::new();
    for r in refs {
        let w = Arc::clone(world);
        let sem = Arc::clone(&sem);
        tasks.push(tokio::spawn(async move {
            let _p = sem.acquire().await;
            let (op, token) = r.split_once(' ').unwrap_or((r.as_str(), ""));
            let path = format!("/v1/operations/{op}");
            let mut r = w.target.call("GET", &path, Some(token), &[], None).await;
            // A passkey session lapses after 15 minutes: read as the operator instead.
            if matches!(&r, Ok(x) if x.status == 401 || x.status == 403) {
                r = w
                    .target
                    .call("GET", &path, Some(&w.operator), &[], None)
                    .await;
            }
            match r {
                Ok(x) if x.status == 200 => x.body["state"].as_str().unwrap_or("?").to_owned(),
                Ok(x) if x.status == 401 || x.status == 403 => "unreadable-auth".into(),
                Ok(x) => format!("http-{}", x.status),
                Err(e) => format!("transport-{e}"),
            }
        }));
    }
    let mut states: std::collections::BTreeMap<String, u64> = Default::default();
    for t in tasks {
        let s = t.await.unwrap_or_else(|_| "panicked".into());
        *states.entry(s).or_insert(0) += 1;
    }
    let total: u64 = states.values().sum();
    let received = states.get("Received").copied().unwrap_or(0);
    let unread: u64 = states
        .iter()
        .filter(|(k, _)| k.starts_with("http-") || k.starts_with("transport-"))
        .map(|(_, v)| *v)
        .sum();
    let lost = states.get("http-404").copied().unwrap_or(0);
    json!({
        "acknowledged": total,
        "states": states,
        "still_received": received,
        "not_found_or_unreadable": unread,
        "unreadable_auth": states.get("unreadable-auth").copied().unwrap_or(0),
        "lost": lost,
    })
}

/// `stress`.
pub async fn main(args: &[String]) -> Result<(), String> {
    let out = PathBuf::from(arg(args, "--out").ok_or("--out")?);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let dir = PathBuf::from(arg(args, "--config-dir").ok_or("--config-dir")?);
    let read = |name: &str| -> Result<String, String> {
        std::fs::read_to_string(dir.join(name))
            .map(|s| s.trim().to_owned())
            .map_err(|e| format!("{name}: {e}"))
    };
    let token = read("operator.token")?;
    let operator_identity = read("operator.identity")?;
    let url = arg(args, "--url").unwrap_or_else(|| "http://127.0.0.1:8080".into());
    let target = Target::http(&url, Duration::from_secs(120))?;
    let plan_name = arg(args, "--plan").unwrap_or_else(|| "smoke".into());
    let phases = match arg(args, "--phases") {
        Some(p) => parse_phases(&p)?,
        None => plan(&plan_name)?,
    };
    let pids: Vec<(String, String)> = args_all(args, "--pid")
        .into_iter()
        .filter_map(|p| p.split_once('=').map(|(a, b)| (a.to_owned(), b.to_owned())))
        .collect();
    let opts = SetupOptions {
        holders: usize::try_from(num(args, "--holders", 200)?).unwrap_or(200),
        passkey_pct: usize::try_from(num(args, "--passkey-pct", 30)?).unwrap_or(30),
        concurrency: usize::try_from(num(args, "--setup-concurrency", 8)?).unwrap_or(8),
        patience: Duration::from_secs(num(args, "--setup-patience-secs", 600)?),
        pollers: usize::try_from(num(args, "--pollers", 2000)?).unwrap_or(2000),
        service_pct: usize::try_from(num(args, "--service-pct", 50)?).unwrap_or(50),
    };
    let label = arg(args, "--label").unwrap_or_else(|| plan_name.clone());
    let delivered = Arc::new(std::sync::atomic::AtomicU64::new(0));
    if let Some(addr) = arg(args, "--receiver") {
        receiver(&addr, Arc::clone(&delivered)).await?;
    }
    run(RunOptions {
        target,
        token,
        operator_identity,
        phases,
        ladder: plan_name == "ladder" && arg(args, "--phases").is_none(),
        setup: opts,
        pids,
        max_inflight: usize::try_from(num(args, "--max-inflight", 4096)?).unwrap_or(4096),
        drain: Duration::from_secs(num(args, "--drain-secs", 900)?),
        seed: num(args, "--seed", 20_261_003)?,
        out,
        label,
        events: arg(args, "--events").map(PathBuf::from),
        delivered,
    })
    .await
}

/// A run's inputs.
pub struct RunOptions {
    /// The surface.
    pub target: Target,
    /// The operator's token.
    pub token: String,
    /// The operator's identity.
    pub operator_identity: String,
    /// The phases.
    pub phases: Vec<Phase>,
    /// Stop early once the core falls behind.
    pub ladder: bool,
    /// The population.
    pub setup: SetupOptions,
    /// Processes whose memory is followed, by label.
    pub pids: Vec<(String, String)>,
    /// Flows in flight at most.
    pub max_inflight: usize,
    /// How long to wait for the backlog to drain.
    pub drain: Duration,
    /// The mix's seed.
    pub seed: u64,
    /// Where the report goes.
    pub out: PathBuf,
    /// The run's label in the report.
    pub label: String,
    /// A file of `epoch_ms label` lines a fault injector appends to, read into the timeline.
    pub events: Option<PathBuf>,
    /// Deliveries the notification receiver took.
    pub delivered: Arc<std::sync::atomic::AtomicU64>,
}

/// One run: setup, the phases, drain, the checks, the report.
pub async fn run(o: RunOptions) -> Result<(), String> {
    std::fs::create_dir_all(&o.out).map_err(|e| format!("{}: {e}", o.out.display()))?;
    let rec = Arc::new(Recorder::default());
    let started_epoch_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
    )
    .map_err(|e| e.to_string())?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let scrape = scraper(
        o.target.clone(),
        o.token.clone(),
        o.pids.clone(),
        Arc::clone(&rec),
        Duration::from_secs(2),
        Arc::clone(&stop),
    );
    let setup_started = Instant::now();
    let world = world::setup(
        o.target.clone(),
        o.token.clone(),
        o.operator_identity.clone(),
        Arc::clone(&rec),
        &o.setup,
    )
    .await?;
    let setup_ms = u64::try_from(setup_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    rec.note(format!("setup took {setup_ms} ms"));
    let phases = drive(&world, &o.phases, o.max_inflight, o.seed, o.ladder).await;
    // Let the flows in flight finish sending (their replies are part of the last phase).
    tokio::time::sleep(Duration::from_secs(2)).await;
    let drained_ms = drain(&world, o.drain).await;
    let loss = loss_check(&world).await;
    rec.note(format!("loss check: {loss}"));
    let tb = o
        .target
        .call("GET", "/v1/trial-balance", Some(&o.token), &[], None)
        .await
        .map(|r| r.body)
        .unwrap_or(Value::Null);
    // Anchoring (scenario b): how many checkpoints the core took, from the operator's view.
    let anchors = match o
        .target
        .call("GET", "/v1/ops/anchors", Some(&o.token), &[], None)
        .await
    {
        Ok(r) if r.status == 200 => {
            let count = ["checkpoints", "anchors", "rows"]
                .iter()
                .find_map(|k| r.body[*k].as_array().map(Vec::len));
            json!({ "status": 200, "checkpoints": count })
        }
        Ok(r) => json!({ "status": r.status, "error": r.error() }),
        Err(e) => json!({ "error": e }),
    };
    stop.store(true, Ordering::Relaxed);
    let _ = scrape.await;
    if let Some(path) = &o.events {
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Some((t, what)) = line.split_once(' ') {
                    if let Ok(t) = t.parse::<u64>() {
                        rec.note(format!(
                            "event at +{}s: {what}",
                            t.saturating_sub(started_epoch_ms) / 1000
                        ));
                    }
                }
            }
        }
    }
    let summary = summarise(&rec, &phases);
    let mut checks = verdict(&summary);
    let lost = loss["lost"].as_u64().unwrap_or(0) + loss["still_received"].as_u64().unwrap_or(0);
    checks.push((
        "NFR-4: every acknowledged operation decided, none lost".into(),
        "0 lost or undecided".into(),
        format!("{lost} of {}", loss["acknowledged"]),
        lost == 0,
    ));
    let unbalanced = tb["unbalanced"].as_array().map_or(usize::MAX, Vec::len);
    checks.push((
        "INV-5: trial balance".into(),
        "no asset unbalanced".into(),
        if unbalanced == usize::MAX {
            format!("unreadable: {tb}")
        } else {
            format!("{unbalanced} unbalanced")
        },
        unbalanced == 0,
    ));
    // The headline (the owner's ruling of 2026-10-03): the greatest rate sustained for at least 90
    // s with every check of its phase passing, in commands a second, against 10 000 and 1 000 000.
    let mut best: Option<(String, u64)> = None;
    for p in summary["phases"].as_array().into_iter().flatten() {
        let name = p["phase"].as_str().unwrap_or("?");
        let secs = p["seconds"].as_u64().unwrap_or(0);
        let prefix = format!("{name}: ");
        let ok = checks
            .iter()
            .filter(|c| c.0.starts_with(&prefix))
            .all(|c| c.3);
        let per_min = p["sent_commands_per_min"].as_u64().unwrap_or(0);
        if ok && secs >= 90 && best.as_ref().is_none_or(|b| per_min > b.1) {
            best = Some((name.to_owned(), per_min));
        }
    }
    let headline = match &best {
        Some((phase, per_min)) => json!({
            "phase": phase,
            "commands_per_min": per_min,
            "commands_per_s": per_min / 60,
            "of_10k_per_s_permille": per_min * 1000 / 60 / 10_000,
            "of_1m_per_s_ppm": per_min * 1_000_000 / 60 / 1_000_000,
        }),
        None => json!({ "phase": null, "commands_per_s": 0 }),
    };
    let report = json!({
        "label": o.label,
        "headline": headline,
        "started_epoch_ms": started_epoch_ms,
        "setup_ms": setup_ms,
        "population": world::describe(&world),
        "mix": world::MIX.iter().map(|(f, w, c)| json!({"flow": format!("{f:?}"), "weight": w, "commands": c})).collect::<Vec<_>>(),
        "phases_planned": o.phases.iter().map(|p| json!({"name": p.name, "per_min": p.per_min, "secs": p.secs})).collect::<Vec<_>>(),
        "summary": summary,
        "drained_ms": drained_ms,
        "loss": loss,
        "trial_balance": tb,
        "anchors": anchors,
        "notifications_received": o.delivered.load(Ordering::Relaxed),
        "checks": checks.iter().map(|(w, b, m, ok)| json!({"check": w, "bound": b, "measured": m, "pass": ok})).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(o.out.join("report.json"), text).map_err(|e| e.to_string())?;
    let md = markdown(&report);
    std::fs::write(o.out.join("summary.md"), &md).map_err(|e| e.to_string())?;
    println!("{md}");
    Ok(())
}

fn ms(v: &Value) -> String {
    let us = v.as_u64().unwrap_or(0);
    if us >= 10_000_000 {
        format!("{} s", us / 1_000_000)
    } else {
        format!("{}.{} ms", us / 1000, (us % 1000) / 100)
    }
}

/// The report as Markdown: one table of phases, the checks, the loss check, memory.
pub fn markdown(r: &Value) -> String {
    let mut s = format!("## Stress run: {}\n\n", r["label"].as_str().unwrap_or("?"));
    let h = &r["headline"];
    s.push_str(&format!(
        "**Greatest sustained rate: {} commands a second** ({}), {} ‰ of 10 000 a second, {} ppm of 1 000 000.\n\n",
        h["commands_per_s"],
        h["phase"].as_str().unwrap_or("no phase held every bound for 90 s"),
        h["of_10k_per_s_permille"].as_u64().unwrap_or(0),
        h["of_1m_per_s_ppm"].as_u64().unwrap_or(0),
    ));
    s.push_str(&format!(
        "Population: {}. Setup {} s.\n\n",
        r["population"],
        r["setup_ms"].as_u64().unwrap_or(0) / 1000
    ));
    s.push_str("| Phase | Offered /min | Sent /min | Decided /min | Ack p50 | Ack p99 | Decided reply p99 | Completion p50 | Completion p99 | Errors | Max backlog | Oldest undecided |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for p in r["summary"]["phases"].as_array().into_iter().flatten() {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} ‰ ({}) | {} | {} s |\n",
            p["phase"].as_str().unwrap_or("?"),
            p["offered_commands_per_min"],
            p["sent_commands_per_min"],
            p["decisions_per_min"],
            ms(&p["acknowledgement_us"]["p50"]),
            ms(&p["acknowledgement_us"]["p99"]),
            ms(&p["decided_reply_us"]["p99"]),
            ms(&p["completion_us"]["p50"]),
            ms(&p["completion_us"]["p99"]),
            p["error_permille"],
            p["errors"],
            p["max_backlog"],
            p["max_oldest_undecided_ms"].as_u64().unwrap_or(0) / 1000,
        ));
    }
    s.push_str("\nAcknowledgements and reads, p50 / p99 at the client, by how the caller authenticated (an API key is read from the core on every request; a session and a configured token are checked at ingress):\n\n| Phase | operator | api_key | session |\n|---|---:|---:|---:|\n");
    for p in r["summary"]["phases"].as_array().into_iter().flatten() {
        let cell = |a: &str| {
            let v = &p["reply_us_by_auth"][a];
            if v.is_null() {
                "—".to_owned()
            } else {
                format!("{} / {}", ms(&v["p50"]), ms(&v["p99"]))
            }
        };
        s.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            p["phase"].as_str().unwrap_or("?"),
            cell("operator"),
            cell("api_key"),
            cell("session")
        ));
    }
    // T-032-11: where ingress's acknowledgement goes, from ingress's own series at each phase's
    // end (p50 / p99, ms).
    s.push_str("\nThe acknowledgement's parts in ingress, p50 / p99 at each phase's end (authentication, the integrating party's lookup, the answering setting's lookup, the received-log's commit):\n\n| Phase | auth | party | answering | append | whole |\n|---|---:|---:|---:|---:|---:|\n");
    for p in r["summary"]["phases"].as_array().into_iter().flatten() {
        let m = &p["ingress_metrics_at_end"];
        let cell = |part: &str| {
            let k = |q: &str| format!("aledgent_ingress_{part}_{q}_us");
            format!("{} / {}", ms(&m[k("p50")]), ms(&m[k("p99")]))
        };
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            p["phase"].as_str().unwrap_or("?"),
            cell("ack_auth"),
            cell("ack_party"),
            cell("ack_answering"),
            cell("ack_append"),
            cell("acknowledgement"),
        ));
    }
    s.push_str("\n| Check | Bound | Measured | |\n|---|---|---|---|\n");
    for c in r["checks"].as_array().into_iter().flatten() {
        s.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            c["check"].as_str().unwrap_or(""),
            c["bound"].as_str().unwrap_or(""),
            c["measured"].as_str().unwrap_or(""),
            if c["pass"].as_bool() == Some(true) {
                "pass"
            } else {
                "**FAIL**"
            }
        ));
    }
    s.push_str(&format!(
        "\nDrained in: {}. Loss check: {}. Dropped flows (harness limit): {}. Abandoned reservations: {}.\n\nMemory: {}\n",
        r["drained_ms"]
            .as_u64()
            .map(|m| format!("{} s", m / 1000))
            .unwrap_or_else(|| "**did not drain**".into()),
        r["loss"],
        r["summary"]["dropped_flows"],
        r["summary"]["abandoned_reservations"],
        r["summary"]["memory"],
    ));
    s
}
