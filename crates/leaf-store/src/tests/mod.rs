// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Tests of the whole store: both commit paths, the witness, the archive, rebuild, indexes, the
//! openings store, and the flat cost of an apply.

mod exit;
mod fixture;
mod flat;
mod indexes;
mod paths;
mod rules;
mod sealed;
mod service;
// F-006's standalone witness is CipherClerk's v1 format (DEC-25).
#[cfg(feature = "smt-v1")]
mod witness;
