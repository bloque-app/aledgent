// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Closure checks (`F-015` §5, T-015-04 and T-015-06), run before any case.
//!
//! - every catalogue id has a case;
//! - every `INV-*` has a case at each layer SPEC §9 names (`REQ-AR-4`);
//! - no case cites an id the catalogue does not hold;
//! - case names are unique and owners are features of PLAN §4;
//! - `ledger-core`'s closed [`Refusal`] list equals the catalogue's `ERR-*`
//!   ids and descriptions, both ways (`REQ-ER-1`).
//!
//! Each check is a pure function of the catalogue and a case table, so the
//! tests below run it on fixture tables that must fail.

use std::collections::{BTreeMap, BTreeSet};

use ledger_core::Refusal;

use crate::case::Case;
use crate::spec::Catalogue;

/// A failed check: one line per finding.
pub type Findings = Vec<String>;

fn verdict(findings: Findings) -> Result<(), Findings> {
    if findings.is_empty() {
        Ok(())
    } else {
        Err(findings)
    }
}

/// Every id of the catalogue has at least one registered case.
pub fn every_catalogue_id_has_a_case(cat: &Catalogue, cases: &[Case]) -> Result<(), Findings> {
    let covered: BTreeSet<&str> = cases.iter().map(|c| c.id).collect();
    verdict(
        cat.ids()
            .filter(|id| !covered.contains(id))
            .map(|id| format!("{id} has no registered case"))
            .collect(),
    )
}

/// Every invariant has a case at each layer SPEC §9 names for it.
pub fn every_invariant_has_a_case_at_each_named_layer(
    cat: &Catalogue,
    cases: &[Case],
) -> Result<(), Findings> {
    let mut findings = Vec::new();
    for inv in &cat.invariants {
        for layer in &inv.layers {
            if !cases.iter().any(|c| c.id == inv.id && c.layer == *layer) {
                findings.push(format!(
                    "{} names the {layer} layer but no case exercises it there",
                    inv.id
                ));
            }
        }
    }
    verdict(findings)
}

/// No case cites an id missing from the catalogue, as its id or as an
/// `INV-*` or `ERR-*` it traces.
pub fn no_case_cites_an_unknown_id(cat: &Catalogue, cases: &[Case]) -> Result<(), Findings> {
    let mut findings = Vec::new();
    for c in cases {
        if !cat.contains(c.id) {
            findings.push(format!(
                "{} cites {}, which the catalogue does not hold",
                c.name, c.id
            ));
        }
        for t in c.traces {
            let checked = t.starts_with("INV-") || t.starts_with("ERR-");
            if checked && !cat.contains(t) {
                findings.push(format!(
                    "{} traces {t}, which the catalogue does not hold",
                    c.name
                ));
            }
        }
    }
    verdict(findings)
}

/// Case names are unique, and each owner is a feature of PLAN §4.
pub fn names_are_unique_and_owners_planned(cases: &[Case]) -> Result<(), Findings> {
    let mut findings = Vec::new();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for c in cases {
        *seen.entry(c.name).or_default() += 1;
        if !c.owner.is_planned() {
            findings.push(format!(
                "{} is owned by {}, which PLAN §4 does not have",
                c.name, c.owner
            ));
        }
    }
    findings.extend(
        seen.into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(name, n)| format!("{name} is registered {n} times")),
    );
    verdict(findings)
}

/// `ledger-core`'s refusals as (code, description) pairs.
pub fn refusals() -> Vec<(&'static str, &'static str)> {
    Refusal::ALL
        .iter()
        .map(|r| (r.code(), r.description()))
        .collect()
}

/// The closed refusal list equals the catalogue's errors, both ways, and
/// each description is the published one (`REQ-ER-1`).
pub fn refusal_matches_catalogue(
    cat: &Catalogue,
    refusals: &[(&str, &str)],
) -> Result<(), Findings> {
    let mut findings = Vec::new();
    for (code, description) in refusals {
        match cat.error(code) {
            None => findings.push(format!("Refusal has {code}, which SPEC §10 does not name")),
            Some(e) if e.condition != *description => findings.push(format!(
                "{code}: the runtime description {description:?} differs from the published {:?}",
                e.condition
            )),
            Some(_) => {}
        }
    }
    for e in &cat.errors {
        if !refusals.iter().any(|(code, _)| *code == e.id) {
            findings.push(format!("SPEC §10 names {}, which Refusal lacks", e.id));
        }
    }
    let unique: BTreeSet<&str> = refusals.iter().map(|(c, _)| *c).collect();
    if unique.len() != refusals.len() {
        findings.push("Refusal repeats a code".into());
    }
    verdict(findings)
}

