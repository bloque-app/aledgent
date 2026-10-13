// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # conformance
//!
//! aledgent's conformance suite (`F-015`, `NFR-14`). One suite names a case
//! for every invariant of SPEC §9, every error of §10 and every acceptance
//! criterion of the V0 stories; runs each case at the layer §9 names
//! (`REQ-AR-4`); and writes the assurance declaration from its own results
//! (`REQ-AR-5`, `REQ-AR-6`), so no claim can exceed what passed.
//!
//! ## Map
//!
//! - [`spec`]: the catalogue generated from `SPEC.md` (`cargo xtask
//!   spec-ids`), and the floor of `REQ-AR-6`.
//! - [`case`]: [`case::Case`], its layer, owner, port, kind and outcome.
//! - [`cases`]: the registry, one table per owning feature. `F-001`'s kernel
//!   and store cases and `F-003`'s service and service-store cases run; the
//!   rest are registered as pending on their owner.
//! - [`harness`]: the [`harness::Harness`] trait, its ports, and the
//!   in-process harness over `F-001`'s two stores.
//! - [`closure`]: the closure checks, including the two-way check of
//!   `ledger-core`'s closed `Refusal` list against the catalogue.
//! - [`shape`]: the refusal-shape assertion every `ERR-*` case uses.
//! - [`gates`]: the gate profiles of `gates.toml`.
//! - [`runner`]: the custom test runner and its report.
//! - [`report`]: the run report, `report.json`.
//! - [`assurance`]: the declaration, its floor, baseline and verify rules.
//! - [`tools`]: `proptest` strategies, the plaintext `ShadowBook`, and
//!   `rederive` over the delta log.
//!
//! ## Running it
//!
//! ```text
//! cargo test -p conformance                          # the current gate profile
//! CONFORMANCE_PROFILE=full cargo test -p conformance # everything: red until V0
//! cargo test -p conformance --test suite -- --list   # every case, by id
//! cargo xtask spec-ids --check                       # the catalogue is current
//! cargo run -p conformance --bin assurance -- --report <r> --target in-process
//! ```

pub mod agent;
pub mod assurance;
pub mod case;
pub mod cases;
pub mod closure;
pub mod custody;
pub mod gates;
pub mod harness;
pub mod ops;
pub mod report;
pub mod runner;
pub mod shape;
pub mod spec;
pub mod tools;
