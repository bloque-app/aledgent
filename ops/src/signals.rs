// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The signals of `NFR-11`, and three more, as Prometheus metrics (`T-018-04`).
//!
//! | Signal | Metric | Source |
//! |---|---|---|
//! | Age of the oldest non-terminal operation (`REQ-OP-8`) | `aledgent_oldest_non_terminal_age_ms`; `aledgent_oldest_undecided_row_age_ms` (`REQ-CS-7`) | core; ingress's received-log |
//! | Observation completeness (`REQ-SA-7`, `REQ-IN-5`) | `aledgent_observation_cursor_lag`, `aledgent_observation_gaps_total`, `aledgent_observation_rescan_differences` | adapter |
//! | Undelivered notifications, count and age (`REQ-EV-6`) | `aledgent_undelivered_notifications`, `aledgent_oldest_undelivered_age_ms` | core's outbox |
//! | Anchor lag | `aledgent_anchor_lag_ms`, `aledgent_anchor_in_flight_age_ms` | core (`F-017`'s anchor store) |
//! | Reconciliation outcome per axis (`NFR-9`) | `aledgent_reconciliation_differences`, `aledgent_reconciliation_last_run_age_ms` | reconciliation records |
//! | Liquidity of the positions funding settlement (`REQ-SN-2`) | `aledgent_position`, `aledgent_lane_under_fee_floor`, `aledgent_liquidity_alerts` | adapter |
//! | Ticker age (`DEC-6`, `RISK-7`) | `aledgent_ticker_age_ms` | core |
//! | `Needs-Review` count and oldest (`REQ-OP-6`, `AC-C4.1`) | `aledgent_review_queue`, `aledgent_needs_review`, `aledgent_review_most_overdue_ms`, with the queue's owner | core |
//! | Block production (Kreivo's runtime) | `aledgent_finalized_height`, `aledgent_finalized_stall_ms` | adapter |
//! | Completion (`NFR-15`, amendment 0007): ceiling 30 s, target 19 s, at p99 | `aledgent_completion_p99_ms`, `aledgent_completion_p50_ms` | ingress |
//! | How long a writer has been fenced (`REQ-RT-20`, `T-037-15`): alert past `fence_alert_after`, five minutes by default | `aledgent_writer_fenced_seconds` | core host |
//! | The release's signals (`T-018-10`): the core's halt, the audit trail's shipping, the runtime's Authority backlog, the adapter's service and finality; the sharded profile's lags and R-03's checks of the messages between Agents, deferred ([`RELEASE`], [`DEFERRED`]) | `aledgent_core_halted`, `aledgent_audit_*`, `aledgent_vos_unacknowledged_authority_decisions`, `aledgent_finality_lag_blocks` | core host; adapter |
//!
//! Beside them: anchoring's incidents and the latest settlement comparison (`F-017`), the
//! book's size (`aledgent_book_records`, `aledgent_book_leaves`, `aledgent_book_genesis_records`;
//! `T-018-11`, the trigger of `T-036-17`), the `Suspense` report (`REQ-LA-2`), whether each source
//! answered (`aledgent_signals_up`), the operator channel's incidents, the age of each store's last
//! verified backup, and the last drill's result. Every value is an integer — counts, milliseconds,
//! the layer's integer units — and none is a ledger fact: no account, amount of a customer,
//! identity or operation reference (`REQ-PV-5`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use ingress::query::{BookSizeBody, SignalsBody};
use serde::{Deserialize, Serialize};
use settle::signals::AdapterSignals;

use crate::backup::StoreKind;

