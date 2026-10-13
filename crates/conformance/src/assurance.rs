// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The assurance declaration (`F-015` §9, T-015-08; `REQ-AR-4`,
//! `REQ-AR-5`, `REQ-AR-6`, `REQ-PR-6`, `NFR-14`).
//!
//! The declaration is generated from a run report and nothing else, so no
//! claim can exceed what passed. For each invariant of SPEC §9:
//!
//! - **enforced** when every case that bears on it passed on every store of
//!   the target, and it is realised at each layer it is declared at;
//! - **attested** when it is not enforced but `assurance/attestations.toml`
//!   holds a reviewed entry with a justification;
//! - **unmet** otherwise, with the reasons.
//!
//! The declared layers are the layers §9 names, except where `REQ-AR-6`
//! declares the first release at another layer (`spec/floor.toml`
//! `v0_layer`: `INV-15`, `INV-16`, `INV-22`). There the declaration follows
//! `REQ-AR-6`, flags the invariant as below the layer §9 names, and keeps the
//! cases at the §9 layer as the target, reported but not counted. §9 and
//! `REQ-AR-6` disagree; item 2 of the proposed amendment 0006 resolves it and
//! is not yet ruled.
//!
//! A passing known-gap case at a layer denies a claim at that layer.
//!
//! In **release** mode, an unmet invariant (`REQ-AR-5`: every invariant
//! declared) or a floor shortfall (`REQ-AR-6`) fails generation. In
//! **milestone** mode they are reported. In both, a declaration weaker than
//! the baseline fails unless an amendment is cited (`REQ-AR-4`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::case::{Kind, Layer};
use crate::gates::AssuranceMode;
use crate::report::{Report, Row, Tag};
use crate::spec::{Catalogue, Disclosures, Floor};

/// The declaration's schema tag.
pub const ASSURANCE_SCHEMA: &str = "aledgent/assurance/v1";

/// The checked-in attestations.
pub const ATTESTATIONS_TOML: &str = include_str!("../assurance/attestations.toml");

/// The kernel the declaration is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelRef {
    /// `cipher-clerk`.
    #[serde(rename = "crate")]
    pub krate: String,
    /// The pinned revision, short.
    pub rev: String,
}

/// An invariant's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Enforced, as the cases show.
    Enforced,
    /// Attested by a reviewed entry, not enforced.
    Attested,
    /// Neither.
    Unmet,
}

/// One invariant's line in the declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvariantClaim {
    /// `INV-n`.
    pub id: String,
    /// The layers SPEC §9 names.
    pub named_layers: Vec<Layer>,
    /// The layers this declaration is judged at: §9's, or `REQ-AR-6`'s.
    pub declared_layers: Vec<Layer>,
    /// The layers a passing check shows it at.
    pub realised_layers: Vec<Layer>,
    /// Enforced, attested or unmet.
    pub status: Status,
    /// Whether the declared layers fall short of §9's (`REQ-AR-4`).
    pub below_named_layer: bool,
    /// What allows the declared layers, when they are not §9's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    /// The floor's note on the disagreement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The cases the status rests on.
    pub cases: Vec<String>,
    /// Cases at the §9 layer this release does not declare: the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_layer_cases: Vec<String>,
    /// Why it is not enforced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// The attestation's justification, when attested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<String>,
}

/// How the declaration measures against `REQ-AR-6`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloorResult {
    /// `REQ-AR-6`.
    pub basis: String,
    /// Whether every line of the floor is met.
    pub met: bool,
    /// What falls short.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortfall: Vec<String>,
}

/// The machine-readable assurance level (`REQ-AR-5`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    /// [`ASSURANCE_SCHEMA`].
    pub schema: String,
    /// The spec version.
    pub spec: String,
    /// The target the claims hold on.
    pub target: String,
    /// Milestone or release.
    pub mode: AssuranceMode,
    /// The gate profile of the run.
    pub profile: String,
    /// The kernel.
    pub kernel: KernelRef,
    /// The commit the suite ran from.
    pub suite_rev: String,
    /// One line per invariant of §9, in order.
    pub invariants: Vec<InvariantClaim>,
    /// Every disclosure is a signed assertion until `DEF-3` (`REQ-PR-6`).
    pub disclosures: Disclosures,
    /// The floor of `REQ-AR-6`.
    pub floor: FloorResult,
}

