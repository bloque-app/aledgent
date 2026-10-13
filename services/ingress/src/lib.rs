// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # aledgent-ingress (`F-009`)
//!
//! Every command and query reaches aledgent through this one process. It writes each command to a
//! durable received-log before it answers, so a restart of the deciding component loses nothing
//! and refuses nothing (`REQ-AR-7`, `REQ-CS-7`). A transfer and a reservation get a `Received`
//! acknowledgement at once (`REQ-CS-2`, `REQ-CS-3`), and their decision by query and notification. The same
//! process drives deadlines with a ticker (`DEC-6`) and delivers notifications from the core's
//! outbox, recording each delivery (`DEC-7`).
//!
//! ## The boundary, and issue #38
//!
//! Where the reservation decision runs is not ruled (issue #38: a VOS Agent, `F-005`, or a native
//! process). Ingress is needed under both, so it talks to the core through one trait,
//! [`boundary::Core`], and this crate provides one implementation of it,
//! [`inprocess::InProcessCore`], over `ledger-core` directly. The VOS Agent implementation is
//! `F-005`'s.
//!
//! ## Map
//!
//! - [`wire`]: the JSON bodies of commands, replies, queries and notifications.
//! - [`command`]: validating a command, its canonical form, its client key, and its intake.
//! - [`query`]: the wire form of `F-011`'s queries, replies and cursor.
//! - [`pass_through`]: a command's pass-through data (`REQ-PO-5`), kept with its row and echoed
//!   to the sender's integrating party.
//! - [`operator`]: `F-013`'s operator commands, validated and routed on `CTR-OPS`.
//! - [`auth`]: deny-by-default transport authentication; credentials never logged.
//! - [`boundary`]: the ingress-to-core trait and the values that cross it.
//! - [`inprocess`]: the in-process core behind a write-ahead journal.
//! - [`remote`]: the core in another process (`F-005`): the client ingress uses to reach the
//!   core Agent's host, and the server the host answers with.
//! - [`store`]: the received-log, delivery attempts, recipients and leadership; in memory and in
//!   PostgreSQL.
//! - [`service`]: one replica: leadership, the forwarder, the ticker and the delivery worker.
//! - [`delivery`]: signing, the reference verifier, the transport.
//! - [`gate`]: the cores' read-write lock, which lets decisions in before reads (`T-031-12`).
//! - [`surface`]: the HTTP routes.
//! - [`openapi`]: the generated OpenAPI 3.1 document.
//! - [`metrics`]: this unit's metrics.
//! - [`clock`]: the ingress clock.
//! - [`config`]: the deployment's configuration file, and secrets from the environment only.
//! - [`custody`]: custody switching, credential sets and recovery (`F-012`).
//! - [`testkit`]: an in-memory deployment for tests and the conformance suite.

pub mod anchoring;
pub mod answering;
pub mod archived;
pub mod audit_ship;
pub mod auth;
pub mod boundary;
pub mod clock;
pub mod command;
pub mod config;
pub mod credentials;
pub mod custody;
pub mod delivery;
pub mod gate;
pub mod inprocess;
pub mod metrics;
pub mod openapi;
pub mod operator;
pub mod pass_through;
pub mod query;
pub mod remote;
pub mod service;
pub mod settlement;
pub mod store;
pub mod surface;
pub mod testkit;
pub mod wire;
