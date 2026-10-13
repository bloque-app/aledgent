// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `aledgent-core-host fleet`: the book in shards across several machines (an experiment,
//! `NFR-21` (a)). Each machine runs this command
//! once per run with its own `--index`; `shards::net::Node` is the host and its relay, and this
//! file is only the VOS port, the rendezvous file and the coordinator's workload.
//!
//! Node 0 is the coordinator: the principals Agent, the router, the load and the anchor's
//! assembler, and `--first` shard Agents (0 by default). Every other node holds `--per-node`
//! shard Agents. Every node reads `--peers` (lines `<index> <ipv4> <public key hex>`, from
//! `tools/ci/fleet-rendezvous.sh`); node 0 announces every node to every node, and the nodes
//! form a mesh of Noise sessions on `--port`.
//!
//! **Phases** (`--phases`, comma-separated, all on one book, measured one by one, each followed by
//! quiescence and the checks):
//! - `closed:N`: N transfers in a closed loop, at most two rounds' worth outstanding per shard;
//! - `paced:R:S`: R transfers a second offered for S seconds; `paced:vP:S` offers P % of the last
//!   closed loop's rate;
//! - `burst:R:S:B:T:D`: B a second for T seconds, R a second for S seconds, B a second for T
//!   seconds again; the burst passes when everything offered in it is answered within D seconds
//!   of its end (F-032's gate 2: 50 × the reference minute M, 750 a second for 60 s at the
//!   synthetic M of 900 (F-032 plan.md §2), between minutes at 15 a second, drained within 30 s;
//!   owner ruling G-03: against the sharded capacity).
//!
//! Every phase revokes principal 0 half way and grants it again. Measured: decided/s, the
//! answer's latency at the router (`NFR-16`), the credit lag (`NFR-19`), the revocation lag
//! (`NFR-20`), rounds, gas, the shards' busy share and the relay's traffic; checked: the trial
//! balance with credits in transit, exactly once (read from the Agents' heads), every live anchor
//! at a consistent cut, and the final anchor.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use shards::host::{Lags, accounts};
use shards::net::noise::{Keypair, hex, unhex32};
use shards::net::wire::{Member, Report};
use shards::net::{Answered, Config, Layout, Node};
use shards::pool::Pool;
use shards::principals::PrincipalsCommand;
use shards::{Account, Command, Outcome, ShardId};

use crate::sharded::{DRIVER, Mix, ROOT, VosPort, arg, arg_str, op, rank};
use shards::host::{NativePort, Port};

const USD: u32 = 840;

