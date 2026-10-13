// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `aledgent-settle` (`F-016`, `DEC-8`): the settlement adapter.
//!
//! It is the only component of aledgent that touches the settlement layer (`INV-25`, guardrail
//! 8). It holds the signing keys, apart from the core (`REQ-AR-3`, `NFR-8`); it holds no authority
//! over any ledger account and cannot cause an entry (`REQ-SA-8`). It takes every call from the
//! core's call outbox as an envelope (`F-027`, `DEC-18`) and stores each one before it
//! acknowledges it (`REQ-SO-6`); it executes them in batches on the settlement contract (`F-026`),
//! as the deployment's community PASS account, with an explicit nonce and a mortal era that ends
//! before the earliest expiry, stores the signed bytes, and only then broadcasts them — after a
//! restart the same bytes, never a second transaction (`REQ-SA-6`, `REQ-SO-7`, `REQ-SL-4`). It reads
//! the contract's sequenced inbox with a cursor at the finalized head and reports every entry as an
//! observation, one per reference, never passing a position it did not read and recording any
//! range it was forced to skip (`REQ-SA-7`). It keeps
//! executed, provably not executed and unknown apart (`REQ-SA-4`), and reports observations only
//! (`INV-11`). It parks what liquidity or fees do not cover, with one alert per position, and
//! resumes by itself (`REQ-SN-2`).
//!
//! It reaches the core only through ingress (`F-009`), as the principal the settlement
//! configuration names; the core refuses that principal everything but its three capabilities.
//! Nothing here depends on how the core runs (in-process, or as `F-005`'s core; issue #38 is
//! closed).
//!
//! ## Map
//!
//! - [`adapter`]: the step, and everything it does.
//! - [`chain`]: the settlement layer behind a trait; [`chain::kreivo`], Kreivo's transactions and
//!   the contract's types in plain SCALE; [`chain::fake`] in memory; `chain::rpc` through a light
//!   client (feature `light-client`, the default) or, opted into, a node's RPC.
//! - [`signer`]: the signer boundary; [`signer::PassAccount`]; [`signer::DevSigner`]; [`signer::ServiceSigner`] (the signer service, `F-035`)
//!   (feature `kms`).
//! - [`store`]: the adapter's durable state; [`store::mem`]; `store::pg` (feature `postgres`).
//! - [`link`]: ingress, over HTTP or in process.
//! - [`config`]: the configuration.
//! - [`testkit`]: a whole deployment in process, for the tests and the conformance suite.

pub mod adapter;
pub mod chain;
pub mod config;
pub mod link;
pub mod signals;
pub mod signer;
pub mod store;
pub mod testkit;

pub use adapter::{Adapter, AdapterError, CrashPoint, Refusing, StepReport};
