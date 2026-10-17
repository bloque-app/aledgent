// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-037` — code history and upgrades (amendments 0023 and 0033, `US-K7`, `REQ-RT-15` to
//! `REQ-RT-21`, `REQ-PR-11`, `NFR-22`). Its registered cases (`F-015` §6, §7).
//!
//! All eleven are pending, registered when each amendment was applied, before `F-037` is built
//! (amendment 0013's practice). They need the code version in each writer's state, the upgrade and
//! mode inputs, the code archive, per-range replay, the fence, checkpoints as rows and leaf chunks,
//! and the continuation.

use crate::case::{Case, Feature, Layer};

/// `F-037`'s table.
pub const CASES: &[Case] = &[
    Case::pending(
        "AC-K7.1",
        "ac_k7_1_an_upgrade_applies_forward_and_the_next_anchor_names_it__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-15", "REQ-RT-16"],
    ),
    Case::pending(
        "AC-K7.2",
        "ac_k7_2_an_upgrade_whose_code_cannot_be_fetched_is_refused__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-16", "REQ-RT-17"],
    ),
    Case::pending(
        "AC-K7.3",
        "ac_k7_3_a_history_across_upgrades_replays_under_its_code_map__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-3", "REQ-RT-17"],
    ),
    Case::pending(
        "AC-K7.4",
        "ac_k7_4_a_construction_migration_links_both_sides_by_one_anchor__service",
        Layer::Service,
        Feature::F037,
        &["REQ-PR-10", "REQ-PR-11"],
    ),
    Case::pending(
        "AC-K7.5",
        "ac_k7_5_fast_and_full_resync_reach_the_same_roots__service",
        Layer::Service,
        Feature::F037,
        &["NFR-21", "REQ-RT-17"],
    ),
    Case::pending(
        "AC-K7.6",
        "ac_k7_6_two_build_profiles_are_two_code_versions__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-16"],
    ),
    Case::pending(
        "AC-K7.7",
        "ac_k7_7_a_mode_change_binds_every_later_upgrade__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-18"],
    ),
    Case::pending(
        "AC-K7.8",
        "ac_k7_8_an_unauthorised_or_unsatisfiable_mode_is_refused__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-18"],
    ),
    Case::pending(
        "AC-K7.9",
        "ac_k7_9_an_input_from_n_is_refused_until_the_code_is_switched__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-20"],
    ),
    Case::pending(
        "AC-K7.10",
        "ac_k7_10_a_continuation_keeps_the_book_and_its_log_chain__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-21", "REQ-RT-17"],
    ),
    Case::pending(
        "AC-K7.11",
        "ac_k7_11_a_checkpoint_restores_under_another_platform__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-19"],
    ),
];
