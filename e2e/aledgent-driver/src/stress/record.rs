// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What a run records, and the numbers it reports. Integers only (`INV-7`, the workspace denies
//! floating point): latencies in microseconds, rates in commands per minute, percentiles by
//! nearest rank.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{Value, json};

/// What a command is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// `POST /v1/transfers`, one leg.
    Transfer,
    /// `POST /v1/transfers`, several legs.
    MultiLeg,
    /// `POST /v1/reservations`.
    Reserve,
    /// `POST /v1/reservations/{r}/capture`.
    Capture,
    /// `POST /v1/reservations/{r}/release`.
    Release,
    /// `POST /v1/issuances`.
    Issue,
    /// `POST /v1/redemptions`.
    Redeem,
    /// `POST /v1/identities`.
    OpenIdentity,
    /// `POST /v1/accounts`.
    OpenAccount,
    /// `POST /v1/auth/challenge` and `POST /v1/auth/passkey`, as one ceremony.
    SignIn,
    /// A balance or an operation read.
    Read,
}

impl Kind {
    /// Its name in reports.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Transfer => "transfer",
            Kind::MultiLeg => "transfer_multileg",
            Kind::Reserve => "reserve",
            Kind::Capture => "capture",
            Kind::Release => "release",
            Kind::Issue => "issue",
            Kind::Redeem => "redeem",
            Kind::OpenIdentity => "open_identity",
            Kind::OpenAccount => "open_account",
            Kind::SignIn => "passkey_sign_in",
            Kind::Read => "read",
        }
    }

    /// Whether it moves value and is acknowledged at `Received` (`REQ-CS-8`, `NFR-1`).
    pub fn acknowledged(self) -> bool {
        matches!(self, Kind::Transfer | Kind::MultiLeg | Kind::Reserve)
    }

    /// Whether it is a command (a read and a sign-in are not).
    pub fn command(self) -> bool {
        !matches!(self, Kind::Read | Kind::SignIn)
    }
}

/// How a request ended.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    /// `202`: acknowledged at `Received`.
    Acknowledged,
    /// `200`: answered with its decision (or read).
    Ok,
    /// `4xx` with a refusal about the caller or the request (`ERR-…`).
    Refused(String),
    /// `503 ERR-DecisionUnavailable`: the core could not decide within `decision_wait`.
    DecisionUnavailable,
    /// Any other `5xx`.
    ServerError(String),
    /// No reply: connection refused, reset, timeout.
    Transport(String),
}

impl Outcome {
    /// Whether it counts as an error of aledgent's (not a refusal of the caller).
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Outcome::DecisionUnavailable | Outcome::ServerError(_) | Outcome::Transport(_)
        )
    }

    fn label(&self) -> String {
        match self {
            Outcome::Acknowledged => "acknowledged".into(),
            Outcome::Ok => "ok".into(),
            Outcome::Refused(c) => format!("refused:{c}"),
            Outcome::DecisionUnavailable => "ERR-DecisionUnavailable".into(),
            Outcome::ServerError(c) => format!("server:{c}"),
            Outcome::Transport(c) => format!("transport:{c}"),
        }
    }
}

/// One request.
#[derive(Clone, Debug)]
pub struct Sample {
    /// Its phase.
    pub phase: usize,
    /// What it was.
    pub kind: Kind,
    /// How it ended.
    pub outcome: Outcome,
    /// Client-side reply latency, µs.
    pub us: u64,
    /// When it was sent, ms since the run started.
    pub at_ms: u64,
    /// How the caller authenticated: `operator` (a configured token), `api_key` (read from the
    /// core on every request), `session` (a passkey session, checked at ingress) or `none`.
    pub auth: &'static str,
}

/// How a bearer token authenticates, by its form (`authn::token`).
pub fn auth_of(token: &str) -> &'static str {
    if token.starts_with(authn::token::API_KEY_PREFIX) {
        "api_key"
    } else if token.starts_with(authn::token::SESSION_PREFIX) {
        "session"
    } else {
        "operator"
    }
}

