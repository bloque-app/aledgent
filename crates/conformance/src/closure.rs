// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Closure checks (`F-015` §5, T-015-04 and T-015-06), run before any case.
//!
//! - every catalogue id has a case;
//! - every `INV-*` has a case at each layer SPEC §9 names (`REQ-AR-4`);
//! - no case cites an id the catalogue does not hold;
//! - case names are unique and owners are features of PLAN §4;
//! - `ledger-core`'s closed [`Refusal`] list equals the catalogue's `ERR-*`
//!   ids and descriptions, both ways (`REQ-ER-1`), with the named allowances of
//!   [`SPEC_AHEAD_OF_PROGRAM`], each of which fails once its release ships. There is none today:
//!   amendment 0037's one ended with rules version 1 (`T-041-06`).
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

/// A spec item that runs ahead of the program, for a stated time (`REQ-ER-1`).
///
/// The only way the spec may hold an error the program's closed list does not: the line is in
/// `SPEC.md` §10 and in no `Refusal`, and the check below is told so by name. The allowance names
/// the amendment that put the line in the spec and the release that ends it, as the core's rules
/// version (`ends_at_generation`), and the check **fails once that release ships**, whether or
/// not the core got the line, so the allowance cannot outlive its reason. The other direction, a `Refusal` with no line in §10, stays a hard failure.
/// `T-040-15` makes this data on the spec item; this list is the stopgap until it is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpecAhead {
    /// The `ERR-*` in §10 that the core's list does not have yet.
    pub id: &'static str,
    /// The amendment that put it in the spec.
    pub amendment: &'static str,
    /// The release that ends the allowance, in words.
    pub ends_with: &'static str,
    /// The release, as a number that changes by itself: the rules version of the core that
    /// ends the allowance (`REQ-RT-20`; every upgrade of the core raises it). While the core's
    /// compiled generation is below it, the allowance holds; at it, the allowance fails whether
    /// or not the core got the line.
    pub ends_at_generation: u64,
}

/// The rules version the core Agent is compiled with: the `GENERATION` of `agents/core`, read
/// from its source so that the check follows the code and not a copy.
pub fn core_generation() -> Result<u64, String> {
    const CORE: &str = include_str!("../../../agents/core/src/lib.rs");
    let line = CORE
        .lines()
        .find(|l| l.starts_with("const GENERATION: u64 = "))
        .ok_or("agents/core/src/lib.rs has no `const GENERATION: u64 = N;`")?;
    line["const GENERATION: u64 = ".len()..]
        .trim_end_matches(';')
        .trim()
        .parse()
        .map_err(|e| format!("the core's GENERATION does not parse: {e}"))
}

/// The allowances in force: none. The one there was, for `ERR-CommandApprovalRequired`
/// (amendment 0037, `T-040-10`), ended with rules version 1: the line is in `ledger-core`'s
/// `Refusal`, appended at the end so no audit code moves, and ingress sends it from there
/// (`T-041-06`). The mechanism stays for the next spec line that runs ahead of the core
/// (`T-040-15`).
pub const SPEC_AHEAD_OF_PROGRAM: &[SpecAhead] = &[];