/// The signals of `NFR-11` and the three extras, each with the metrics that show it. The rules
/// label each alert with its signal.
pub const SIGNALS: &[(&str, &[&str])] = &[
    (
        "oldest-non-terminal",
        &[
            "aledgent_oldest_non_terminal_age_ms",
            "aledgent_oldest_undecided_row_age_ms",
        ],
    ),
    (
        "observation-completeness",
        &[
            "aledgent_observation_cursor_lag",
            "aledgent_observation_gaps_total",
            "aledgent_observation_rescan_differences",
        ],
    ),
    (
        "undelivered-notifications",
        &[
            "aledgent_undelivered_notifications",
            "aledgent_oldest_undelivered_age_ms",
        ],
    ),
    (
        "anchor-lag",
        &["aledgent_anchor_lag_ms", "aledgent_anchor_in_flight_age_ms"],
    ),
    (
        "reconciliation",
        &[
            "aledgent_reconciliation_differences",
            "aledgent_reconciliation_last_run_age_ms",
        ],
    ),
    (
        "liquidity",
        &[
            "aledgent_position",
            "aledgent_lane_native",
            "aledgent_lane_under_fee_floor",
            "aledgent_liquidity_alerts",
        ],
    ),
    ("ticker", &["aledgent_ticker_age_ms"]),
    (
        "needs-review",
        &[
            "aledgent_review_queue",
            "aledgent_needs_review",
            "aledgent_review_overdue",
            "aledgent_review_most_overdue_ms",
            "aledgent_review_service_level_ms",
        ],
    ),
    (
        "block-production",
        &["aledgent_finalized_height", "aledgent_finalized_stall_ms"],
    ),
    (
        "completion",
        &["aledgent_completion_p99_ms", "aledgent_completion_p50_ms"],
    ),
    // `REQ-RT-20` (amendment 0033): how long a writer has been fenced is one of `NFR-11`'s.
    ("writer-fence", &["aledgent_writer_fenced_seconds"]),
];

/// The kinds of the adapter's alerts (`F-016`), each exported even at zero.
pub const ADAPTER_ALERTS: &[&str] = &[
    "digest", "fees", "gap", "incident", "lane", "position", "refusing", "rescan", "runtime",
];

/// Anchoring's signals beyond the anchor lag (`F-017`), and the `Suspense` report (`REQ-LA-2`).
pub const ANCHORING: &[(&str, &[&str])] = &[
    (
        "anchoring",
        &[
            "aledgent_anchor_incidents",
            "aledgent_settlement_differing_assets",
            "aledgent_settlement_comparison_age_ms",
        ],
    ),
    (
        "suspense",
        &[
            "aledgent_suspense_nonzero",
            "aledgent_suspense_oldest_age_ms",
        ],
    ),
];

/// The kinds of anchoring's incidents (`F-017`), each exported even at zero.
pub const ANCHOR_INCIDENTS: &[&str] = &[
    "commitment_mismatch",
    "unknown_anchor",
    "position_difference",
];

/// The release's signals beyond `NFR-11`'s (`T-018-10`): the core's halt, the
/// audit trail's shipping (`T-013-13`), the runtime's Authority backlog, the sharded profile's lags
/// and fence, the adapter's service and finality. The Agent core's archive has its own
/// (`T-005-15`).
pub const RELEASE: &[(&str, &[&str])] = &[
    ("core-halt", &["aledgent_core_halted"]),
    (
        "audit-shipping",
        &[
            "aledgent_audit_shipping_configured",
            "aledgent_audit_unshipped_records",
            "aledgent_audit_ship_lag_ms",
            "aledgent_audit_shipped_records",
            "aledgent_audit_store_up",
            "aledgent_audit_store_refused",
            "aledgent_audit_backpressure",
        ],
    ),
    (
        "vos-authority",
        &["aledgent_vos_unacknowledged_authority_decisions"],
    ),
    (
        "partition-lag",
        &[
            "aledgent_credit_lag_p99_ms",
            "aledgent_revocation_lag_p99_ms",
            "aledgent_stream_lag_ms",
            "aledgent_replay_fenced",
        ],
    ),
    (
        "settlement-adapter",
        &[
            "aledgent_adapter_serving",
            "aledgent_best_height",
            "aledgent_finality_lag_blocks",
        ],
    ),
    (
        "message-provenance",
        &[
            "aledgent_r03_failing_streams",
            "aledgent_r03_unverified_age_ms",
            "aledgent_r03_checks_total",
            "aledgent_r03_failures_total",
        ],
    ),
];

