// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-040` — per-transaction approval at ingress (amendment 0037, `US-A10`, `REQ-AU-23`): its
//! registered cases (`F-015` §6, §7).
//!
//! All seventeen are registered as pending when the amendment is applied (amendment 0013's
//! practice), before `F-040` is built. They need the two steps of an approval, the policy of which
//! commands need one, and the durable one-time use of its challenge (`T-040-02` to `T-040-06`).
//! The refusal `ERR-CommandApprovalRequired` was held at ingress until the core carried it
//! (`T-040-10`); the core's closed list has it from rules version 1 (`T-041-06`), and the
//! conformance allowance (`closure::SPEC_AHEAD_OF_PROGRAM`) that named it is gone.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use crate::case::{Case, Feature, Layer};

/// `F-040`'s registered cases.
pub const CASES: &[Case] = &[
    Case::pending(
        "AC-A10.1",
        "ac_a10_1_a_command_that_needs_an_approval_and_has_none_is_refused__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "ERR-CommandApprovalRequired"],
    ),
    Case::pending(
        "AC-A10.2",
        "ac_a10_2_a_valid_approval_takes_the_command_in__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "REQ-CS-8"],
    ),
    Case::pending(
        "AC-A10.3",
        "ac_a10_3_an_approval_of_another_command_is_refused__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "ERR-CredentialInvalid"],
    ),
    Case::pending(
        "AC-A10.4",
        "ac_a10_4_a_spent_expired_or_foreign_approval_is_refused__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "REQ-ER-3", "ERR-CredentialInvalid"],
    ),
    Case::pending(
        "AC-A10.5",
        "ac_a10_5_a_retry_of_an_approved_command_returns_the_first_result__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "REQ-OP-10", "ERR-ClientKeyReused"],
    ),
    Case::pending(
        "AC-A10.6",
        "ac_a10_6_a_bearer_secret_needs_no_approval__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23"],
    ),
    Case::pending(
        "AC-A10.7",
        "ac_a10_7_sign_in_and_reads_ask_for_no_approval__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23"],
    ),
    Case::pending(
        "AC-A10.8",
        "ac_a10_8_adding_a_device_bound_credential_needs_an_existing_credentials_approval__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "REQ-AU-20", "ERR-CommandApprovalRequired"],
    ),
    Case::pending(
        "AC-A10.9",
        "ac_a10_9_two_implementations_of_the_command_digest_agree__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23"],
    ),
    Case::pending(
        "AC-A10.10",
        "ac_a10_10_a_shape_fault_is_malformed_not_a_missing_approval__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "REQ-ER-3", "ERR-MalformedCommand"],
    ),
    Case::pending(
        "AC-A10.11",
        "ac_a10_11_a_second_credential_approves_with_the_devices_own_check__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "REQ-AU-7", "ERR-CredentialInvalid"],
    ),
    Case::pending(
        "AC-A10.12",
        "ac_a10_12_the_approval_of_one_registration_does_not_cover_another__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "ERR-CredentialInvalid"],
    ),
    Case::pending(
        "AC-A10.13",
        "ac_a10_13_a_removed_credential_does_not_approve_and_the_list_is_read_live__service",
        Layer::Service,
        Feature::F040,
        &[
            "REQ-AU-23",
            "NFR-20",
            "ERR-CredentialInvalid",
            "ERR-DecisionUnavailable",
        ],
    ),
    Case::pending(
        "AC-A10.14",
        "ac_a10_14_a_session_of_any_principal_kind_needs_an_approval_and_a_key_signature_is_one__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23"],
    ),
    Case::pending(
        "AC-A10.15",
        "ac_a10_15_an_approval_made_for_another_deployment_is_refused__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "ERR-CredentialInvalid"],
    ),
    Case::pending(
        "AC-A10.16",
        "ac_a10_16_an_upgrade_is_approved_by_the_credential_the_mode_names__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "REQ-RT-18", "ERR-CredentialInvalid"],
    ),
    Case::pending(
        "ERR-CommandApprovalRequired",
        "err_command_approval_required__service",
        Layer::Service,
        Feature::F040,
        &["REQ-AU-23", "REQ-ER-1", "REQ-CS-4"],
    ),
];