/// `aledgent-core-host fleet-key <path>`: draw a relay key, write its secret half (hex) to
/// `<path>` (mode 0600), print its public half.
pub fn fleet_key(args: &[String]) {
    let k = Keypair::generate();
    let path = &args[2];
    std::fs::write(path, k.secret_hex()).expect("write the key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    println!("{}", hex(&k.public));
}

fn lags_of(mut v: Vec<u64>) -> Lags {
    if v.is_empty() {
        return Lags::default();
    }
    Lags {
        n: v.len() as u64,
        p50_us: rank(&mut v, 50),
        p99_us: rank(&mut v, 99),
        max_us: *v.iter().max().unwrap_or(&0),
    }
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Closed(u64),
    Paced {
        rate: u64,
        pct: bool,
        secs: u64,
    },
    Burst {
        rate: u64,
        secs: u64,
        base: u64,
        base_secs: u64,
        bound: u64,
    },
}

impl Phase {
    fn parse(s: &str) -> Phase {
        let p: Vec<&str> = s.split(':').collect();
        let n = |i: usize| -> u64 { p[i].parse().expect("a number in a phase") };
        match p[0] {
            "closed" => Phase::Closed(n(1)),
            "paced" if p[1].starts_with('v') => Phase::Paced {
                rate: p[1][1..].parse().expect("a percentage"),
                pct: true,
                secs: n(2),
            },
            "paced" => Phase::Paced {
                rate: n(1),
                pct: false,
                secs: n(2),
            },
            "burst" => Phase::Burst {
                rate: n(1),
                secs: n(2),
                base: n(3),
                base_secs: n(4),
                bound: n(5),
            },
            other => panic!("unknown phase {other}"),
        }
    }
}

struct Book {
    accts: Vec<Account>,
    fees: Pool,
    principals: Vec<[u8; 16]>,
}

/// One phase's results.
struct Row {
    label: String,
    offered: u64,
    decided: u64,
    accepted: u64,
    cross: u64,
    span_us: u64,
    ack: Lags,
    credit: Lags,
    /// The replay's lag (native decide): a round's native commit → its confirmation.
    replay: Lags,
    /// Decided per second during the offered window (a closed loop: until the last offer).
    load_rate: u64,
    revocation: Lags,
    round_p50_us: u64,
    round_p99_us: u64,
    rounds: u64,
    gas_per_round_m: u64,
    state_max: u64,
    busy_pct: u64,
    backlog_max: u64,
    drain_ms: Option<u64>,
    burst_pass: Option<bool>,
    anchors: u64,
    balanced: bool,
    exactly_once: bool,
    net_mb: u64,
    cpu_pct_max: u64,
    anchor: [u8; 32],
}

impl Row {
    fn rate(&self) -> u64 {
        (u128::from(self.decided) * 1_000_000 / u128::from(self.span_us.max(1))) as u64
    }
}

/// The coordinator's checks over every host's report: exactly once from the Agents' heads, and
/// the trial balance with credits in transit.
fn check(reports: &BTreeMap<u16, Report>) -> (bool, bool) {
    let shards: Vec<_> = reports.values().flat_map(|r| r.shards.iter()).collect();
    let applied: BTreeMap<(ShardId, ShardId), u64> = shards
        .iter()
        .flat_map(|s| s.applied.iter().map(move |(src, v)| ((*src, s.shard), *v)))
        .collect();
    let mut once = true;
    for s in &shards {
        once &= s.root != [0xFF; 32];
        for (d, e) in &s.enqueued {
            if applied.get(&(s.shard, *d)) != Some(e) {
                eprintln!(
                    "pair {}→{}: enqueued {e}, applied {:?}",
                    s.shard.0,
                    d.0,
                    applied.get(&(s.shard, *d))
                );
                once = false;
            }
        }
        once &= s.outbound_left.iter().all(|(_, left)| !left);
    }
    let (mut zero, mut sum, mut issued, mut transit, mut outstanding) =
        (true, 0i128, 0i128, 0i128, 0i128);
    for s in &shards {
        let Some((_, total, iss, tr)) = s.sums.first().copied() else {
            return (false, once);
        };
        zero &= total == 0;
        sum += total - iss - tr;
        issued += iss;
        transit += tr;
        for (d, seq, asset, amount) in &s.untrimmed {
            if *asset == USD && *seq > applied.get(&(s.shard, *d)).copied().unwrap_or(0) {
                outstanding += i128::from(*amount);
            }
        }
    }
    (
        zero && sum + issued + transit == 0 && transit == outstanding,
        once,
    )
}

/// What one phase's load came to at the router.
struct Offered {
    offered: u64,
    answers: Vec<Answered>,
    /// Refused at ingress (a revoked principal): decided there.
    refused: u64,
    span_us: u64,
    backlog_max: u64,
    /// From the last offer to the last answer.
    drain_ms: Option<u64>,
    /// From the burst's end to its last command's answer.
    burst_drain_ms: Option<u64>,
    /// Commands answered (or refused at ingress) by the end of the offered window.
    during_load: u64,
    /// The offered window, µs.
    load_us: u64,
}

struct Coordinator<P: Port + Send + 'static> {
    node: Arc<Node<P>>,
    book: Book,
    batch: u64,
    anchor_every: Duration,
    drain_cap: Duration,
    last_closed_rate: u64,
    gas_before: u64,
    rounds_before: u64,
    anchors: u64,
    /// No answer for this long, with commands outstanding: the phase is cut off.
    stall: Duration,
    stalled: bool,
}