/// Metrics the rules and dashboards name that nothing exports yet, each with what will:
///
/// - `aledgent_credit_lag_p99_ms` (`NFR-19`), `aledgent_revocation_lag_p99_ms` (`NFR-20`),
///   `aledgent_stream_lag_ms` (`REQ-RT-13`): the sharded profile's host, once ingress routes to
///   shards (`s-04`). With the one core Agent of the MVP a credit is applied in its debit's
///   commit and a revocation takes effect at its commit (ingress's API-key cache is held under
///   `NFR-20`'s bound at start), so neither lag exists to measure;
/// - `aledgent_replay_fenced`: native decide (`F-034`), off in the MVP;
/// - `aledgent_r03_*` (R-03, owner ruling `rq-01`): the receivers' checks of the messages between the
///   book's Agents, which the sharded profile's host counts (`shards::attest::Report`). With the one
///   core Agent no message crosses Agents, so there is nothing to check.
///
/// Their rules are committed, so they page the day the metric appears.
pub const DEFERRED: &[&str] = &[
    "aledgent_credit_lag_p99_ms",
    "aledgent_revocation_lag_p99_ms",
    "aledgent_stream_lag_ms",
    "aledgent_replay_fenced",
    "aledgent_r03_failing_streams",
    "aledgent_r03_unverified_age_ms",
    "aledgent_r03_checks_total",
    "aledgent_r03_failures_total",
];

/// The `Suspense` report of the last reconciliation run (`REQ-LA-2`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspenseSignal {
    /// (account, asset) pairs of `Suspense` accounts not at zero.
    pub nonzero: u64,
    /// How long the oldest of them has not been zero, in milliseconds, at the run.
    pub oldest_age: u64,
}

/// One reconciliation axis's last recorded run (`crate::reconcile`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisSignal {
    /// The axis: `1`, `2` or `3`.
    pub axis: u8,
    /// When it last ran, if ever.
    pub last_run: Option<u64>,
    /// How many differences it found; zero when it passed.
    pub differences: u64,
    /// Whether it could run at all (axis 3 needs a positions report from the adapter).
    pub ran: bool,
}

/// Everything one scrape collected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// When, in milliseconds since the Unix epoch.
    pub at: u64,
    /// The core's and ingress's signals, if the core answered.
    pub core: Option<SignalsBody>,
    /// The adapter's signals, if they could be read, and when they were written.
    pub adapter: Option<AdapterSignals>,
    /// How long the finalized height has not moved, as the exporter has seen it.
    pub finalized_stall: Option<u64>,
    /// The reconciliation's axes.
    pub reconciliation: Vec<AxisSignal>,
    /// The last run's `Suspense` report.
    pub suspense: Option<SuspenseSignal>,
    /// When each store's last verified backup was taken.
    pub backups: BTreeMap<StoreKind, u64>,
    /// The last drill: when it started and whether it passed.
    pub drill: Option<(u64, bool)>,
    /// The Agent core's archiver status (`T-005-15`): `Some(None)` when it is configured and could
    /// not be read, which shows as a status as old as the epoch.
    pub core_archive: Option<Option<core_archive::ArchiveStatus>>,
    /// The review queue's owner, for its label (`REQ-OP-6`).
    pub review_owner: String,
}

/// A Prometheus text exposition, written metric by metric with its help and type.
struct Exposition {
    out: String,
    declared: BTreeMap<&'static str, ()>,
}

impl Exposition {
    fn new() -> Exposition {
        Exposition {
            out: String::new(),
            declared: BTreeMap::new(),
        }
    }

    fn declare(&mut self, name: &'static str, kind: &str, help: &str) {
        if self.declared.insert(name, ()).is_none() {
            let _ = writeln!(self.out, "# HELP {name} {help}");
            let _ = writeln!(self.out, "# TYPE {name} {kind}");
        }
    }

    fn sample(
        &mut self,
        name: &'static str,
        labels: &[(&str, &str)],
        value: impl std::fmt::Display,
    ) {
        let _ = write!(self.out, "{name}");
        if !labels.is_empty() {
            let body: Vec<String> = labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
                .collect();
            let _ = write!(self.out, "{{{}}}", body.join(","));
        }
        let _ = writeln!(self.out, " {value}");
    }

