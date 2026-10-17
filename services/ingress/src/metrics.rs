// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! This unit's metrics (`T-009-10`): what `F-018` scrapes, dashboards and pages on.
//!
//! - the age of the oldest undecided row of the received-log, against the decision bound
//!   (`REQ-CS-7`);
//! - the age of the last tick the core applied (`RISK-7`);
//! - undelivered notifications and the oldest one's age (`REQ-EV-6`);
//! - the acknowledgement of a command that moves value, p50 and p99: from arrival until the
//!   command is durably taken in (`NFR-1`, 50 ms at p99; amendments 0007 and 0008);
//! - the decided reply of every other command, account and security commands included, p50 and
//!   p99: from arrival until the reply that carries its decision, or `ERR-DecisionUnavailable`
//!   (`NFR-16`, 1 s at p99 proposed; amendment 0008);
//! - completion, p50 and p99: from receipt until the command's decision completes it, that is a
//!   refusal, a terminal state, or `Reserved` for a reservation (`NFR-15`: ceiling 30 s, target
//!   19 s, at p99; `F-018` pages on the ceiling). A command with an external leg completes later,
//!   at `Settled` or `Released`: ingress does not yet measure that part (`F-009` NOTES);
//! - the reservation path's reply latency, p50 and p99: part of completion, with no budget of its
//!   own (amendment 0007);
//! - counters of commands, decisions, forwarder retries, deliveries, failed attempts, and
//!   commands answered `ERR-DecisionUnavailable`.
//!
//! Integers only: no floating point anywhere (`INV-7`). Latencies are microseconds.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// How many samples of each series are kept for the percentiles.
const SAMPLES: usize = 10_000;

/// The latest [`SAMPLES`] values of one series.
#[derive(Default)]
struct Series(Mutex<VecDeque<u64>>);

impl Series {
    fn push(&self, v: u64) {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if s.len() == SAMPLES {
            s.pop_front();
        }
        s.push_back(v);
    }

    fn percentiles(&self) -> Latency {
        let mut v: Vec<u64> = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .copied()
            .collect();
        if v.is_empty() {
            return Latency::default();
        }
        v.sort_unstable();
        let at = |pct: usize| v[(v.len() * pct).div_ceil(100).saturating_sub(1)];
        Latency {
            samples: v.len() as u64,
            p50_us: at(50),
            p99_us: at(99),
        }
    }
}

/// The metrics of one replica.
#[derive(Default)]
pub struct Metrics {
    /// Commands logged.
    pub commands: AtomicU64,
    /// Repeats answered from the log.
    pub repeats: AtomicU64,
    /// Rows decided by the core.
    pub decisions: AtomicU64,
    /// Forwarder calls retried after an unavailable or ambiguous core.
    pub forward_retries: AtomicU64,
    /// Ticks logged.
    pub ticks: AtomicU64,
    /// Ticks the ticker skipped: nothing was due (`Core::next_due`).
    pub ticks_skipped: AtomicU64,
    /// Notifications delivered (2xx from the recipient).
    pub delivered: AtomicU64,
    /// Delivery attempts that failed or could not be made.
    pub delivery_failures: AtomicU64,
    /// Notifications passed over as parked: no recipient, no secret, or out of attempts.
    pub delivery_parked: AtomicU64,
    /// Undelivered notifications, as of the delivery worker's last pass.
    pub undelivered: AtomicU64,
    /// The oldest undelivered notification's age, in milliseconds, as of the last pass.
    pub oldest_undelivered_ms: AtomicU64,
    /// Commands answered `ERR-DecisionUnavailable` (`REQ-CS-7`).
    pub decisions_unavailable: AtomicU64,
    /// Credentials checked against the durable verifier copy because no core answered (`F-038`
    /// §7, `DEC-27` (e″)).
    pub verifier_copy_used: AtomicU64,
    /// Reply stamps not written to the received-log: the queue was full or the store refused the
    /// batch (`T-009-25`). Each is a row whose `replied_at` stays NULL.
    pub reply_stamps_dropped: AtomicU64,
    reservation_us: Series,
    acknowledgement_us: Series,
    decided_us: Series,
    completion_ms: Series,
    /// The acknowledgement's parts (`T-032-11`, stress finding F14), in order: authentication,
    /// the integrating party's lookup, the answering setting's lookup, and the received-log's
    /// commit; what is left of the acknowledgement is parsing and the reply.
    ack_auth_us: Series,
    ack_party_us: Series,
    ack_answering_us: Series,
    ack_append_us: Series,
}

