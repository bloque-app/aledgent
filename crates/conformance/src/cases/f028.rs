// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-028` — credential schemes and the credential import (amendment 0010, Epic N): its registered
//! cases (`F-015` §6, §7). The bodies live with `F-012`'s world (`super::f012`).

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use super::f012::{ac_n1_1__service, ac_n1_2__service, ac_n1_3__service, ac_n1_4__service};
use crate::case::{Case, Feature, Layer, Port};

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F028,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-028`'s table.
pub const CASES: &[Case] = &[
    service_case!(
        "AC-N1.1",
        ac_n1_1__service,
        &["REQ-MG-7", "REQ-AT-1", "INV-20"]
    ),
    service_case!(
        "AC-N1.2",
        ac_n1_2__service,
        &["REQ-MG-7", "ERR-CredentialInvalid"]
    ),
    service_case!(
        "AC-N1.3",
        ac_n1_3__service,
        &["REQ-MG-8", "ERR-NotAuthorised"]
    ),
    service_case!("AC-N1.4", ac_n1_4__service, &["REQ-MG-8", "REQ-AU-13"]),
    // Amendment 0013 (R3): identities without a credential, and enrolment; T-028-10 to T-028-12.
    Case::pending(
        "AC-A9.1",
        "ac_a9_1_only_the_operator_opens_an_identity_without_a_credential__service",
        Layer::Service,
        Feature::F028,
        &["REQ-ID-8", "ERR-NotAuthorised"],
    ),
    Case::pending(
        "AC-A9.2",
        "ac_a9_2_no_owned_custody_with_an_empty_set__service",
        Layer::Service,
        Feature::F028,
        &["REQ-ID-8", "REQ-AU-6", "ERR-NotAuthorised"],
    ),
    Case::pending(
        "AC-A9.3",
        "ac_a9_3_an_enrolment_completes_into_the_set__service",
        Layer::Service,
        Feature::F028,
        &["REQ-AU-22", "REQ-AT-1", "INV-20"],
    ),
    Case::pending(
        "AC-A9.4",
        "ac_a9_4_a_spent_or_wrong_enrolment_is_refused__service",
        Layer::Service,
        Feature::F028,
        &["REQ-AU-22", "ERR-CredentialInvalid"],
    ),
    Case::pending(
        "AC-A9.5",
        "ac_a9_5_no_enrolment_once_the_set_holds_a_credential__service",
        Layer::Service,
        Feature::F028,
        &["REQ-AU-22", "REQ-AU-20", "ERR-NotAuthorised"],
    ),
    Case::pending(
        "AC-A9.6",
        "ac_a9_6_an_enrolled_credential_proves_itself_to_reclaim__service",
        Layer::Service,
        Feature::F028,
        &["REQ-AU-22", "REQ-AU-13"],
    ),
];
