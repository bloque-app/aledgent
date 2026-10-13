// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # ops
//!
//! aledgent's operations (`F-018`, `DEC-10`, `DEC-14`). Operators can show three things at any
//! time, and this crate is how:
//!
//! 1. **A backup restores** to a book that re-derives every balance from its entries and has lost
//!    no acknowledged operation (`NFR-4`, `NFR-10`, `AC-K5.1`): [`backup`] takes every durable
//!    store's backup with a verified checksum, [`restore`] is the procedure, and [`drill`] runs
//!    it on an isolated host with its five checks and records the result.
//! 2. **The signals of `NFR-11` are visible and page someone** when they cross a line:
//!    [`signals`] and [`exporter`] scrape them, `ops/prometheus/` and `ops/alertmanager/` hold the
//!    rules and routes, `ops/grafana/` the dashboards, and [`alerts`] evaluates those very files
//!    in the tests that push each signal over its line.
//! 3. **The book is reconciled** on `NFR-9`'s axes at one minor unit with zero tolerance:
//!    [`reconcile`], axis 3 through `F-017`'s settlement comparison, with the `Suspense` report;
//!    every run recorded as JSON and on the audit trail.
//!
//! Everything here reads. Nothing changes storage outside the command surface (`REQ-AT-3`): a
//! correction is an operation through `CTR-OPS`, and a restore writes only into an empty
//! deployment, through `F-014`'s import.
//!
//! **The core** is `F-009`'s in-process core (`ingress::inprocess::InProcessCore`), or `F-005`'s
//! core Agent. The Agent core's backup is continuous: its host streams every decision to the
//! off-node archive (`core_archive`, `T-005-15`), whose status the [`exporter`] reads; [`agent`]
//! restores a fresh node from it and replays ingress's received-log after its point
//! (`T-005-16`). Nothing here links VOS: the step that re-sends archived inputs to a fresh Agent
//! is `aledgent-core-host restore`, a process of its own.

pub mod agent;
pub mod alerts;
pub mod backup;
pub mod checksum;
pub mod drill;
pub mod error;
pub mod exporter;
pub mod kit;
pub mod reconcile;
pub mod restore;
pub mod seal;
pub mod signals;

pub use error::Error;