/// Reviewed attestations.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestations {
    /// One per attested invariant.
    #[serde(default, rename = "attest")]
    pub entries: Vec<Attestation>,
}

/// One reviewed attestation.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    /// The invariant.
    pub id: String,
    /// Why the invariant holds although no case enforces it.
    pub justification: String,
    /// Who reviewed it.
    pub reviewed_by: String,
}

impl Attestations {
    /// The checked-in attestations.
    pub fn checked_in() -> Attestations {
        Attestations::from_toml(ATTESTATIONS_TOML).expect("assurance/attestations.toml parses")
    }

    /// Parse an attestations file. An entry without a justification or a
    /// reviewer is refused: an attestation is a reviewed claim, not a flag.
    pub fn from_toml(text: &str) -> Result<Attestations, String> {
        let a: Attestations = toml::from_str(text).map_err(|e| e.to_string())?;
        for e in &a.entries {
            if e.justification.trim().is_empty() || e.reviewed_by.trim().is_empty() {
                return Err(format!(
                    "attestation of {} lacks a justification or a reviewer",
                    e.id
                ));
            }
        }
        Ok(a)
    }
}

/// Everything generation reads besides the report.
#[derive(Clone, Debug)]
pub struct Inputs<'a> {
    /// The catalogue.
    pub catalogue: &'a Catalogue,
    /// The floor.
    pub floor: &'a Floor,
    /// The attestations.
    pub attestations: &'a Attestations,
    /// The target to declare for; its sub-targets count too.
    pub target: &'a str,
    /// Milestone or release.
    pub mode: AssuranceMode,
    /// The kernel revision.
    pub kernel_rev: &'a str,
}

/// A case's verdict over every sub-target of the target.
fn case_verdicts<'a>(rows: &[&'a Row]) -> BTreeMap<&'a str, (&'a Row, Tag)> {
    let mut by_case: BTreeMap<&str, (&Row, Tag)> = BTreeMap::new();
    for r in rows {
        let entry = by_case.entry(r.case.as_str()).or_insert((r, Tag::Pass));
        // The worst outcome on any store is the case's outcome.
        let rank = |t: Tag| match t {
            Tag::Pass => 0,
            Tag::Skipped => 1,
            Tag::NotRunnable => 2,
            Tag::Pending => 3,
            Tag::Fail => 4,
        };
        if rank(r.outcome) > rank(entry.1) {
            *entry = (r, r.outcome);
        }
    }
    by_case
}

fn claim(
    report: &Report,
    inputs: &Inputs<'_>,
    inv: &crate::spec::InvariantEntry,
) -> InvariantClaim {
    let v0 = inputs.floor.v0_layers_of(&inv.id);
    let declared = v0.map_or_else(|| inv.layers.clone(), |v| v.layers.clone());
    let is_target = |l: Layer| v0.is_some() && inv.layers.contains(&l) && !declared.contains(&l);

    let rows: Vec<&Row> = report
        .rows_of(inputs.target)
        .filter(|r| r.id == inv.id)
        .collect();
    let verdicts = case_verdicts(&rows);

    let mut cases = Vec::new();
    let mut target_layer_cases = Vec::new();
    let mut reasons = Vec::new();
    let mut realised = BTreeSet::new();
    let mut denied = BTreeSet::new();
    for (name, (row, tag)) in &verdicts {
        if is_target(row.layer) {
            target_layer_cases.push(name.to_string());
            continue;
        }
        cases.push(name.to_string());
        // A layer is realised only if every case at it passed as a check.
        if !(*tag == Tag::Pass && row.kind == Kind::Check) {
            denied.insert(row.layer);
        }
        match tag {
            Tag::Pass => {
                if row.kind == Kind::KnownGap {
                    reasons.push(format!(
                        "{name}: a known gap at the {} layer passes, so the layer does not enforce it",
                        row.layer
                    ));
                } else {
                    realised.insert(row.layer);
                }
            }
            Tag::Fail => reasons.push(format!("{name}: failed on {}", row.target)),
            Tag::Pending => reasons.push(format!(
                "{name}: pending on {}",
                row.pending_on.unwrap_or(row.owner)
            )),
            Tag::NotRunnable => reasons.push(format!("{name}: not runnable on {}", row.target)),
            Tag::Skipped => reasons.push(format!("{name}: not run on {}", row.target)),
        }
    }
    let realised: Vec<Layer> = realised.difference(&denied).copied().collect();
    if cases.is_empty() {
        reasons.push("no case ran on this target".into());
    }
    for l in &declared {
        if !realised.contains(l) {
            reasons.push(format!("not realised at the declared {l} layer"));
        }
    }
    let enforced = reasons.is_empty();
    let attestation = inputs
        .attestations
        .entries
        .iter()
        .find(|a| a.id == inv.id)
        .map(|a| format!("{} (reviewed by {})", a.justification, a.reviewed_by));
    let status = if enforced {
        Status::Enforced
    } else if attestation.is_some() {
        Status::Attested
    } else {
        Status::Unmet
    };
    let below = inv.layers.iter().any(|l| !declared.contains(l));
    InvariantClaim {
        id: inv.id.clone(),
        named_layers: inv.layers.clone(),
        declared_layers: declared,
        realised_layers: realised,
        status,
        below_named_layer: below,
        basis: v0.map(|_| inputs.floor.basis.clone()),
        note: v0.map(|v| v.note.clone()),
        cases,
        target_layer_cases,
        reasons: if enforced { Vec::new() } else { reasons },
        attestation: if status == Status::Attested {
            attestation
        } else {
            None
        },
    }
}

