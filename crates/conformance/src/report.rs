// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The run report, `target/conformance/report.json` (`F-015` §2).
//!
//! One row per case per target, with its outcome. The assurance generator
//! reads nothing else, so a declaration can never claim more than a run
//! showed.

use serde::{Deserialize, Serialize};

use crate::case::{Feature, Kind, Layer, Outcome};

/// The report's schema tag.
pub const REPORT_SCHEMA: &str = "aledgent/conformance-report/v1";

/// A whole run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// [`REPORT_SCHEMA`].
    pub schema: String,
    /// The spec version of the catalogue the run used.
    pub spec: String,
    /// The gate profile the run was judged by.
    pub profile: String,
    /// The commit the suite was built from, or `unknown`.
    pub suite_rev: String,
    /// The targets the run covered.
    pub targets: Vec<String>,
    /// One row per case per target.
    pub results: Vec<Row>,
}

/// A case's outcome on one target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// The case name.
    pub case: String,
    /// The catalogue id.
    pub id: String,
    /// The layer it exercises.
    pub layer: Layer,
    /// Its owner.
    pub owner: Feature,
    /// What its passing means.
    pub kind: Kind,
    /// The target.
    pub target: String,
    /// What happened.
    pub outcome: Tag,
    /// Why it failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The feature it waits on, if pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_on: Option<Feature>,
}

/// An outcome, as the report spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tag {
    /// Passed.
    Pass,
    /// Failed.
    Fail,
    /// Its feature is not built.
    Pending,
    /// The target lacks its port.
    NotRunnable,
    /// Not run: filtered out of this run.
    Skipped,
}

impl Row {
    /// The row for an outcome.
    pub fn new(case: &crate::case::Case, target: &str, outcome: &Outcome) -> Row {
        let (tag, detail, pending_on) = match outcome {
            Outcome::Pass => (Tag::Pass, None, None),
            Outcome::Fail(why) => (Tag::Fail, Some(why.clone()), None),
            Outcome::Pending(f) => (Tag::Pending, None, Some(*f)),
            Outcome::NotRunnableHere => (Tag::NotRunnable, None, None),
        };
        Row {
            case: case.name.to_string(),
            id: case.id.to_string(),
            layer: case.layer,
            owner: case.owner,
            kind: case.kind,
            target: target.to_string(),
            outcome: tag,
            detail,
            pending_on,
        }
    }

    /// A row for a case that did not run.
    pub fn skipped(case: &crate::case::Case, target: &str) -> Row {
        let mut row = Row::new(case, target, &Outcome::Pass);
        row.outcome = Tag::Skipped;
        row
    }
}

impl Report {
    /// Parse a report.
    pub fn from_json(text: &str) -> Result<Report, String> {
        let r: Report = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if r.schema != REPORT_SCHEMA {
            return Err(format!("report schema {} is not {REPORT_SCHEMA}", r.schema));
        }
        Ok(r)
    }

    /// Whether `target` is `wanted` or one of its sub-targets
    /// (`in-process/mem-store` is under `in-process`).
    pub fn target_matches(target: &str, wanted: &str) -> bool {
        target == wanted
            || target
                .strip_prefix(wanted)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    /// The rows of `target` and its sub-targets.
    pub fn rows_of<'a>(&'a self, target: &'a str) -> impl Iterator<Item = &'a Row> + 'a {
        self.results
            .iter()
            .filter(move |r| Report::target_matches(&r.target, target))
    }
}