    fn gauge(
        &mut self,
        name: &'static str,
        help: &str,
        labels: &[(&str, &str)],
        value: impl std::fmt::Display,
    ) {
        self.declare(name, "gauge", help);
        self.sample(name, labels, value);
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn flag(b: bool) -> u8 {
    u8::from(b)
}

/// Render a snapshot as a Prometheus text exposition (format 0.0.4).
pub fn render(s: &Snapshot) -> String {
    let mut e = Exposition::new();
    let up = "Whether the source answered this scrape.";
    e.gauge(
        "aledgent_signals_up",
        up,
        &[("source", "core")],
        flag(s.core.is_some()),
    );
    e.gauge(
        "aledgent_signals_up",
        up,
        &[("source", "adapter")],
        flag(s.adapter.is_some()),
    );
    if let Some(c) = &s.core {
        core(&mut e, c, &s.review_owner);
    }
    if let Some(a) = &s.adapter {
        adapter(&mut e, a, s.at);
    }
    if let Some(stall) = s.finalized_stall {
        e.gauge(
            "aledgent_finalized_stall_ms",
            "How long the layer's finalized height has not moved: blocks need a crank when the chain is idle.",
            &[],
            stall,
        );
    }
    for a in &s.reconciliation {
        let axis = a.axis.to_string();
        let l = [("axis", axis.as_str())];
        e.gauge(
            "aledgent_reconciliation_differences",
            "Differences the last reconciliation found on the axis, at one minor unit and zero tolerance (NFR-9).",
            &l,
            a.differences,
        );
        e.gauge(
            "aledgent_reconciliation_ran",
            "Whether the axis could run at all: axis 3 needs a positions report from the adapter.",
            &l,
            flag(a.ran),
        );
        e.gauge(
            "aledgent_reconciliation_last_run_age_ms",
            "Time since the axis last ran; since the epoch when it never has.",
            &l,
            s.at.saturating_sub(a.last_run.unwrap_or(0)),
        );
    }
    if let Some(x) = &s.suspense {
        e.gauge(
            "aledgent_suspense_nonzero",
            "Suspense (account, asset) pairs not at zero at the last reconciliation (REQ-LA-2).",
            &[],
            x.nonzero,
        );
        e.gauge(
            "aledgent_suspense_oldest_age_ms",
            "How long the oldest non-zero Suspense balance has not been zero; zero when none is.",
            &[],
            x.oldest_age,
        );
    }
    for store in StoreKind::ALL {
        let age =
            s.at.saturating_sub(s.backups.get(&store).copied().unwrap_or(0));
        e.gauge(
            "aledgent_backup_last_age_ms",
            "Time since the store's last verified backup; since the epoch when it has none.",
            &[("store", store.as_str())],
            age,
        );
    }
    if let Some(a) = &s.core_archive {
        archive(&mut e, a.as_ref(), s.at);
    }
    if let Some((at, passed)) = s.drill {
        e.gauge(
            "aledgent_drill_last_age_ms",
            "Time since the last restore drill started (NFR-10).",
            &[],
            s.at.saturating_sub(at),
        );
        e.gauge(
            "aledgent_drill_passed",
            "Whether the last restore drill passed.",
            &[],
            flag(passed),
        );
    }
    e.out
}

/// The Agent core's archive (`T-005-15`, `NFR-4`, `NFR-21` (c)): how much a loss of the core's
/// node would lose now, and whether the archiver is alive and writing.
fn archive(e: &mut Exposition, a: Option<&core_archive::ArchiveStatus>, now: u64) {
    e.gauge(
        "aledgent_archive_status_age_ms",
        "Time since the core host's archiver last wrote its status; since the epoch when it cannot be read.",
        &[],
        now.saturating_sub(a.map_or(0, |a| a.at)),
    );
    let Some(a) = a else {
        return;
    };
    e.gauge(
        "aledgent_archive_lag_ms",
        "How long the oldest committed decision not yet in the off-node archive has waited: the core's recovery point if its node were lost now.",
        &[],
        a.lag_ms,
    );
    e.gauge(
        "aledgent_archive_lag_inputs",
        "Committed core inputs not yet in the off-node archive.",
        &[],
        a.lag_inputs,
    );
    e.gauge(
        "aledgent_archive_pending_bytes",
        "Bytes of the core host's logs not yet in the off-node archive.",
        &[],
        a.pending_bytes,
    );
    e.gauge(
        "aledgent_archive_failures",
        "Archive uploads that failed in a row.",
        &[],
        a.failures,
    );
    e.gauge(
        "aledgent_archive_conflict",
        "Whether the archiver refuses to write because the node's logs are not the archive's history (INV-25).",
        &[],
        flag(a.conflict.is_some()),
    );
    e.gauge(
        "aledgent_archive_checkpoint_age_ms",
        "Time since the archive's latest checkpoint of the Agent's runtime state; a restore re-sends every input after it.",
        &[],
        now.saturating_sub(a.checkpoint.as_ref().map_or(0, |c| c.at)),
    );
}

fn anchoring(e: &mut Exposition, c: &SignalsBody) {
    e.gauge(
        "aledgent_anchor_lag_ms",
        "Anchor lag (NFR-11): time since the newest Final checkpoint was created; zero while there is no checkpoint.",
        &[],
        c.anchor_lag.unwrap_or(0),
    );
    e.gauge(
        "aledgent_anchor_in_flight_age_ms",
        "How long the checkpoint in flight has waited for finality; zero when none is in flight.",
        &[],
        c.anchor_in_flight_age.unwrap_or(0),
    );
    let mut kinds: BTreeMap<&str, u64> = ANCHOR_INCIDENTS.iter().map(|k| (*k, 0)).collect();
    for i in &c.anchor_incidents {
        kinds.insert(i.kind.as_str(), i.count);
    }
    for (kind, n) in kinds {
        e.gauge(
            "aledgent_anchor_incidents",
            "Anchoring's incidents (F-017) by kind, ever raised.",
            &[("kind", kind)],
            n,
        );
    }
    e.gauge(
        "aledgent_settlement_differing_assets",
        "Assets whose latest settlement comparison differs by at least one minor unit (REQ-PR-5).",
        &[],
        c.settlement_differing,
    );
    e.gauge(
        "aledgent_settlement_comparison_age_ms",
        "Age of the latest settlement comparison; zero while none was made (the reconciliation's axis 3 pages on that).",
        &[],
        c.settlement_comparison_age.unwrap_or(0),
    );
}

/// The core host's own signals (`T-018-10`): the halt, the audit trail's shipping (`T-013-13`),
/// and the runtime's Authority backlog.
fn host(e: &mut Exposition, c: &SignalsBody) {
    e.gauge(
        "aledgent_writer_fenced_seconds",
        "How long the core's writer has been fenced (T-037-15, REQ-RT-20): the code installed is not the code its state names, so it takes no input until the code map's code is installed or the upgrade is moved or cancelled. 0 when it is not fenced.",
        &[("writer", "core")],
        c.writer_fenced_age.map_or(0, |ms| ms / 1_000),
    );
    e.gauge(
        "aledgent_core_halted",
        "Whether the core has stopped deciding: halted on an input the Agent cannot apply (F18), or stopped on an incident. Every input waits Received.",
        &[],
        flag(c.core_halted),
    );
    e.gauge(
        "aledgent_audit_shipping_configured",
        "Whether the core ships its audit trail to an audit store (T-013-13, REQ-AT-2, NFR-6).",
        &[],
        flag(c.audit.is_some()),
    );
    if let Some(a) = &c.audit {
        e.gauge(
            "aledgent_audit_unshipped_records",
            "Audit records in the core's outbox the audit store has not confirmed.",
            &[],
            a.unshipped,
        );
        e.gauge(
            "aledgent_audit_ship_lag_ms",
            "Age of the oldest audit record the store has not confirmed; zero when none waits.",
            &[],
            a.oldest_unshipped_age.unwrap_or(0),
        );
        e.gauge(
            "aledgent_audit_shipped_records",
            "Audit records the audit store holds.",
            &[],
            a.shipped,
        );
        e.gauge(
            "aledgent_audit_store_up",
            "Whether the last shipping round reached the audit store.",
            &[],
            flag(a.store_up),
        );
        e.gauge(
            "aledgent_audit_store_refused",
            "Whether the audit store refused a record under its rules, or holds another than the core's at its head: shipping has stopped.",
            &[],
            flag(a.refused),
        );
        e.gauge(
            "aledgent_audit_backpressure",
            "Whether the core refuses new inputs because its audit outbox is at its bound.",
            &[],
            flag(a.backpressure),
        );
        e.gauge(
            "aledgent_audit_max_unshipped",
            "The audit outbox's bound, past which the core refuses new inputs.",
            &[],
            a.max_unshipped,
        );
    }
    if let Some(u) = c.vos_unacknowledged {
        e.gauge(
            "aledgent_vos_unacknowledged_authority_decisions",
            "Authority decisions the core host issued for invocations not yet acknowledged; VOS refuses past 256.",
            &[],
            u,
        );
    }
}

fn core(e: &mut Exposition, c: &SignalsBody, owner: &str) {
    anchoring(e, c);
    host(e, c);
    e.gauge(
        "aledgent_operations_open",
        "Non-terminal operations.",
        &[],
        c.open,
    );
    if let Some(b) = &c.book {
        book(e, b);
    }
    e.gauge(
        "aledgent_oldest_non_terminal_age_ms",
        "Age of the oldest non-terminal operation (REQ-OP-8); zero when none is open.",
        &[],
        c.oldest_open_age.unwrap_or(0),
    );
    e.gauge(
        "aledgent_operations_received",
        "Operations in Received in the core.",
        &[],
        c.received,
    );
    e.gauge(
        "aledgent_oldest_undecided_row_age_ms",
        "Age of the oldest row ingress holds undecided (REQ-CS-7); zero when all are decided.",
        &[],
        c.oldest_undecided_age.unwrap_or(0),
    );
    e.gauge(
        "aledgent_completion_p99_ms",
        "Completion of a command at p99 (NFR-15): pages over the 30 s ceiling; the target is 19 s. Zero before the first sample.",
        &[],
        c.completion_p99.unwrap_or(0),
    );
    e.gauge(
        "aledgent_completion_p50_ms",
        "Completion of a command at p50 (NFR-15). Zero before the first sample.",
        &[],
        c.completion_p50.unwrap_or(0),
    );
    e.gauge(
        "aledgent_undelivered_notifications",
        "Undelivered notifications (REQ-EV-6).",
        &[],
        c.undelivered,
    );
    e.gauge(
        "aledgent_oldest_undelivered_age_ms",
        "Age of the oldest undelivered notification; zero when none is owed.",
        &[],
        c.oldest_undelivered_age.unwrap_or(0),
    );
    // Before the first tick the ticker is as old as the deployment: report the read's time.
    e.gauge(
        "aledgent_ticker_age_ms",
        "Time since the core applied its last tick (DEC-6, RISK-7).",
        &[],
        c.tick_age.unwrap_or(c.now),
    );
    let o = [("owner", owner)];
    e.gauge(
        "aledgent_review_queue",
        "Operations in the review queue: Needs-Review, past deadline, flagged (REQ-OP-6).",
        &o,
        c.review,
    );
    e.gauge(
        "aledgent_needs_review",
        "Operations in Needs-Review.",
        &o,
        c.needs_review,
    );
    e.gauge(
        "aledgent_review_overdue",
        "Review-queue entries past their service level.",
        &o,
        c.review_overdue,
    );
    e.gauge(
        "aledgent_review_most_overdue_ms",
        "How far past its service level the most overdue review entry is.",
        &o,
        c.review_most_overdue,
    );
    e.gauge(
        "aledgent_review_service_level_ms",
        "The review queue's service level.",
        &o,
        c.review_service_level,
    );
    for i in &c.incidents {
        let l = [("kind", i.kind.as_str())];
        e.declare(
            "aledgent_incidents_total",
            "counter",
            "Incidents raised on the operator channel (F-013).",
        );
        e.sample("aledgent_incidents_total", &l, i.count);
        e.gauge(
            "aledgent_incident_latest_age_ms",
            "Time since the latest incident of the kind was raised.",
            &l,
            c.now.saturating_sub(i.latest),
        );
    }
}

/// The book's size (`T-018-11`): the trigger of `T-036-17`, the disk-backed service store the
/// owner requires before a book of about three times its genesis is served (ruling `a0022-q5`).
fn book(e: &mut Exposition, b: &BookSizeBody) {
    e.gauge(
        "aledgent_book_records",
        "The book's records, in a chainspec's unit: identities, ledger accounts, grants, aliases and operations, and the seed's opening rows and credential imports (T-018-11). T-036-17 is required before about 3x genesis.",
        &[],
        b.records,
    );
    e.gauge(
        "aledgent_book_leaves",
        "Leaves of the core's service store: what the core host's replica holds in memory, and T-036-17 moves to disk.",
        &[],
        b.leaves,
    );
    if let Some(g) = b.genesis_records {
        e.gauge(
            "aledgent_book_genesis_records",
            "Records of the chainspec the book was founded on (F-036); absent for a book founded without one.",
            &[],
            g,
        );
    }
}

fn adapter(e: &mut Exposition, a: &AdapterSignals, now: u64) {
    e.gauge(
        "aledgent_adapter_serving",
        "Whether the settlement adapter serves.",
        &[],
        flag(a.serving),
    );
    e.gauge(
        "aledgent_adapter_signals_age_ms",
        "Age of the adapter's last written signals.",
        &[],
        now.saturating_sub(a.at),
    );
    e.gauge(
        "aledgent_observation_cursor_lag",
        "Inbox positions the adapter's cursor is behind the finalized inbox (REQ-SA-7).",
        &[],
        a.lag(),
    );
    e.declare(
        "aledgent_observation_gaps_total",
        "counter",
        "Ranges the adapter was forced to skip, ever (REQ-SA-7).",
    );
    e.sample("aledgent_observation_gaps_total", &[], a.gaps);
    let open = |k: &str| a.alerts.get(k).copied().unwrap_or(0);
    e.gauge(
        "aledgent_observation_rescan_differences",
        "Open incidents a re-scan raised: records never ingested or no longer shown (REQ-IN-5).",
        &[],
        open("rescan") + open("incident"),
    );
    for kind in ["position", "fees"] {
        e.gauge(
            "aledgent_liquidity_alerts",
            "Open liquidity or fee shortfalls, one per position (REQ-SN-2).",
            &[("kind", kind)],
            open(kind),
        );
    }
    let mut kinds: BTreeMap<&str, u64> = ADAPTER_ALERTS.iter().map(|k| (*k, 0)).collect();
    for (kind, n) in &a.alerts {
        kinds.insert(kind.as_str(), *n);
    }
    for (kind, n) in kinds {
        e.gauge(
            "aledgent_adapter_alerts",
            "The adapter's open alerts by kind.",
            &[("kind", kind)],
            n,
        );
    }
    for l in &a.lanes {
        let lane = [("lane", l.lane.as_str())];
        e.gauge(
            "aledgent_lane_native",
            "A lane's native balance, in the layer's units: it pays the fees.",
            &lane,
            &l.native,
        );
        e.gauge(
            "aledgent_lane_under_fee_floor",
            "Whether a lane's native balance is under the fee floor (REQ-SN-2).",
            &lane,
            flag(l.under_floor),
        );
    }
    for p in &a.positions {
        e.gauge(
            "aledgent_position",
            "The instance's position per asset, in the layer's units.",
            &[("asset", p.asset.as_str())],
            &p.position,
        );
    }
    e.gauge(
        "aledgent_finalized_height",
        "The settlement layer's finalized height, as the adapter read it.",
        &[],
        a.finalized,
    );
    e.gauge(
        "aledgent_best_height",
        "The settlement layer's best height, as the adapter read it.",
        &[],
        a.best.max(a.finalized),
    );
    e.gauge(
        "aledgent_finality_lag_blocks",
        "Blocks the finalized head is behind the best one: finality stalls while blocks come.",
        &[],
        a.finality_lag(),
    );
}

/// Parse a text exposition into samples: `(name, labels, value)`. Only what [`render`] writes:
/// integer values. For the tests and the alert rules' evaluation.
pub fn parse(text: &str) -> Vec<(String, BTreeMap<String, String>, i128)> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((series, value)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(value) = value.parse::<i128>() else {
            continue;
        };
        let (name, labels) = match series.split_once('{') {
            None => (series.to_owned(), BTreeMap::new()),
            Some((n, rest)) => {
                let body = rest.trim_end_matches('}');
                let mut labels = BTreeMap::new();
                for pair in body.split("\",") {
                    if let Some((k, v)) = pair.split_once("=\"") {
                        labels.insert(k.to_owned(), v.trim_end_matches('"').to_owned());
                    }
                }
                (n.to_owned(), labels)
            }
        };
        out.push((name, labels, value));
    }
    out
}