/// The closed refusal list equals the catalogue's errors, both ways, and
/// each description is the published one (`REQ-ER-1`), with the named
/// allowances of `ahead` and no others.
pub fn refusal_matches_catalogue(
    cat: &Catalogue,
    refusals: &[(&str, &str)],
    ahead: &[SpecAhead],
    core_generation: u64,
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
        if refusals.iter().any(|(code, _)| *code == e.id) {
            continue;
        }
        if ahead.iter().any(|a| a.id == e.id) {
            continue;
        }
        findings.push(format!("SPEC §10 names {}, which Refusal lacks", e.id));
    }
    for a in ahead {
        if a.amendment.is_empty() || a.ends_with.is_empty() {
            findings.push(format!(
                "{}: an allowance must name its amendment and the release that ends it",
                a.id
            ));
        }
        if cat.error(a.id).is_none() {
            findings.push(format!(
                "{}: an allowance for an error SPEC §10 does not name",
                a.id
            ));
        }
        if core_generation >= a.ends_at_generation {
            findings.push(format!(
                "{}: the allowance of amendment {} ended with {} (the core is at code generation \
                 {core_generation}): put the line in Refusal and remove the allowance",
                a.id, a.amendment, a.ends_with
            ));
        }
        if refusals.iter().any(|(code, _)| *code == a.id) {
            findings.push(format!(
                "{} is allowed to be missing from Refusal (amendment {}, until {}), and Refusal \
                 now has it: remove the allowance",
                a.id, a.amendment, a.ends_with
            ));
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
            match core_generation() {
                Ok(g) => refusal_matches_catalogue(cat, &refusals(), SPEC_AHEAD_OF_PROGRAM, g),
                Err(e) => Err(vec![e]),
            },
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
        let err = refusal_matches_catalogue(&cat(), &list, SPEC_AHEAD_OF_PROGRAM, 0).unwrap_err();
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
        let err = refusal_matches_catalogue(&cat(), &list, SPEC_AHEAD_OF_PROGRAM, 0).unwrap_err();
        assert_eq!(
            err,
            vec!["SPEC §10 names ERR-QueryUnbounded, which Refusal lacks".to_string()]
        );
    }

    #[test]
    fn a_description_that_differs_from_the_published_one_fails() {
        let mut list = refusals();
        list[0].1 = "Not allowed";
        let err = refusal_matches_catalogue(&cat(), &list, SPEC_AHEAD_OF_PROGRAM, 0).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].starts_with("ERR-NotAuthorised: the runtime description"));
    }

    /// A fixture allowance, as the one `T-040-10` added was: a spec line ahead of the core, ended by
    /// rules version 2 (the real list is empty, so the expiry logic is tested on this).
    const FIXTURE: SpecAhead = SpecAhead {
        id: "ERR-CommandApprovalRequired",
        amendment: "0037",
        ends_with: "a fixture release, the first of rules version 2",
        ends_at_generation: 2,
    };

    /// The core's refusals but one, as a core that did not have the line would list them.
    fn refusals_without(id: &str) -> Vec<(&'static str, &'static str)> {
        refusals().into_iter().filter(|(c, _)| *c != id).collect()
    }

    #[test]
    fn there_is_no_allowance_in_force_and_the_core_carries_the_line_t_041_06() {
        // Rules version 1 is the core's release that ended amendment 0037's allowance: the line is
        // in `Refusal`, nothing is allowed to be missing, and the closed list equals §10 exactly.
        assert!(SPEC_AHEAD_OF_PROGRAM.is_empty());
        assert_eq!(core_generation(), Ok(1));
        assert!(
            refusals()
                .iter()
                .any(|(code, _)| *code == "ERR-CommandApprovalRequired"),
            "Refusal carries ERR-CommandApprovalRequired"
        );
        assert_eq!(
            refusal_matches_catalogue(&cat(), &refusals(), SPEC_AHEAD_OF_PROGRAM, 1),
            Ok(())
        );
    }

    #[test]
    fn a_fixture_allowance_holds_until_its_rules_version_and_fails_at_it_even_without_the_line() {
        let list = refusals_without("ERR-CommandApprovalRequired");
        assert!(
            refusal_matches_catalogue(&cat(), &list, &[FIXTURE], 1).is_ok(),
            "the allowance holds while the core is below the rules version that ends it"
        );
        // At the rules version that ends it, the allowance is over, whether or not the line is in.
        let err = refusal_matches_catalogue(&cat(), &list, &[FIXTURE], 2).unwrap_err();
        assert_eq!(err.len(), 1, "{err:?}");
        assert!(
            err[0].contains("the allowance of amendment 0037 ended"),
            "{err:?}"
        );
    }

    #[test]
    fn without_the_allowance_the_spec_line_ahead_of_the_program_fails() {
        let list = refusals_without("ERR-CommandApprovalRequired");
        let err = refusal_matches_catalogue(&cat(), &list, &[], 1).unwrap_err();
        assert_eq!(
            err,
            vec!["SPEC §10 names ERR-CommandApprovalRequired, which Refusal lacks".to_string()]
        );
    }

    #[test]
    fn the_allowance_fails_once_the_core_carries_the_line() {
        // The core as it is, with the line: an allowance for it is left over and must go.
        let err = refusal_matches_catalogue(&cat(), &refusals(), &[FIXTURE], 1).unwrap_err();
        assert_eq!(err.len(), 1, "{err:?}");
        assert!(err[0].ends_with("remove the allowance"), "{err:?}");
    }

    #[test]
    fn an_allowance_without_an_amendment_or_a_release_fails() {
        let bare = [SpecAhead {
            amendment: "",
            ends_with: "",
            ..FIXTURE
        }];
        let list = refusals_without("ERR-CommandApprovalRequired");
        let err = refusal_matches_catalogue(&cat(), &list, &bare, 1).unwrap_err();
        assert_eq!(err.len(), 1, "{err:?}");
    }

    #[test]
    fn the_allowance_covers_that_error_only() {
        let list: Vec<_> = refusals()
            .into_iter()
            .filter(|(c, _)| *c != "ERR-QueryUnbounded" && *c != "ERR-CommandApprovalRequired")
            .collect();
        let err = refusal_matches_catalogue(&cat(), &list, &[FIXTURE], 1).unwrap_err();
        assert_eq!(
            err,
            vec!["SPEC §10 names ERR-QueryUnbounded, which Refusal lacks".to_string()]
        );
    }
}