/// A completion: from the acknowledgement until the operation left `Received`, sampled by
/// polling (`NFR-15` measured from outside; ingress's own series is the authority).
#[derive(Clone, Debug)]
pub struct Completion {
    /// Its phase (the one it was sent in).
    pub phase: usize,
    /// What it was.
    pub kind: Kind,
    /// µs from the acknowledgement until a read showed it decided.
    pub us: u64,
    /// The state it reached.
    pub state: String,
}

/// One scrape of ingress's metrics and the processes' memory.
#[derive(Clone, Debug, Default)]
pub struct Scrape {
    /// ms since the run started.
    pub at_ms: u64,
    /// The metrics this harness reads, by name.
    pub metrics: BTreeMap<String, u64>,
    /// Resident memory by process label, KiB.
    pub rss_kib: BTreeMap<String, u64>,
}

impl Scrape {
    fn get(&self, name: &str) -> u64 {
        self.metrics.get(name).copied().unwrap_or(0)
    }

    /// Commands taken in and not yet decided. Ingress counts a decision for every core input it
    /// forwards: commands, ticks and recorded deliveries; so the backlog is commands, ticks and
    /// deliveries less decisions.
    pub fn backlog(&self) -> Option<u64> {
        let c = self.metrics.get("aledgent_ingress_commands_total")?;
        Some(
            (c + self.get("aledgent_ingress_ticks_total")
                + self.get("aledgent_ingress_notifications_delivered_total"))
            .saturating_sub(self.get("aledgent_ingress_decisions_total")),
        )
    }

    /// Commands decided so far: decisions less ticks and recorded deliveries.
    pub fn command_decisions(&self) -> Option<u64> {
        let d = self.metrics.get("aledgent_ingress_decisions_total")?;
        Some(d.saturating_sub(
            self.get("aledgent_ingress_ticks_total")
                + self.get("aledgent_ingress_notifications_delivered_total"),
        ))
    }
}

/// The series of ingress's metrics this harness keeps.
pub const SERIES: &[&str] = &[
    "aledgent_ingress_commands_total",
    "aledgent_ingress_decisions_total",
    "aledgent_ingress_ticks_total",
    "aledgent_ingress_notifications_delivered_total",
    "aledgent_ingress_delivery_failures_total",
    "aledgent_ingress_repeats_total",
    "aledgent_ingress_forward_retries_total",
    "aledgent_ingress_decisions_unavailable_total",
    "aledgent_ingress_oldest_undecided_age_ms",
    "aledgent_ingress_notifications_undelivered",
    "aledgent_ingress_oldest_undelivered_age_ms",
    "aledgent_ingress_ticker_age_ms",
    "aledgent_ingress_acknowledgement_p50_us",
    "aledgent_ingress_acknowledgement_p99_us",
    "aledgent_ingress_decided_reply_p50_us",
    "aledgent_ingress_decided_reply_p99_us",
    "aledgent_completion_p50_ms",
    "aledgent_completion_p99_ms",
    "aledgent_ingress_reservation_p99_us",
    "aledgent_ingress_ack_auth_p50_us",
    "aledgent_ingress_ack_auth_p99_us",
    "aledgent_ingress_ack_party_p50_us",
    "aledgent_ingress_ack_party_p99_us",
    "aledgent_ingress_ack_answering_p50_us",
    "aledgent_ingress_ack_answering_p99_us",
    "aledgent_ingress_ack_append_p50_us",
    "aledgent_ingress_ack_append_p99_us",
];

/// Parse the series in [`SERIES`] out of a Prometheus text page. A value that is not a
/// non-negative integer is skipped.
pub fn parse_metrics(text: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(name), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let bare = name.split('{').next().unwrap_or(name);
        if SERIES.contains(&bare) {
            if let Ok(v) = value.parse::<u64>() {
                *out.entry(bare.to_owned()).or_insert(0) += v;
            }
        }
    }
    out
}