impl<P: Port + Send + 'static> Coordinator<P> {
    fn revoke(&self) {
        let mut cmds = vec![PrincipalsCommand::Revoke {
            principal: self.book.principals[0],
        }];
        for a in self.book.accts.iter().step_by(self.book.principals.len()) {
            cmds.push(PrincipalsCommand::Grant {
                principal: self.book.principals[0],
                payer: *a,
                limit: 1_000,
            });
        }
        self.node.principals(&cmds).expect("the revocation");
    }

    /// Offer the load of `phase`.
    fn offer(&mut self, phase: Phase, mix: &mut Mix) -> Offered {
        let total_shards = u64::from(self.node.layout().total());
        let window = 2 * self.batch * total_shards;
        // (rate per second, seconds) segments; a closed loop is one segment of rate 0.
        let (segments, closed_n): (Vec<(u64, u64)>, u64) = match phase {
            Phase::Closed(n) => (vec![], n),
            Phase::Paced { rate, pct, secs } => {
                let r = if pct {
                    (self.last_closed_rate * rate / 100).max(1)
                } else {
                    rate
                };
                (vec![(r, secs)], 0)
            }
            Phase::Burst {
                rate,
                secs,
                base,
                base_secs,
                ..
            } => (vec![(base, base_secs), (rate, secs), (base, base_secs)], 0),
        };
        let target: u64 = if segments.is_empty() {
            closed_n
        } else {
            segments.iter().map(|(r, s)| r * s).sum()
        };
        let start = Instant::now();
        let mut sent = 0u64;
        let mut refused = 0u64;
        let mut answers: Vec<Answered> = Vec::new();
        let mut revoked = false;
        let mut last_anchor = Instant::now();
        let mut backlog_max = 0u64;
        let mut sent_done: Option<Instant> = None;
        // The burst's own commands: tickets offered in the middle segment.
        let mut burst_tickets: Option<(u64, u64)> = None;
        let mut burst_window: Option<(Instant, Instant)> = None;
        let mut burst_left = 0u64;
        let mut burst_done: Option<Instant> = None;
        let mut first_ticket: Option<u64> = None;
        let mut last_progress = Instant::now();
        let mut during_load = 0u64;
        let mut last_answer = Instant::now();
        loop {
            let now = Instant::now();
            let el = now.duration_since(start).as_micros() as u64;
            let allowance = if segments.is_empty() {
                let outstanding = self.node.unanswered() as u64;
                window.saturating_sub(outstanding).min(target - sent)
            } else {
                // Cumulative offer by now, segment by segment.
                let mut due = 0u64;
                let mut t0 = 0u64;
                for (r, s) in &segments {
                    let len = s * 1_000_000;
                    let inside = el.saturating_sub(t0).min(len);
                    due += (u128::from(*r) * u128::from(inside) / 1_000_000) as u64;
                    t0 += len;
                }
                due.min(target).saturating_sub(sent)
            };
            if allowance > 0 {
                let batch: Vec<_> = (0..allowance)
                    .map(|_| {
                        let i = (mix.next() % self.book.accts.len() as u64) as usize;
                        let payee = if mix.next() % 100 < 10 {
                            self.book.fees.id
                        } else {
                            self.book.accts[(mix.next() % self.book.accts.len() as u64) as usize]
                        };
                        let by = (mix.next() % 100 < 50)
                            .then(|| self.book.principals[i % self.book.principals.len()]);
                        (by, self.book.accts[i], payee, USD, 1 + mix.next() % 500)
                    })
                    .collect();
                for r in self.node.transfers(&batch) {
                    match r {
                        Ok(t) => {
                            first_ticket.get_or_insert(t);
                            if let Phase::Burst {
                                base, base_secs, ..
                            } = phase
                            {
                                let in_burst =
                                    sent >= base * base_secs && sent < target - base * base_secs;
                                if in_burst {
                                    let e = burst_tickets.get_or_insert((t, t));
                                    e.1 = t;
                                    burst_left += 1;
                                }
                            }
                        }
                        Err(_) => refused += 1,
                    }
                    sent += 1;
                }
                if let Phase::Burst {
                    base,
                    base_secs,
                    rate,
                    secs,
                    ..
                } = phase
                {
                    if burst_window.is_none() && sent >= base * base_secs + rate * secs {
                        let b0 = start + Duration::from_secs(base_secs);
                        burst_window = Some((b0, b0 + Duration::from_secs(secs)));
                    }
                }
            }
            if !revoked && sent >= target / 2 {
                self.revoke();
                revoked = true;
            }
            if sent >= target && sent_done.is_none() {
                sent_done = Some(Instant::now());
                during_load = answers.len() as u64 + refused;
            }
            backlog_max = backlog_max.max(self.node.unanswered() as u64);
            let got = self.node.answers(Duration::from_millis(2));
            if let Some((a, b)) = burst_tickets {
                for g in &got {
                    if g.ticket >= a && g.ticket <= b {
                        burst_left = burst_left.saturating_sub(1);
                    }
                }
                if burst_window.is_some() && burst_left == 0 && burst_done.is_none() {
                    burst_done = Some(Instant::now());
                }
            }
            if !got.is_empty() {
                last_answer = Instant::now();
            }
            answers.extend(got);
            if last_anchor.elapsed() >= self.anchor_every {
                self.node
                    .anchor()
                    .expect("every live anchor is at a consistent cut");
                self.anchors += 1;
                last_anchor = Instant::now();
            }
            if let Some(h) = self.node.halted() {
                // The other nodes would wait for this run until their timeout: tell them first.
                println!(
                    "| {} | {} | halted after {} s: {h}; offered {sent}, answered {} | | | | | | | | | | | | | |",
                    self.node.layout().nodes,
                    self.node.layout().total(),
                    start.elapsed().as_secs(),
                    answers.len()
                );
                self.node.tell_shutdown();
                eprintln!("node 0 halted: {h}");
                std::process::exit(1);
            }
            if last_progress.elapsed() >= Duration::from_secs(10) {
                last_progress = Instant::now();
                eprintln!(
                    "  {} s: offered {sent} of {target}, answered {}, outstanding {}, anchors {}",
                    start.elapsed().as_secs(),
                    answers.len(),
                    self.node.unanswered(),
                    self.anchors
                );
            }
            if sent >= target && self.node.unanswered() == 0 {
                break;
            }
            if self.node.unanswered() > 0 && last_answer.elapsed() > self.stall {
                eprintln!(
                    "STALL: no answer for {} s with {} outstanding: a shard Agent is stuck (its image near VOS's limit?)",
                    self.stall.as_secs(),
                    self.node.unanswered()
                );
                self.stalled = true;
                break;
            }
            if sent_done.is_some_and(|d| d.elapsed() > self.drain_cap) {
                eprintln!(
                    "{} commands unanswered {} s after the last offer: cut off",
                    self.node.unanswered(),
                    self.drain_cap.as_secs()
                );
                break;
            }
        }
        let last = Instant::now();
        let span = last.duration_since(start).as_micros() as u64;
        let drain = sent_done.map(|d| last.duration_since(d).as_millis() as u64);
        let burst = burst_window.map(|(_, end)| {
            let done = burst_done.unwrap_or(last);
            (
                end,
                done,
                done.saturating_duration_since(end).as_millis() as u64,
            )
        });
        Offered {
            offered: sent,
            answers,
            refused,
            span_us: span,
            backlog_max,
            drain_ms: drain,
            burst_drain_ms: burst.map(|(_, _, ms)| ms),
            during_load,
            load_us: sent_done.map_or(span, |d| d.duration_since(start).as_micros() as u64),
        }
    }

    fn phase(&mut self, phase: Phase, mix: &mut Mix) -> Row {
        let label = match phase {
            Phase::Closed(n) => format!("closed {n}"),
            Phase::Paced {
                rate,
                pct: true,
                secs,
            } => format!(
                "paced {rate} % of closed ({}/s) × {secs} s",
                self.last_closed_rate * rate / 100
            ),
            Phase::Paced { rate, secs, .. } => format!("paced {rate}/s × {secs} s"),
            Phase::Burst {
                rate,
                secs,
                base,
                base_secs,
                bound,
            } => {
                format!(
                    "burst {base}/s {base_secs} s, {rate}/s {secs} s, {base}/s {base_secs} s; drain ≤ {bound} s"
                )
            }
        };
        eprintln!("phase: {label}");
        let anchors_before = self.anchors;
        let Offered {
            offered,
            answers,
            refused,
            span_us,
            backlog_max,
            drain_ms,
            burst_drain_ms: burst,
            during_load,
            load_us,
        } = self.offer(phase, mix);
        if self.stalled {
            println!(
                "| {} | {} | {label} | {offered} | stalled: {} answered, no answer for {} s; halted: {:?} | | | | | | | | | | | | |",
                self.node.layout().nodes,
                self.node.layout().total(),
                answers.len(),
                self.stall.as_secs(),
                self.node.halted()
            );
            self.node.tell_shutdown();
            std::process::exit(1);
        }
        let quiet = self.node.quiesce(Duration::from_secs(600));
        if let Err(e) = &quiet {
            eprintln!("{e}");
        }
        let (anchor, cut) = self.node.anchor().expect("the phase's last anchor");
        let _ = cut;
        let reports = self
            .node
            .reports(&[USD], &[], true, Duration::from_secs(600))
            .expect("every host's report");
        let (balanced, once) = check(&reports);
        let shards: Vec<_> = reports.values().flat_map(|r| r.shards.iter()).collect();
        let mut round_us: Vec<u64> = shards
            .iter()
            .flat_map(|s| s.round_us.iter().map(|v| u64::from(*v)))
            .collect();
        let rounds = round_us.len() as u64;
        let gas: u64 = shards.iter().map(|s| s.stats.actor_gas).sum();
        let all_rounds: u64 = shards.iter().map(|s| s.stats.invocations).sum();
        let gas_per_round_m =
            (gas - self.gas_before) / (all_rounds - self.rounds_before).max(1) / 1_000_000;
        self.gas_before = gas;
        self.rounds_before = all_rounds;
        let busy: u64 = shards.iter().map(|s| s.busy_us).sum();
        let wall: u64 = reports.values().map(|r| r.wall_us).max().unwrap_or(1);
        let credit: Vec<u64> = reports
            .values()
            .flat_map(|r| r.credit_lag_us.iter().map(|v| u64::from(*v)))
            .collect();
        let cpu_pct_max = reports
            .values()
            .map(|r| r.cpu_us * 100 / r.wall_us.max(1))
            .max()
            .unwrap_or(0);
        let net_mb = reports.values().map(|r| r.net.sent_bytes).sum::<u64>() >> 20;
        let decided = answers.len() as u64 + refused;
        let row = Row {
            label,
            offered,
            decided,
            accepted: answers.iter().filter(|a| a.outcome.accepted()).count() as u64,
            cross: answers
                .iter()
                .filter(|a| matches!(a.outcome, Outcome::Debited { .. }))
                .count() as u64,
            span_us,
            ack: lags_of(answers.iter().map(|a| a.ack_us).collect()),
            credit: lags_of(credit),
            replay: lags_of(
                shards
                    .iter()
                    .flat_map(|s| s.replay_us.iter().map(|v| u64::from(*v)))
                    .collect(),
            ),
            load_rate: (u128::from(during_load) * 1_000_000 / u128::from(load_us.max(1))) as u64,
            revocation: lags_of(self.node.take_revocation_lags()),
            round_p50_us: rank(&mut round_us, 50),
            round_p99_us: rank(&mut round_us, 99),
            rounds,
            gas_per_round_m,
            state_max: shards
                .iter()
                .map(|s| s.stats.state_bytes)
                .max()
                .unwrap_or(0),
            busy_pct: busy * 100 / (wall * shards.len() as u64).max(1),
            backlog_max,
            drain_ms,
            burst_pass: match (phase, burst) {
                (Phase::Burst { bound, .. }, Some(ms)) => Some(ms <= bound * 1000),
                (Phase::Burst { .. }, None) => Some(false),
                _ => None,
            },
            anchors: self.anchors - anchors_before,
            balanced: balanced && quiet.is_ok(),
            exactly_once: once && quiet.is_ok(),
            net_mb,
            cpu_pct_max,
            anchor,
        };
        if let Phase::Burst { .. } = phase {
            if let Some(ms) = burst {
                eprintln!("burst: drained {ms} ms after its minute ended");
            }
        }
        if let Phase::Closed(_) = phase {
            self.last_closed_rate = row.rate();
        }
        row
    }
}