/// Measure claims against the floor of `REQ-AR-6`.
pub fn floor_shortfall(claims: &[InvariantClaim], floor: &Floor) -> Vec<String> {
    let mut out = Vec::new();
    for req in &floor.requirements {
        for id in &req.ids {
            match claims.iter().find(|c| &c.id == id) {
                None => out.push(format!("{id}: not declared ({})", req.text)),
                Some(c) if c.status != Status::Enforced => out.push(format!(
                    "{id}: {:?}, the floor asks {} ({})",
                    c.status,
                    req.layers
                        .iter()
                        .map(|l| l.as_str())
                        .collect::<Vec<_>>()
                        .join(" or "),
                    req.text
                )),
                Some(c) if !c.realised_layers.iter().any(|l| req.layers.contains(l)) => {
                    out.push(format!(
                        "{id}: enforced at {:?}, the floor asks {} ({})",
                        c.realised_layers,
                        req.layers
                            .iter()
                            .map(|l| l.as_str())
                            .collect::<Vec<_>>()
                            .join(" or "),
                        req.text
                    ))
                }
                Some(_) => {}
            }
        }
    }
    out
}

/// Generate the declaration for `inputs.target` from `report`.
pub fn generate(report: &Report, inputs: &Inputs<'_>) -> Result<Declaration, Vec<String>> {
    if report.spec != inputs.catalogue.spec_version {
        return Err(vec![format!(
            "the report was run against spec {}, the catalogue is {}",
            report.spec, inputs.catalogue.spec_version
        )]);
    }
    if report.rows_of(inputs.target).next().is_none() {
        return Err(vec![format!(
            "the report has no row for target {}",
            inputs.target
        )]);
    }
    let invariants: Vec<InvariantClaim> = inputs
        .catalogue
        .invariants
        .iter()
        .map(|inv| claim(report, inputs, inv))
        .collect();
    let shortfall = floor_shortfall(&invariants, inputs.floor);
    let declaration = Declaration {
        schema: ASSURANCE_SCHEMA.into(),
        spec: inputs.catalogue.spec_version.clone(),
        target: inputs.target.to_string(),
        mode: inputs.mode,
        profile: report.profile.clone(),
        kernel: KernelRef {
            krate: "cipher-clerk".into(),
            rev: inputs.kernel_rev.chars().take(7).collect(),
        },
        suite_rev: report.suite_rev.clone(),
        invariants,
        disclosures: inputs.floor.disclosures.clone(),
        floor: FloorResult {
            basis: inputs.floor.basis.clone(),
            met: shortfall.is_empty(),
            shortfall,
        },
    };
    if inputs.mode == AssuranceMode::Release {
        let mut errors: Vec<String> = declaration
            .invariants
            .iter()
            .filter(|c| c.status == Status::Unmet)
            .map(|c| {
                format!(
                    "{} is neither enforced nor attested (REQ-AR-5): {}",
                    c.id,
                    c.reasons.join("; ")
                )
            })
            .collect();
        errors.extend(
            declaration
                .floor
                .shortfall
                .iter()
                .map(|s| format!("floor (REQ-AR-6): {s}")),
        );
        if !errors.is_empty() {
            return Err(errors);
        }
    }
    Ok(declaration)
}