/// Everything a run recorded.
pub struct Recorder {
    /// When the run started.
    pub start: Instant,
    samples: Mutex<Vec<Sample>>,
    completions: Mutex<Vec<Completion>>,
    scrapes: Mutex<Vec<Scrape>>,
    notes: Mutex<Vec<(u64, String)>>,
    /// Flows the harness could not start because too many were in flight (its own limit).
    pub dropped: std::sync::atomic::AtomicU64,
    /// Acknowledged commands whose completion was not sampled (the poller's limit).
    pub unsampled: std::sync::atomic::AtomicU64,
    /// Reservations never seen `Reserved` within the flow's patience, so not captured or
    /// released by it.
    pub abandoned: std::sync::atomic::AtomicU64,
    /// Every operation reference acknowledged, for the loss check.
    pub acknowledged: Mutex<Vec<String>>,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder {
            start: Instant::now(),
            samples: Mutex::default(),
            completions: Mutex::default(),
            scrapes: Mutex::default(),
            notes: Mutex::default(),
            dropped: Default::default(),
            unsampled: Default::default(),
            abandoned: Default::default(),
            acknowledged: Mutex::default(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Recorder {
    /// ms since the start.
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Keep a sample.
    pub fn sample(&self, s: Sample) {
        lock(&self.samples).push(s);
    }

    /// Keep a completion.
    pub fn completion(&self, c: Completion) {
        lock(&self.completions).push(c);
    }

    /// Keep a scrape.
    pub fn scrape(&self, s: Scrape) {
        lock(&self.scrapes).push(s);
    }

    /// A line on the run's timeline (a phase starting, a fault injected).
    pub fn note(&self, text: impl Into<String>) {
        let at = self.now_ms();
        let text = text.into();
        println!("[stress] +{}s {text}", at / 1000);
        lock(&self.notes).push((at, text));
    }

    /// A copy of the samples.
    pub fn samples(&self) -> Vec<Sample> {
        lock(&self.samples).clone()
    }

    /// A copy of the completions.
    pub fn completions(&self) -> Vec<Completion> {
        lock(&self.completions).clone()
    }

    /// A copy of the scrapes.
    pub fn scrapes(&self) -> Vec<Scrape> {
        lock(&self.scrapes).clone()
    }

    /// A copy of the timeline.
    pub fn notes(&self) -> Vec<(u64, String)> {
        lock(&self.notes).clone()
    }
}

/// p50, p95, p99 and max of `v` by nearest rank, and the count. Empty: all zero.
pub fn percentiles(mut v: Vec<u64>) -> Value {
    v.sort_unstable();
    let n = v.len();
    let at = |p: usize| -> u64 {
        if n == 0 {
            return 0;
        }
        // Nearest rank: the smallest value with at least p % of the samples at or below it.
        let rank = (p * n).div_ceil(100).max(1);
        v[rank - 1]
    };
    json!({
        "n": n,
        "p50": at(50),
        "p95": at(95),
        "p99": at(99),
        "max": v.last().copied().unwrap_or(0),
    })
}

/// The per-phase numbers of a run, and the time series. `phases` are the plan's (name, offered
/// commands per minute, start ms, end ms).
pub fn summarise(rec: &Recorder, phases: &[(String, u64, u64, u64)]) -> Value {
    let samples = rec.samples();
    let completions = rec.completions();
    let scrapes = rec.scrapes();
    let mut out = Vec::new();
    for (i, (name, offered, from, to)) in phases.iter().enumerate() {
        let mine: Vec<&Sample> = samples.iter().filter(|s| s.phase == i).collect();
        let secs = (to.saturating_sub(*from) / 1000).max(1);
        let commands: Vec<&&Sample> = mine.iter().filter(|s| s.kind.command()).collect();
        let ack: Vec<u64> = mine
            .iter()
            .filter(|s| s.outcome == Outcome::Acknowledged)
            .map(|s| s.us)
            .collect();
        let decided: Vec<u64> = mine
            .iter()
            .filter(|s| s.kind.command() && !s.kind.acknowledged())
            .filter(|s| matches!(s.outcome, Outcome::Ok | Outcome::DecisionUnavailable))
            .map(|s| s.us)
            .collect();
        let mut ack_by_auth: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for s in mine.iter().filter(|s| s.outcome == Outcome::Acknowledged) {
            ack_by_auth.entry(s.auth).or_default().push(s.us);
        }
        let ack_by_auth: BTreeMap<&str, Value> = ack_by_auth
            .into_iter()
            .map(|(k, v)| (k, percentiles(v)))
            .collect();
        let mut reply_by_auth: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        // Acknowledgements and reads: the replies that wait for nothing but authentication and
        // the received-log (or the read).
        for s in mine.iter().filter(|s| {
            s.outcome == Outcome::Acknowledged || (s.kind == Kind::Read && s.outcome == Outcome::Ok)
        }) {
            reply_by_auth.entry(s.auth).or_default().push(s.us);
        }
        let reply_by_auth: BTreeMap<&str, Value> = reply_by_auth
            .into_iter()
            .map(|(k, v)| (k, percentiles(v)))
            .collect();
        let reads: Vec<u64> = mine
            .iter()
            .filter(|s| s.kind == Kind::Read && s.outcome == Outcome::Ok)
            .map(|s| s.us)
            .collect();
        let sign_in: Vec<u64> = mine
            .iter()
            .filter(|s| s.kind == Kind::SignIn && s.outcome == Outcome::Ok)
            .map(|s| s.us)
            .collect();
        let done: Vec<u64> = completions
            .iter()
            .filter(|c| c.phase == i)
            .map(|c| c.us)
            .collect();
        let mut completion_by_kind: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        let mut completion_states: BTreeMap<String, u64> = BTreeMap::new();
        for c in completions.iter().filter(|c| c.phase == i) {
            completion_by_kind
                .entry(c.kind.name())
                .or_default()
                .push(c.us);
            *completion_states.entry(c.state.clone()).or_insert(0) += 1;
        }
        let completion_by_kind: BTreeMap<&str, Value> = completion_by_kind
            .into_iter()
            .map(|(k, v)| (k, percentiles(v)))
            .collect();
        let errors = mine.iter().filter(|s| s.outcome.is_error()).count();
        let mut by_outcome: BTreeMap<String, u64> = BTreeMap::new();
        for s in &mine {
            *by_outcome.entry(s.outcome.label()).or_insert(0) += 1;
        }
        let mut by_kind: BTreeMap<&str, u64> = BTreeMap::new();
        for s in &mine {
            *by_kind.entry(s.kind.name()).or_insert(0) += 1;
        }
        let window: Vec<&Scrape> = scrapes
            .iter()
            .filter(|s| s.at_ms >= *from && s.at_ms <= *to)
            .collect();
        let max_backlog = window.iter().filter_map(|s| s.backlog()).max().unwrap_or(0);
        let max_oldest = window
            .iter()
            .filter_map(|s| s.metrics.get("aledgent_ingress_oldest_undecided_age_ms"))
            .max()
            .copied()
            .unwrap_or(0);
        // Commands decided per minute over the phase, from ingress's own counters.
        let first = window
            .first()
            .and_then(|s| s.command_decisions().map(|d| (s.at_ms, d)));
        let last = window
            .last()
            .and_then(|s| s.command_decisions().map(|d| (s.at_ms, d)));
        let end_backlog = window.last().and_then(|s| s.backlog()).unwrap_or(0);
        let decisions_per_min = match (first, last) {
            (Some((t0, d0)), Some((t1, d1))) if t1 > t0 => {
                d1.saturating_sub(d0).saturating_mul(60_000) / (t1 - t0)
            }
            _ => 0,
        };
        let server = window.last().map(|s| s.metrics.clone()).unwrap_or_default();
        let n = mine.len() as u64;
        out.push(json!({
            "phase": name,
            "seconds": secs,
            "offered_commands_per_min": offered,
            "sent_commands": commands.len(),
            "sent_commands_per_min": (commands.len() as u64).saturating_mul(60) / secs,
            "decisions_per_min": decisions_per_min,
            "requests": n,
            "errors": errors,
            "error_permille": (errors as u64).saturating_mul(1000).checked_div(n).unwrap_or(0),
            "by_outcome": by_outcome,
            "by_kind": by_kind,
            "acknowledgement_us": percentiles(ack),
            "acknowledgement_us_by_auth": ack_by_auth,
            "reply_us_by_auth": reply_by_auth,
            "decided_reply_us": percentiles(decided),
            "completion_us": percentiles(done),
            "completion_us_by_kind": completion_by_kind,
            "completion_states": completion_states,
            "read_us": percentiles(reads),
            "sign_in_us": percentiles(sign_in),
            "max_backlog": max_backlog,
            "end_backlog": end_backlog,
            "max_oldest_undecided_ms": max_oldest,
            "ingress_metrics_at_end": server,
        }));
    }
    // A time series in 10 s buckets: requests, errors, acknowledgement p99, backlog, memory.
    let end = rec.now_ms();
    let mut series = Vec::new();
    let mut t = 0;
    while t < end {
        let b: Vec<&Sample> = samples
            .iter()
            .filter(|s| s.at_ms >= t && s.at_ms < t + 10_000)
            .collect();
        let acks: Vec<u64> = b
            .iter()
            .filter(|s| s.outcome == Outcome::Acknowledged)
            .map(|s| s.us)
            .collect();
        let sc = scrapes
            .iter()
            .rfind(|s| s.at_ms >= t && s.at_ms < t + 10_000);
        series.push(json!({
            "t_s": t / 1000,
            "requests": b.len(),
            "errors": b.iter().filter(|s| s.outcome.is_error()).count(),
            "ack_p99_us": percentiles(acks)["p99"],
            "backlog": sc.and_then(|s| s.backlog()),
            "oldest_undecided_ms": sc.and_then(|s| s.metrics.get("aledgent_ingress_oldest_undecided_age_ms").copied()),
            "rss_kib": sc.map(|s| s.rss_kib.clone()),
        }));
        t += 10_000;
    }
    // Memory: first, max and last per process.
    let mut memory: BTreeMap<String, Value> = BTreeMap::new();
    let labels: Vec<String> = scrapes
        .iter()
        .flat_map(|s| s.rss_kib.keys().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for l in labels {
        let v: Vec<(u64, u64)> = scrapes
            .iter()
            .filter_map(|s| s.rss_kib.get(&l).map(|r| (s.at_ms, *r)))
            .collect();
        let (t0, first) = v.first().copied().unwrap_or((0, 0));
        let (t1, last) = v.last().copied().unwrap_or((0, 0));
        let max = v.iter().map(|x| x.1).max().unwrap_or(0);
        let per_hour = if t1 > t0 {
            i128::from(last).saturating_sub(i128::from(first)) * 3_600_000 / i128::from(t1 - t0)
        } else {
            0
        };
        memory.insert(
            l,
            json!({ "first_kib": first, "max_kib": max, "last_kib": last, "growth_kib_per_hour": per_hour.to_string() }),
        );
    }
    json!({
        "phases": out,
        "series": series,
        "memory": memory,
        "dropped_flows": rec.dropped.load(std::sync::atomic::Ordering::Relaxed),
        "unsampled_completions": rec.unsampled.load(std::sync::atomic::Ordering::Relaxed),
        "abandoned_reservations": rec.abandoned.load(std::sync::atomic::Ordering::Relaxed),
        "timeline": rec.notes().into_iter().map(|(t, s)| json!({"t_ms": t, "note": s})).collect::<Vec<_>>(),
    })
}

/// The run's verdict against the bounds, one line per check: (what, bound, measured, pass).
pub fn verdict(summary: &Value) -> Vec<(String, String, String, bool)> {
    let mut out = Vec::new();
    for p in summary["phases"].as_array().into_iter().flatten() {
        let name = p["phase"].as_str().unwrap_or("?");
        // The bounds are measured inside aledgent (`SPEC.md` §11): ingress's own series, read at
        // the phase's end (its window is the latest 10 000 samples). The client's view, which
        // adds the network and HTTP, is shown beside it.
        let server = &p["ingress_metrics_at_end"];
        let client = |k: &str| p[k]["p99"].as_u64().unwrap_or(0);
        let has = |k: &str| p[k]["n"].as_u64().unwrap_or(0) > 0;
        if has("acknowledgement_us") {
            // Ingress's own series starts after authentication (`surface::command_reply`), so it
            // leaves out an API key's read from the core; on loopback the client's view is the
            // honest one, and it decides.
            let v = server["aledgent_ingress_acknowledgement_p99_us"].as_u64();
            let c = client("acknowledgement_us");
            out.push((
                format!("{name}: NFR-1 acknowledgement p99"),
                "≤ 50 ms (client on loopback; ingress's series excludes authentication)".into(),
                format!(
                    "{} ms at the client; {} ms in ingress's series",
                    c / 1000,
                    v.map_or("?".into(), |u| (u / 1000).to_string()),
                ),
                c <= 50_000,
            ));
        }
        if has("decided_reply_us") {
            let v = server["aledgent_ingress_decided_reply_p99_us"].as_u64();
            out.push((
                format!("{name}: NFR-16 decided reply p99"),
                "≤ 1 s inside aledgent".into(),
                format!(
                    "{} ms inside; {} ms at the client",
                    v.map_or("?".into(), |u| (u / 1000).to_string()),
                    client("decided_reply_us") / 1000
                ),
                v.unwrap_or(client("decided_reply_us")) <= 1_000_000,
            ));
        }
        if has("completion_us") || server["aledgent_completion_p99_ms"].as_u64().is_some() {
            let v = server["aledgent_completion_p99_ms"].as_u64();
            out.push((
                format!("{name}: NFR-15 completion p99"),
                "≤ 30 s (target 19 s) inside aledgent".into(),
                format!(
                    "{} ms inside; {} ms sampled at the client",
                    v.map_or("?".into(), |m| m.to_string()),
                    client("completion_us") / 1000
                ),
                v.map_or(client("completion_us") <= 30_000_000, |m| m <= 30_000),
            ));
        }
        let e = p["error_permille"].as_u64().unwrap_or(0);
        out.push((
            format!("{name}: errors (5xx, transport, ERR-DecisionUnavailable)"),
            "≤ 1 ‰".into(),
            format!("{e} ‰ ({} of {})", p["errors"], p["requests"]),
            e <= 1,
        ));
        // Keeping up: at the phase's end, no more than ten seconds of its offered load is
        // waiting for a decision.
        let offered = p["offered_commands_per_min"].as_u64().unwrap_or(0);
        let decided = p["decisions_per_min"].as_u64().unwrap_or(0);
        let backlog = p["end_backlog"].as_u64().unwrap_or(0);
        let allowed = (offered / 6).max(10);
        out.push((
            format!("{name}: the core keeps up"),
            format!("backlog at the end ≤ 10 s of offered load ({allowed})"),
            format!("{backlog} waiting; {decided} decided/min against {offered} offered"),
            backlog <= allowed,
        ));
    }
    out
}