/// A peer's address: `<ipv4>` on the run's port, or `<ipv4>:<port>` as given (a rehearsal on one
/// machine).
fn addr_of(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        ip.to_string()
    } else {
        format!("{ip}:{port}")
    }
}

/// The peers file: `<index> <ipv4>[:port] <public key hex>` per line.
fn peers(path: &str) -> BTreeMap<u16, (String, [u8; 32])> {
    std::fs::read_to_string(path)
        .expect("the peers file")
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let i = f.next()?.parse().ok()?;
            let ip = f.next()?.to_string();
            let key = unhex32(f.next()?)?;
            Some((i, (ip, key)))
        })
        .collect()
}

/// The cores to pin this host's shard Agents to, one each, when the host has more cores than
/// Agents (core 0 is left to the host's own threads).
fn pinning(n: usize) -> Option<Vec<core_affinity::CoreId>> {
    if std::env::var("SHARDS_PIN").is_ok_and(|v| v == "0") {
        return None;
    }
    let cores = core_affinity::get_core_ids()?;
    (cores.len() > n).then_some(cores)
}

/// `aledgent-core-host fleet <runtime.pvm> <shard.vos> <out.csv> --index I --nodes N --peers F
/// --port P --key K [--first K0] [--per-node K] [--batch B] [--accounts A] [--phases ...]
/// [--chainspec <file> [--grown-max N]] (a book grown from a chainspec, `crate::chainspec::ballast`)
/// [--anchor-ms MS] [--wait-secs S] [--net-id ID]` (module documentation).
pub fn fleet(args: &[String]) {
    let runtime = Arc::new(std::fs::read(&args[2]).expect("the runtime"));
    let package = Arc::new(std::fs::read(&args[3]).expect("the shard package"));
    let out = args[4].clone();
    let me = arg(args, "--index", 0) as u16;
    let nodes = arg(args, "--nodes", 2) as u16;
    let layout = Layout {
        nodes,
        first: arg(args, "--first", 0) as u16,
        per_node: arg(args, "--per-node", 1) as u16,
    };
    let batch = arg(args, "--batch", 64);
    let cfg = Config {
        batch: batch as usize,
        max_credits: arg(args, "--max-credits", 64) as usize,
        // F-034 across hosts: rounds decided natively and replayed through the Agents.
        native: args.iter().any(|a| a == "--native-decide"),
        replay_batch: arg(args, "--replay-batch", 2) as usize,
        round_wait: Duration::from_millis(arg(args, "--round-wait-ms", 0)),
        ..Config::default()
    };
    let port = arg(args, "--port", 47_100) as u16;
    let wait = Duration::from_secs(arg(args, "--wait-secs", 2_400));
    let net_id = format!(
        "aledgent-fleet/{}/{nodes}",
        arg_str(args, "--net-id", "bench")
    );
    let secret =
        std::fs::read_to_string(arg_str(args, "--key", "relay.key")).expect("the relay key");
    let key = Keypair::from_secret(unhex32(&secret).expect("a 32-byte hex key"));
    let peers = peers(arg_str(args, "--peers", "peers.txt"));

    // This host's Agents: VOS Agents, each on its own thread (pinned one per core where the host
    // has room), or with `--native` the same transition natively (a local rehearsal).
    let mine: Vec<u16> = layout.shards_of(me).collect();
    let t = Instant::now();
    let ctx = Ctx {
        me,
        nodes,
        port,
        wait,
        batch,
        net_id,
        peers,
        out,
    };
    if args.iter().any(|a| a == "--native") {
        let ports = mine.iter().map(|_| NativePort::new(DRIVER)).collect();
        let principals = (me == 0).then(|| NativePort::new(DRIVER));
        let logs = mine.iter().map(|_| None).collect();
        let node = Node::found(me, layout, cfg, ports, logs, principals, ROOT, [0x77; 32])
            .expect("found the Agents");
        eprintln!(
            "node {me} of {nodes}: shards {:?} founded natively in {} ms",
            layout.shards_of(me),
            t.elapsed().as_millis()
        );
        serve(Arc::new(node), key, &ctx, args);
        return;
    }
    let cores = pinning(mine.len() + usize::from(me == 0));
    let core = |i: usize| cores.as_ref().map(|c| c[(i + 1) % c.len()]);
    let mut ports = Vec::new();
    for (i, s) in mine.iter().enumerate() {
        ports.push(
            VosPort::spawn(
                Arc::clone(&runtime),
                Arc::clone(&package),
                0x50u8.wrapping_add(*s as u8),
                core(i),
            )
            .expect("a shard Agent"),
        );
    }
    let principals = (me == 0).then(|| {
        VosPort::spawn(
            Arc::clone(&runtime),
            Arc::clone(&package),
            0x4F,
            cores.as_ref().map(|c| c[0]),
        )
        .expect("the principals Agent")
    });
    let logs = mine.iter().map(|_| None).collect();
    let node = Node::found(me, layout, cfg, ports, logs, principals, ROOT, [0x77; 32])
        .expect("found the Agents");
    eprintln!(
        "node {me} of {nodes}: shards {:?} founded in {} ms; {} vCPU; Agents {}",
        layout.shards_of(me),
        t.elapsed().as_millis(),
        core_affinity::get_core_ids().map_or(0, |c| c.len()),
        if cores.is_some() {
            "pinned"
        } else {
            "not pinned"
        }
    );
    serve(Arc::new(node), key, &ctx, args);
}