/// One acknowledgement's parts, microseconds (`Metrics::ack_parts`).
#[derive(Clone, Copy, Debug, Default)]
pub struct AckParts {
    /// Authentication.
    pub auth: u64,
    /// The integrating party's lookup (`REQ-OP-10`).
    pub party: u64,
    /// The answering setting's lookup (`REQ-CS-9`).
    pub answering: u64,
    /// The received-log's commit (`REQ-AR-7`).
    pub append: u64,
}

/// Percentiles of one series. For completion the unit is milliseconds, not microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Latency {
    /// How many samples.
    pub samples: u64,
    /// The median.
    pub p50_us: u64,
    /// The 99th percentile.
    pub p99_us: u64,
}

impl Metrics {
    /// Add one to a counter.
    pub fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one reservation-path latency, from the request's arrival to its reply.
    pub fn reservation(&self, micros: u64) {
        self.reservation_us.push(micros);
    }

    /// The reservation path's percentiles, microseconds.
    pub fn latency(&self) -> Latency {
        self.reservation_us.percentiles()
    }

    /// Record one acknowledgement of a command that moves value, from the request's arrival until
    /// the command is durably taken in (`NFR-1`).
    pub fn acknowledgement(&self, micros: u64) {
        self.acknowledgement_us.push(micros);
    }

    /// The acknowledgement's percentiles, microseconds (`NFR-1`).
    pub fn acknowledgements(&self) -> Latency {
        self.acknowledgement_us.percentiles()
    }

    /// Record one acknowledgement's parts (`T-032-11`).
    pub fn ack_parts(&self, p: AckParts) {
        self.ack_auth_us.push(p.auth);
        self.ack_party_us.push(p.party);
        self.ack_answering_us.push(p.answering);
        self.ack_append_us.push(p.append);
    }

    /// Record one reply to a command answered with its decision, from the request's arrival until
    /// the reply (`NFR-16`, `REQ-CS-8`).
    pub fn decided(&self, micros: u64) {
        self.decided_us.push(micros);
    }

    /// The decided reply's percentiles, microseconds (`NFR-16`).
    pub fn decided_replies(&self) -> Latency {
        self.decided_us.percentiles()
    }

    /// Record one completion, from receipt until the decision that completes the command
    /// (`NFR-15`).
    pub fn completion(&self, millis: u64) {
        self.completion_ms.push(millis);
    }

    /// Completion's percentiles, milliseconds (`NFR-15`).
    pub fn completions(&self) -> Latency {
        self.completion_ms.percentiles()
    }