/// Whether `amendment` names an existing file `amendments/NNNN-*.md` under
/// `repo_root`.
pub fn is_amendment(repo_root: &Path, amendment: &str) -> bool {
    let Some(name) = amendment.strip_prefix("amendments/") else {
        return false;
    };
    let well_formed = name.len() > 8
        && name.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && name.as_bytes()[4] == b'-'
        && name.ends_with(".md")
        && !name.contains('/');
    well_formed && repo_root.join(amendment).is_file()
}

/// No-weakening against the last released declaration (`REQ-AR-4`,
/// `REQ-AR-6`): an invariant enforced in the baseline stays enforced at
/// every layer it was realised at; one attested stays attested or better.
/// A weakening passes only when `amendment` names an existing file in
/// `amendments/`.
pub fn check_no_weakening(
    new: &Declaration,
    baseline: &Declaration,
    amendment: Option<&str>,
    repo_root: &Path,
) -> Result<Vec<String>, Vec<String>> {
    let mut weakened = Vec::new();
    for old in &baseline.invariants {
        let Some(now) = new.invariants.iter().find(|c| c.id == old.id) else {
            weakened.push(format!("{} is no longer declared", old.id));
            continue;
        };
        match old.status {
            Status::Enforced => {
                if now.status != Status::Enforced {
                    weakened.push(format!("{}: enforced → {:?}", old.id, now.status));
                } else {
                    let lost: Vec<&Layer> = old
                        .realised_layers
                        .iter()
                        .filter(|l| !now.realised_layers.contains(l))
                        .collect();
                    if !lost.is_empty() {
                        weakened.push(format!("{}: no longer realised at {lost:?}", old.id));
                    }
                }
            }
            Status::Attested => {
                if now.status == Status::Unmet {
                    weakened.push(format!("{}: attested → unmet", old.id));
                }
            }
            Status::Unmet => {}
        }
    }
    if old_mode_stronger(baseline, new) {
        weakened.push(format!(
            "disclosures: {} → {}",
            baseline.disclosures.mode, new.disclosures.mode
        ));
    }
    if weakened.is_empty() {
        return Ok(Vec::new());
    }
    match amendment {
        Some(a) if is_amendment(repo_root, a) => Ok(weakened
            .into_iter()
            .map(|w| format!("{w} (allowed by {a})"))
            .collect()),
        Some(a) => Err(weakened
            .into_iter()
            .chain([format!("{a} is not a file in amendments/")])
            .collect()),
        None => Err(weakened),
    }
}

fn old_mode_stronger(baseline: &Declaration, new: &Declaration) -> bool {
    baseline.disclosures.mode != new.disclosures.mode && new.disclosures.mode == "signed-assertion"
}