/// What every node of a run is told.
struct Ctx {
    me: u16,
    nodes: u16,
    port: u16,
    wait: Duration,
    batch: u64,
    net_id: String,
    peers: BTreeMap<u16, (String, [u8; 32])>,
    out: String,
}

/// Listen, then coordinate (node 0) or serve until the coordinator stops this node, with a
/// progress line every 30 s on the console.
fn serve<P: Port + Send + 'static>(node: Arc<Node<P>>, key: Keypair, c: &Ctx, args: &[String]) {
    let bind = arg_str(args, "--bind", "0.0.0.0");
    node.listen(
        key,
        format!("{bind}:{}", c.port).parse().expect("an address"),
        c.net_id.as_bytes(),
    )
    .expect("listen");
    let me = c.me;
    if me != 0 {
        let (ip, k) = c.peers.get(&0).expect("node 0 in the peers file").clone();
        node.add_peer(0, Some(addr_of(&ip, c.port)), k);
        let started = Instant::now();
        let mut stopped = false;
        let mut was_up = false;
        let mut down_since: Option<Instant> = None;
        while !stopped && started.elapsed() < c.wait + Duration::from_secs(4 * 3600) {
            stopped = node.wait_stopped(Duration::from_secs(30));
            let st = node.status();
            // A coordinator gone for two minutes after the run began is not coming back.
            if st.connected > 0 {
                was_up = true;
                down_since = None;
            } else if was_up {
                let since = *down_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(120) {
                    eprintln!(
                        "node {me}: the coordinator has been gone for two minutes; ending this run"
                    );
                    break;
                }
            }
            eprintln!(
                "node {me} at {} s: connected {}, queued {}, inbox {}, outbox {}, audit {}, running {}, feed {}, halted {:?}",
                started.elapsed().as_secs(),
                st.connected,
                st.queued,
                st.inbox,
                st.outbox,
                st.audit,
                st.running,
                st.feed,
                st.halted
            );
        }
        eprintln!(
            "node {me}: {}",
            if stopped {
                "stopped by the coordinator"
            } else {
                "timed out"
            }
        );
        node.stop();
        if let Some(h) = node.halted() {
            eprintln!("node {me} halted: {h}");
            std::process::exit(1);
        }
        return;
    }
    coordinate(
        node, &c.peers, c.port, c.nodes, c.wait, c.batch, args, &c.out,
    );
}

