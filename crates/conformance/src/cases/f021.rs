// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-021` — the Shared (replicated) core: ephemeral horizontal scaling (amendment 0016, `US-K6`,
//! `NFR-21`). Its registered cases (`F-015` §6, §7).
//!
//! All three are pending. They need a deployment of several execution nodes under load: nodes
//! added, a node replaced by one with no local state, and one node's storage destroyed at an
//! arbitrary instant. That waits for membership change and portable checkpoints (`DEP-11`).

use crate::case::{Case, Feature, Layer};

/// `F-021`'s table.
pub const CASES: &[Case] = &[
    Case::pending(
        "AC-K6.1",
        "ac_k6_1_adding_execution_nodes_changes_nothing_in_the_book__service",
        Layer::Service,
        Feature::F021,
        &["NFR-21", "NFR-17", "NFR-18", "REQ-RT-9"],
    ),
    Case::pending(
        "AC-K6.2",
        "ac_k6_2_a_node_with_no_local_state_replaces_any_node_online__service",
        Layer::Service,
        Feature::F021,
        &["NFR-21", "NFR-4", "NFR-5"],
    ),
    Case::pending(
        "AC-K6.3",
        "ac_k6_3_losing_one_nodes_storage_loses_no_operation__service",
        Layer::Service,
        Feature::F021,
        &["NFR-21", "NFR-4", "INV-6"],
    ),
];