/// Verify mode (`NFR-14`): each enforced claim of `declaration` must be
/// backed by the given run of the same target, at every layer it claims.
pub fn verify(
    declaration: &Declaration,
    report: &Report,
    inputs: &Inputs<'_>,
) -> Result<(), Vec<String>> {
    let inputs = Inputs {
        target: &declaration.target,
        mode: AssuranceMode::Milestone,
        ..inputs.clone()
    };
    let fresh = generate(report, &inputs)?;
    let mut errors = Vec::new();
    if declaration.spec != fresh.spec {
        errors.push(format!(
            "the declaration is for spec {}, the run for {}",
            declaration.spec, fresh.spec
        ));
    }
    for c in &declaration.invariants {
        let Some(f) = fresh.invariants.iter().find(|f| f.id == c.id) else {
            errors.push(format!("{} is not an invariant of the catalogue", c.id));
            continue;
        };
        match c.status {
            Status::Enforced => {
                if f.status != Status::Enforced {
                    errors.push(format!(
                        "{} is declared enforced; the run shows {:?}: {}",
                        c.id,
                        f.status,
                        f.reasons.join("; ")
                    ));
                }
                let unbacked: Vec<&Layer> = c
                    .realised_layers
                    .iter()
                    .filter(|l| !f.realised_layers.contains(l))
                    .collect();
                if !unbacked.is_empty() {
                    errors.push(format!(
                        "{} claims {unbacked:?}, which no passing case of the run shows",
                        c.id
                    ));
                }
            }
            Status::Attested => {
                if !inputs.attestations.entries.iter().any(|a| a.id == c.id) {
                    errors.push(format!(
                        "{} is declared attested without a reviewed attestation",
                        c.id
                    ));
                }
            }
            Status::Unmet => {}
        }
    }
    if declaration.disclosures != inputs.floor.disclosures {
        errors.push(format!(
            "disclosures are declared {}, the floor says {} ({})",
            declaration.disclosures.mode,
            inputs.floor.disclosures.mode,
            inputs.floor.disclosures.basis
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{Feature, Outcome};
    use crate::cases::registry;
    use crate::report::REPORT_SCHEMA;

    /// A report where every registered case has `outcome` unless `over`
    /// says otherwise, on both in-process stores.
    fn fixture(default: Outcome, over: &[(&str, Outcome)]) -> Report {
        let mut results = Vec::new();
        for target in ["in-process/mem-store", "in-process/file-store"] {
            for case in registry() {
                let outcome = over
                    .iter()
                    .find(|(n, _)| *n == case.name)
                    .map(|(_, o)| o.clone())
                    .unwrap_or_else(|| default.clone());
                results.push(Row::new(&case, target, &outcome));
            }
        }
        Report {
            schema: REPORT_SCHEMA.into(),
            spec: Catalogue::checked_in().spec_version,
            profile: "fixture".into(),
            suite_rev: "fixture".into(),
            targets: vec![
                "in-process/mem-store".into(),
                "in-process/file-store".into(),
            ],
            results,
        }
    }

    /// The known gap closed and its case rewritten: drop its rows.
    fn without_gaps(mut r: Report) -> Report {
        r.results.retain(|row| row.kind != Kind::KnownGap);
        r
    }

    struct World {
        cat: Catalogue,
        floor: Floor,
        attest: Attestations,
    }

    fn world() -> World {
        World {
            cat: Catalogue::checked_in(),
            floor: Floor::checked_in(),
            attest: Attestations::checked_in(),
        }
    }

    fn inputs(w: &World, mode: AssuranceMode) -> Inputs<'_> {
        Inputs {
            catalogue: &w.cat,
            floor: &w.floor,
            attestations: &w.attest,
            target: "in-process",
            mode,
            kernel_rev: ledger_kernel::KERNEL_REV,
        }
    }

    fn claim_of<'a>(d: &'a Declaration, id: &str) -> &'a InvariantClaim {
        d.invariants.iter().find(|c| c.id == id).unwrap()
    }

    #[test]
    fn a_full_green_run_meets_the_floor_in_release_mode() {
        let w = world();
        let report = without_gaps(fixture(Outcome::Pass, &[]));
        let d = generate(&report, &inputs(&w, AssuranceMode::Release)).unwrap();
        assert!(d.floor.met, "{:?}", d.floor.shortfall);
        assert!(d.invariants.iter().all(|c| c.status == Status::Enforced));
        assert_eq!(d.invariants.len(), 25);
        assert_eq!(d.kernel.rev, "e9a7b08");
        assert_eq!(d.disclosures.mode, "signed-assertion");
        assert_eq!(d.disclosures.basis, "REQ-PR-6");
    }

    #[test]
    fn inv_15_16_22_are_declared_at_service_following_req_ar_6() {
        let w = world();
        let report = without_gaps(fixture(Outcome::Pass, &[]));
        let d = generate(&report, &inputs(&w, AssuranceMode::Release)).unwrap();
        for id in ["INV-15", "INV-16", "INV-22"] {
            let c = claim_of(&d, id);
            assert_eq!(c.declared_layers, vec![Layer::Service], "{id}");
            assert!(c.below_named_layer, "{id}");
            assert_eq!(c.basis.as_deref(), Some("REQ-AR-6"));
            assert!(c.note.as_deref().unwrap().contains("0006"));
            assert!(!c.target_layer_cases.is_empty(), "{id}");
            assert!(
                !c.realised_layers
                    .iter()
                    .any(|l| c.named_layers.contains(l) && !l.is_service())
            );
        }
        // The §9 target cases pending do not block the V0 claim.
        let pending_targets: Vec<(&str, Outcome)> = registry()
            .into_iter()
            .filter(|c| ["INV-15", "INV-16", "INV-22"].contains(&c.id) && !c.layer.is_service())
            .map(|c| (c.name, Outcome::Pending(c.owner)))
            .collect();
        let report = without_gaps(fixture(Outcome::Pass, &pending_targets));
        let d = generate(&report, &inputs(&w, AssuranceMode::Release)).unwrap();
        for id in ["INV-15", "INV-16", "INV-22"] {
            assert_eq!(claim_of(&d, id).status, Status::Enforced, "{id}");
        }
        // Every other invariant is at the layers §9 names.
        for c in d
            .invariants
            .iter()
            .filter(|c| !["INV-15", "INV-16", "INV-22"].contains(&c.id.as_str()))
        {
            assert!(!c.below_named_layer, "{}", c.id);
            assert_eq!(c.declared_layers, c.named_layers);
        }
    }

    #[test]
    fn inv_3_failing_its_kernel_case_gives_no_kernel_claim_and_misses_the_floor() {
        let w = world();
        let report = without_gaps(fixture(
            Outcome::Pass,
            &[(
                "inv_3_a_raw_overdraw_of_a_flagged_account_is_refused__kernel",
                Outcome::Fail("overdrawn".into()),
            )],
        ));
        let d = generate(&report, &inputs(&w, AssuranceMode::Milestone)).unwrap();
        let c = claim_of(&d, "INV-3");
        assert_eq!(c.status, Status::Unmet);
        assert!(!c.realised_layers.contains(&Layer::Kernel));
        assert!(!d.floor.met);
        assert!(d.floor.shortfall.iter().any(|s| s.starts_with("INV-3:")));
        // In release mode the same report cannot produce a declaration.
        let err = generate(&report, &inputs(&w, AssuranceMode::Release)).unwrap_err();
        assert!(
            err.iter().any(|e| e.starts_with("INV-3 is neither")),
            "{err:?}"
        );
        assert!(
            err.iter().any(|e| e.starts_with("floor (REQ-AR-6): INV-3")),
            "{err:?}"
        );
    }

    #[test]
    fn a_passing_known_gap_denies_the_kernel_claim_for_inv_4() {
        let w = world();
        let report = fixture(Outcome::Pass, &[]);
        let d = generate(&report, &inputs(&w, AssuranceMode::Milestone)).unwrap();
        let c = claim_of(&d, "INV-4");
        assert_eq!(c.status, Status::Unmet);
        assert!(c.realised_layers.is_empty(), "{:?}", c.realised_layers);
        assert!(c.reasons.iter().any(|r| r.contains("known gap")));
    }

    #[test]
    fn a_pending_or_single_store_failure_blocks_enforcement() {
        let w = world();
        let mut report = without_gaps(fixture(Outcome::Pass, &[]));
        // Fail INV-1's kernel case on the file store only.
        for r in &mut report.results {
            if r.id == "INV-1" && r.target == "in-process/file-store" {
                r.outcome = Tag::Fail;
            }
        }
        let d = generate(&report, &inputs(&w, AssuranceMode::Milestone)).unwrap();
        assert_eq!(claim_of(&d, "INV-1").status, Status::Unmet);
        // Only the mem store is still enforced on its own.
        let mem = Inputs {
            target: "in-process/mem-store",
            ..inputs(&w, AssuranceMode::Milestone)
        };
        let d = generate(&report, &mem).unwrap();
        assert_eq!(claim_of(&d, "INV-1").status, Status::Enforced);
    }

    #[test]
    fn an_attestation_needs_a_reviewed_entry() {
        let mut w = world();
        let report = without_gaps(fixture(
            Outcome::Pass,
            &[(
                "inv_19_an_anchor_commits_to_a_stated_sequence_and_nothing_else__service",
                Outcome::Pending(Feature::F017),
            )],
        ));
        let d = generate(&report, &inputs(&w, AssuranceMode::Milestone)).unwrap();
        assert_eq!(claim_of(&d, "INV-19").status, Status::Unmet);
        w.attest = Attestations::from_toml(
            "[[attest]]\nid = \"INV-19\"\njustification = \"reviewed by hand\"\nreviewed_by = \"owner\"\n",
        )
        .unwrap();
        let d = generate(&report, &inputs(&w, AssuranceMode::Release)).unwrap();
        let c = claim_of(&d, "INV-19");
        assert_eq!(c.status, Status::Attested);
        assert!(
            c.attestation
                .as_deref()
                .unwrap()
                .contains("reviewed by hand")
        );
        assert!(
            Attestations::from_toml(
                "[[attest]]\nid = \"INV-19\"\njustification = \"\"\nreviewed_by = \"owner\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn a_baseline_weakening_fails_without_an_amendment_and_passes_with_one() {
        let w = world();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let green = without_gaps(fixture(Outcome::Pass, &[]));
        let baseline = generate(&green, &inputs(&w, AssuranceMode::Release)).unwrap();
        assert_eq!(
            check_no_weakening(&baseline, &baseline, None, &root),
            Ok(Vec::new())
        );

        let weaker_report = without_gaps(fixture(
            Outcome::Pass,
            &[(
                "inv_2_overwrites_are_refused_and_survive_a_reopen__store",
                Outcome::Fail("rewritten".into()),
            )],
        ));
        let weaker = generate(&weaker_report, &inputs(&w, AssuranceMode::Milestone)).unwrap();
        let err = check_no_weakening(&weaker, &baseline, None, &root).unwrap_err();
        assert!(
            err.iter().any(|e| e.starts_with("INV-2: enforced")),
            "{err:?}"
        );
        let err = check_no_weakening(
            &weaker,
            &baseline,
            Some("amendments/9999-nothing.md"),
            &root,
        )
        .unwrap_err();
        assert!(err.iter().any(|e| e.contains("not a file in amendments/")));
        let ok = check_no_weakening(
            &weaker,
            &baseline,
            Some("amendments/0006-lifecycle-and-assurance-fixes.md"),
            &root,
        )
        .unwrap();
        assert!(ok[0].contains("allowed by amendments/0006"));
    }

    #[test]
    fn verify_rejects_a_declaration_that_claims_more_than_the_run() {
        let w = world();
        let green = without_gaps(fixture(Outcome::Pass, &[]));
        let declared = generate(&green, &inputs(&w, AssuranceMode::Release)).unwrap();
        assert_eq!(
            verify(&declared, &green, &inputs(&w, AssuranceMode::Release)),
            Ok(())
        );

        // The deployment's own run: INV-12's store case failed there.
        let run = without_gaps(fixture(
            Outcome::Pass,
            &[(
                "inv_12_an_observation_coordinate_applies_at_most_once__store",
                Outcome::Fail("applied twice".into()),
            )],
        ));
        let err = verify(&declared, &run, &inputs(&w, AssuranceMode::Release)).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.starts_with("INV-12 is declared enforced")),
            "{err:?}"
        );

        // A hand-edited claim of a layer no case shows.
        let mut inflated = declared.clone();
        inflated
            .invariants
            .iter_mut()
            .find(|c| c.id == "INV-8")
            .unwrap()
            .realised_layers
            .push(Layer::Store);
        let err = verify(&inflated, &green, &inputs(&w, AssuranceMode::Release)).unwrap_err();
        assert!(err.iter().any(|e| e.starts_with("INV-8 claims")), "{err:?}");
    }

    #[test]
    fn the_declaration_round_trips_as_json() {
        let w = world();
        let d = generate(
            &fixture(Outcome::Pending(Feature::F003), &[]),
            &inputs(&w, AssuranceMode::Milestone),
        )
        .unwrap();
        let json = serde_json::to_string_pretty(&d).unwrap();
        assert_eq!(serde_json::from_str::<Declaration>(&json).unwrap(), d);
        assert!(json.contains("\"schema\": \"aledgent/assurance/v1\""));
    }
}