    /// The Prometheus text form, with the gauges read at scrape time.
    pub fn render(&self, oldest_undecided_ms: Option<u64>, tick_age_ms: Option<u64>) -> String {
        let mut out = String::new();
        let mut line = |name: &str, help: &str, kind: &str, v: u64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {v}"
            );
        };
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        line(
            "aledgent_ingress_commands_total",
            "Commands logged.",
            "counter",
            get(&self.commands),
        );
        line(
            "aledgent_ingress_repeats_total",
            "Repeats answered from the received-log.",
            "counter",
            get(&self.repeats),
        );
        line(
            "aledgent_ingress_decisions_total",
            "Rows decided by the core.",
            "counter",
            get(&self.decisions),
        );
        line(
            "aledgent_ingress_forward_retries_total",
            "Forwarder calls retried.",
            "counter",
            get(&self.forward_retries),
        );
        line(
            "aledgent_ingress_ticks_total",
            "Ticks logged.",
            "counter",
            get(&self.ticks),
        );
        line(
            "aledgent_ingress_ticks_skipped_total",
            "Ticks skipped: nothing was due.",
            "counter",
            get(&self.ticks_skipped),
        );
        line(
            "aledgent_ingress_notifications_delivered_total",
            "Notifications delivered.",
            "counter",
            get(&self.delivered),
        );
        line(
            "aledgent_ingress_delivery_failures_total",
            "Delivery attempts that failed or could not be made.",
            "counter",
            get(&self.delivery_failures),
        );
        line(
            "aledgent_ingress_delivery_parked_total",
            "Notifications passed over as parked: no recipient, no secret, or out of attempts.",
            "counter",
            get(&self.delivery_parked),
        );
        line(
            "aledgent_ingress_notifications_undelivered",
            "Undelivered notifications (REQ-EV-6).",
            "gauge",
            get(&self.undelivered),
        );
        line(
            "aledgent_ingress_oldest_undelivered_age_ms",
            "Age of the oldest undelivered notification (REQ-EV-6).",
            "gauge",
            get(&self.oldest_undelivered_ms),
        );
        line(
            "aledgent_ingress_oldest_undecided_age_ms",
            "Age of the oldest undecided received command (REQ-CS-7).",
            "gauge",
            oldest_undecided_ms.unwrap_or(0),
        );
        line(
            "aledgent_ingress_ticker_age_ms",
            "Time since the last tick the core applied (RISK-7).",
            "gauge",
            tick_age_ms.unwrap_or(u64::MAX),
        );
        line(
            "aledgent_ingress_decisions_unavailable_total",
            "Commands answered ERR-DecisionUnavailable (REQ-CS-7).",
            "counter",
            get(&self.decisions_unavailable),
        );
        line(
            "aledgent_ingress_verifier_copy_used_total",
            "Credentials checked against the durable verifier copy: no core answered (F-038).",
            "counter",
            get(&self.verifier_copy_used),
        );
        line(
            "aledgent_ingress_reply_stamps_dropped_total",
            "Reply stamps not written to the received-log: replied_at stays NULL (T-009-25).",
            "counter",
            get(&self.reply_stamps_dropped),
        );
        for (part, series) in [
            ("auth", &self.ack_auth_us),
            ("party", &self.ack_party_us),
            ("answering", &self.ack_answering_us),
            ("append", &self.ack_append_us),
        ] {
            let l = series.percentiles();
            for (q, v) in [("p50", l.p50_us), ("p99", l.p99_us)] {
                line(
                    &format!("aledgent_ingress_ack_{part}_{q}_us"),
                    "One part of the acknowledgement (NFR-1), microseconds.",
                    "gauge",
                    v,
                );
            }
        }
        let l = self.latency();
        line(
            "aledgent_ingress_reservation_p50_us",
            "Reservation path, median: part of completion (NFR-15).",
            "gauge",
            l.p50_us,
        );
        line(
            "aledgent_ingress_reservation_p99_us",
            "Reservation path, 99th percentile: part of completion (NFR-15).",
            "gauge",
            l.p99_us,
        );
        let a = self.acknowledgements();
        line(
            "aledgent_ingress_acknowledgement_p50_us",
            "Acknowledgement of a command that moves value, median (NFR-1).",
            "gauge",
            a.p50_us,
        );
        line(
            "aledgent_ingress_acknowledgement_p99_us",
            "Acknowledgement of a command that moves value, 99th percentile (NFR-1: 50 ms).",
            "gauge",
            a.p99_us,
        );
        let d = self.decided_replies();
        line(
            "aledgent_ingress_decided_reply_p50_us",
            "Reply to a command answered with its decision, median (NFR-16).",
            "gauge",
            d.p50_us,
        );
        line(
            "aledgent_ingress_decided_reply_p99_us",
            "Reply to a command answered with its decision, 99th percentile (NFR-16: 1 s, proposed).",
            "gauge",
            d.p99_us,
        );
        let c = self.completions();
        line(
            "aledgent_completion_p50_ms",
            "Completion of a command, median (NFR-15).",
            "gauge",
            c.p50_us,
        );
        line(
            "aledgent_completion_p99_ms",
            "Completion of a command, 99th percentile (NFR-15: ceiling 30 s, target 19 s).",
            "gauge",
            c.p99_us,
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank() {
        let m = Metrics::default();
        for i in 1..=100 {
            m.reservation(i);
        }
        let l = m.latency();
        assert_eq!((l.samples, l.p50_us, l.p99_us), (100, 50, 99));
    }

    /// `NFR-1`, `NFR-15` (amendment 0007), `NFR-16` (amendment 0008): the acknowledgement, the
    /// decided reply and completion are kept and exported beside the reservation path, and the
    /// completion gauge is the one `F-018` pages on.
    #[test]
    fn nfr_1_nfr_15_nfr_16_acknowledgement_decided_reply_and_completion_are_exported() {
        let m = Metrics::default();
        for i in 1..=100 {
            m.acknowledgement(i * 100);
            m.decided(i * 2_000);
            m.completion(i * 300);
        }
        assert_eq!(m.acknowledgements().p99_us, 9_900);
        assert_eq!(m.decided_replies().p99_us, 198_000);
        assert_eq!(m.completions().p99_us, 29_700);
        let text = m.render(None, None);
        assert!(text.contains("aledgent_ingress_acknowledgement_p99_us 9900"));
        assert!(text.contains("aledgent_ingress_decided_reply_p99_us 198000"));
        assert!(text.contains("aledgent_completion_p99_ms 29700"));
        assert!(text.contains("aledgent_ingress_decisions_unavailable_total 0"));
    }
}