#[allow(clippy::too_many_arguments)]
fn coordinate<P: Port + Send + 'static>(
    node: Arc<Node<P>>,
    peers: &BTreeMap<u16, (String, [u8; 32])>,
    port: u16,
    nodes: u16,
    wait: Duration,
    batch: u64,
    args: &[String],
    out: &str,
) {
    let members: Vec<Member> = (0..nodes)
        .map(|i| {
            let (ip, key) = peers
                .get(&i)
                .unwrap_or_else(|| panic!("node {i} in the peers file"))
                .clone();
            Member {
                index: i,
                addr: addr_of(&ip, port),
                key,
            }
        })
        .collect();
    node.set_roster(members);
    // Every node with a session to every other.
    let t = Instant::now();
    loop {
        match node.statuses(Duration::from_secs(5)) {
            Ok(all) if all.values().all(|s| s.connected + 1 == nodes) => break,
            Ok(all) => {
                if t.elapsed() > wait {
                    let c: Vec<u16> = all.values().map(|s| s.connected).collect();
                    panic!("the mesh did not form in {} s: {c:?}", wait.as_secs());
                }
            }
            Err(e) => {
                if t.elapsed() > wait {
                    panic!("the mesh did not form in {} s: {e}", wait.as_secs());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!(
        "mesh of {nodes} nodes formed in {} ms",
        t.elapsed().as_millis()
    );
    let layout = node.layout();

    // Set-up through the Agents: accounts and the fee pool, grants, funds.
    let t = Instant::now();
    let n_accounts = arg(args, "--accounts", 256) as u32;
    let dir = node.directory();
    let book = Book {
        accts: accounts(n_accounts),
        fees: Pool::new([0xFE; 16], &dir),
        principals: (0..arg(args, "--principals", 8).max(1))
            .map(|i| {
                let mut p = [0xA0u8; 16];
                p[8..].copy_from_slice(&i.to_be_bytes());
                p
            })
            .collect(),
    };
    let mut cmds: Vec<PrincipalsCommand> = book
        .accts
        .iter()
        .chain(book.fees.members.iter())
        .map(|a| PrincipalsCommand::Open {
            account: *a,
            asset: USD,
        })
        .collect();
    // The grown state (`--chainspec`): the chainspec's ledger accounts, routed by the router's own
    // rule (`crate::chainspec::ballast`), opened here and funded below.
    let grown = match crate::sharded::arg_str(args, "--chainspec", "") {
        "" => Vec::new(),
        path => crate::chainspec::ballast(path, crate::sharded::args_max(args))
            .unwrap_or_else(|e| panic!("--chainspec: {e}")),
    };
    cmds.extend(grown.iter().map(|(a, _)| PrincipalsCommand::Open {
        account: *a,
        asset: USD,
    }));
    if !grown.is_empty() {
        eprintln!("grown state: {} accounts from the chainspec", grown.len());
    }
    for (i, a) in book.accts.iter().enumerate() {
        cmds.push(PrincipalsCommand::Grant {
            principal: book.principals[i % book.principals.len()],
            payer: *a,
            limit: 1_000,
        });
    }
    node.principals(&cmds).expect("open and grant");
    node.add_pool(book.fees.clone());
    while node.feed_everywhere() < node.feed_version() {
        assert!(
            t.elapsed() < Duration::from_secs(1_800),
            "the shards did not learn the accounts"
        );
        if let Some(h) = node.halted() {
            panic!("halted: {h}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for (i, a) in book.accts.iter().enumerate() {
        node.operator(Command::Issue {
            op: op(1 << 40 | i as u64),
            account: *a,
            asset: USD,
            amount: 1_000_000_000,
        });
    }
    let mut funded = book.accts.len();
    for (i, (a, amount)) in grown.iter().enumerate() {
        if *amount > 0 {
            node.operator(Command::Issue {
                op: op(1 << 41 | i as u64),
                account: *a,
                asset: USD,
                amount: *amount,
            });
            funded += 1;
        }
    }
    let mut issued = 0;
    while issued < funded {
        issued += node.answers(Duration::from_millis(100)).len();
        assert!(
            t.elapsed() < Duration::from_secs(1_800),
            "funding did not finish"
        );
    }
    node.quiesce(Duration::from_secs(600))
        .expect("quiescent after set-up");
    let _ = node
        .reports(&[USD], &[], true, Duration::from_secs(600))
        .expect("reset");
    eprintln!("set up in {} ms", t.elapsed().as_millis());

    let mut co = Coordinator {
        node: Arc::clone(&node),
        book,
        batch,
        anchor_every: Duration::from_millis(arg(args, "--anchor-ms", 1_000)),
        drain_cap: Duration::from_secs(arg(args, "--drain-cap-secs", 300)),
        last_closed_rate: 0,
        gas_before: 0,
        rounds_before: 0,
        anchors: 0,
        stall: Duration::from_secs(arg(args, "--stall-secs", 180)),
        stalled: false,
    };
    let reports = node
        .reports(&[USD], &[], false, Duration::from_secs(600))
        .expect("reports");
    let shards: Vec<_> = reports.values().flat_map(|r| r.shards.iter()).collect();
    co.gas_before = shards.iter().map(|s| s.stats.actor_gas).sum();
    co.rounds_before = shards.iter().map(|s| s.stats.invocations).sum();

    let phases: Vec<Phase> = arg_str(args, "--phases", "closed:4000")
        .split(',')
        .map(|p| Phase::parse(p.trim()))
        .collect();
    let mut mix = Mix(0xC0FF_EE00 + u64::from(nodes));
    let mut csv = String::from(
        "nodes,shards,phase,offered,decided,accepted,cross,span_us,decided_per_s,ack_p50_us,ack_p99_us,ack_max_us,credit_lag_p50_us,credit_lag_p99_us,credit_lag_max_us,revocation_lag_max_us,round_p50_us,round_p99_us,rounds,actor_gas_per_round_m,agent_state_max_bytes,shard_busy_pct,backlog_max,drain_ms,burst_pass,live_anchors,trial_balance,exactly_once,relay_sent_mb,cpu_pct_max,anchor,decided_during_load_per_s,replay_lag_p50_us,replay_lag_p99_us\n",
    );
    println!(
        "| Nodes | Shards | Phase | Offered | Decided/s | During the load /s | Replay lag p50 / p99 ms | Ack p50 / p99 / max ms | Credit lag p50 / p99 ms | Revocation lag ms | Round p50 / p99 ms | Gas per round (M) | Shards busy | Backlog max | Drain after load ms | Burst | Live anchors | Trial balance | Exactly once |"
    );
    println!(
        "|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---|---|"
    );
    let mut failed = false;
    for p in phases {
        let r = co.phase(p, &mut mix);
        let yn = |b: bool| if b { "yes" } else { "**no**" };
        println!(
            "| {} | {} | {} | {} | **{}** | {} | {} / {} | {} / {} / {} | {} / {} | {} | {} / {} | {} | {} % | {} | {} | {} | {} | {} | {} |",
            nodes,
            layout.total(),
            r.label,
            r.offered,
            r.rate(),
            r.load_rate,
            r.replay.p50_us / 1000,
            r.replay.p99_us / 1000,
            r.ack.p50_us / 1000,
            r.ack.p99_us / 1000,
            r.ack.max_us / 1000,
            r.credit.p50_us / 1000,
            r.credit.p99_us / 1000,
            r.revocation.max_us / 1000,
            r.round_p50_us / 1000,
            r.round_p99_us / 1000,
            r.gas_per_round_m,
            r.busy_pct,
            r.backlog_max,
            r.drain_ms.map_or("cut".to_string(), |d| d.to_string()),
            match r.burst_pass {
                None => "",
                Some(true) => "absorbed and drained",
                Some(false) => "**not drained in bound**",
            },
            r.anchors,
            if r.balanced { "holds" } else { "**broken**" },
            yn(r.exactly_once),
        );
        let _ = writeln!(
            csv,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            nodes,
            layout.total(),
            r.label.replace(',', ";"),
            r.offered,
            r.decided,
            r.accepted,
            r.cross,
            r.span_us,
            r.rate(),
            r.ack.p50_us,
            r.ack.p99_us,
            r.ack.max_us,
            r.credit.p50_us,
            r.credit.p99_us,
            r.credit.max_us,
            r.revocation.max_us,
            r.round_p50_us,
            r.round_p99_us,
            r.rounds,
            r.gas_per_round_m,
            r.state_max,
            r.busy_pct,
            r.backlog_max,
            r.drain_ms.map_or(String::from("cut"), |d| d.to_string()),
            r.burst_pass.map_or("", |b| if b { "yes" } else { "no" }),
            r.anchors,
            if r.balanced { "holds" } else { "BROKEN" },
            if r.exactly_once { "yes" } else { "NO" },
            r.net_mb,
            r.cpu_pct_max,
            hex(&r.anchor),
            r.load_rate,
            r.replay.p50_us,
            r.replay.p99_us,
        );
        failed |= !r.balanced || !r.exactly_once;
        if node.halted().is_some() {
            failed = true;
            break;
        }
    }
    let state: u64 = node
        .reports(&[], &[], false, Duration::from_secs(600))
        .map(|r| {
            r.values()
                .flat_map(|x| x.shards.iter())
                .map(|s| s.stats.state_bytes)
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    println!();
    println!(
        "Largest shard Agent image at the end: {state} bytes (a Local Agent fails near 4.4 MB)."
    );
    std::fs::write(out, csv).expect("write the CSV");
    node.shutdown_all();
    if failed {
        std::process::exit(1);
    }
}
