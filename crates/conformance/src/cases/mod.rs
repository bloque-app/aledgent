// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The case registry (`F-015` §2, T-015-05).
//!
//! One table per owning feature, one module each. A feature writes its case
//! bodies in its module and turns its `Case::pending` entries into
//! `Case::run` ones; nothing else changes. [`registry`] joins the tables.
//!
//! The plan proposed registration by distributed slice (`linkme`). Its
//! `#[link_section]` statics are unsafe attributes, which the workspace's
//! `unsafe_code = "forbid"` rejects, so the suite uses the plan's
//! alternative, a hand-kept table, split per feature (`NOTES.md`).

pub mod f001;
pub mod f003;
pub mod f004;
pub mod f006;
pub mod f007;
pub mod f008;
pub mod f009;
pub mod f010;
pub mod f011;
pub mod f012;
pub mod f013;
pub mod f014;
pub mod f016;
pub mod f017;
pub mod f018;
pub mod f021;
pub mod f028;
pub mod f029;
pub mod f036;
pub mod f037;
pub mod f040;

use crate::case::Case;

/// Every table, in feature order.
const TABLES: &[&[Case]] = &[
    f001::CASES,
    f003::CASES,
    f004::CASES,
    f006::CASES,
    f007::CASES,
    f008::CASES,
    f009::CASES,
    f010::CASES,
    f011::CASES,
    f012::CASES,
    f013::CASES,
    f014::CASES,
    f016::CASES,
    f017::CASES,
    f018::CASES,
    f021::CASES,
    f028::CASES,
    f029::CASES,
    f036::CASES,
    f037::CASES,
    f040::CASES,
];

/// Every registered case.
pub fn registry() -> Vec<Case> {
    TABLES.iter().flat_map(|t| t.iter().copied()).collect()
}
