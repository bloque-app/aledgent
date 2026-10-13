// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-029` — book-only assets and their binding (amendment 0011, `US-F5`): its registered cases
//! (`F-015` §6, §7). The bodies run the settlement adapter over its fake chain, so they live with
//! `F-016`'s kit (`super::f016`).

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use super::f016::{
    ac_f5_1__service, ac_f5_2__service, ac_f5_3__service, ac_f5_4__service, ac_f5_5__service,
};
use crate::case::{Case, Feature, Layer, Port};

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F029,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-029`'s table.
pub const CASES: &[Case] = &[
    service_case!(
        "AC-F5.1",
        ac_f5_1__service,
        &["REQ-AS-7", "ERR-UnsupportedLeg"]
    ),
    service_case!(
        "AC-F5.2",
        ac_f5_2__service,
        &["REQ-AS-8", "ERR-UnsupportedLeg"]
    ),
    service_case!("AC-F5.3", ac_f5_3__service, &["REQ-AS-8"]),
    service_case!(
        "AC-F5.4",
        ac_f5_4__service,
        &["REQ-AS-8", "NFR-9", "ERR-NotAuthorised"]
    ),
    service_case!("AC-F5.5", ac_f5_5__service, &["REQ-AS-9"]),
];