/// Every closure check, by name, with its verdict.
pub fn run_all(cat: &Catalogue, cases: &[Case]) -> Vec<(&'static str, Result<(), Findings>)> {
    vec![
        (
            "every_catalogue_id_has_a_case",
            every_catalogue_id_has_a_case(cat, cases),
        ),
        (
            "every_invariant_has_a_case_at_each_named_layer",
            every_invariant_has_a_case_at_each_named_layer(cat, cases),
        ),
        (
            "no_case_cites_an_unknown_id",
            no_case_cites_an_unknown_id(cat, cases),
        ),
        (
            "names_are_unique_and_owners_planned",
            names_are_unique_and_owners_planned(cases),
        ),
        (
            "refusal_matches_catalogue",
            refusal_matches_catalogue(cat, &refusals()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{Feature, Layer};
    use crate::cases::registry;

    fn cat() -> Catalogue {
        Catalogue::checked_in()
    }

    #[test]
    fn the_registry_passes_every_closure_check() {
        for (name, verdict) in run_all(&cat(), &registry()) {
            assert_eq!(verdict, Ok(()), "{name}");
        }
    }

    #[test]
    fn a_fixture_case_citing_an_unknown_id_fails() {
        let mut cases = registry();
        cases.push(Case::pending(
            "INV-99",
            "inv_99_fixture__store",
            Layer::Store,
            Feature::F003,
            &[],
        ));
        let err = no_case_cites_an_unknown_id(&cat(), &cases).unwrap_err();
        assert_eq!(err.len(), 1, "{err:?}");
        assert!(err[0].contains("INV-99"));

        let mut cases = registry();
        cases.push(Case::pending(
            "AC-C5.1",
            "ac_c5_1_beyond_v0_fixture__service",
            Layer::Service,
            Feature::F004,
            &["ERR-Fault"],
        ));
        let err = no_case_cites_an_unknown_id(&cat(), &cases).unwrap_err();
        assert_eq!(err.len(), 2, "{err:?}");
    }

    #[test]
    fn a_fixture_with_no_case_for_one_error_fails() {
        let cases: Vec<Case> = registry()
            .into_iter()
            .filter(|c| c.id != "ERR-BatchTooLarge")
            .collect();
        let err = every_catalogue_id_has_a_case(&cat(), &cases).unwrap_err();
        assert_eq!(
            err,
            vec!["ERR-BatchTooLarge has no registered case".to_string()]
        );
    }

    #[test]
    fn a_fixture_missing_a_named_layer_fails() {
        // INV-2 is named kernel + store; drop its store case.
        let cases: Vec<Case> = registry()
            .into_iter()
            .filter(|c| !(c.id == "INV-2" && c.layer == Layer::Store))
            .collect();
        let err = every_invariant_has_a_case_at_each_named_layer(&cat(), &cases).unwrap_err();
        assert_eq!(err.len(), 1, "{err:?}");
        assert!(err[0].starts_with("INV-2 names the store layer"));
    }

    #[test]
    fn duplicate_names_and_unplanned_owners_fail() {
        let mut cases = registry();
        cases.push(cases[0]);
        let mut stray = cases[1];
        stray.name = "stray_fixture";
        stray.owner = Feature(99);
        cases.push(stray);
        let err = names_are_unique_and_owners_planned(&cases).unwrap_err();
        assert_eq!(err.len(), 2, "{err:?}");
    }

    #[test]
    fn a_refusal_added_without_a_catalogue_entry_fails() {
        let mut list = refusals();
        list.push(("ERR-Fault", "An unexpected fault"));
        let err = refusal_matches_catalogue(&cat(), &list).unwrap_err();
        assert_eq!(
            err,
            vec!["Refusal has ERR-Fault, which SPEC §10 does not name".to_string()]
        );
    }

    #[test]
    fn a_catalogue_error_missing_from_refusal_fails() {
        let list: Vec<_> = refusals()
            .into_iter()
            .filter(|(c, _)| *c != "ERR-QueryUnbounded")
            .collect();
        let err = refusal_matches_catalogue(&cat(), &list).unwrap_err();
        assert_eq!(
            err,
            vec!["SPEC §10 names ERR-QueryUnbounded, which Refusal lacks".to_string()]
        );
    }

    #[test]
    fn a_description_that_differs_from_the_published_one_fails() {
        let mut list = refusals();
        list[0].1 = "Not allowed";
        let err = refusal_matches_catalogue(&cat(), &list).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].starts_with("ERR-NotAuthorised: the runtime description"));
    }
}
