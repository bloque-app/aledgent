// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Gate profiles (`F-015` §10, T-015-09), read from `gates.toml`.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::case::{Case, Feature, Layer};
use crate::spec::{Catalogue, Floor};

/// The checked-in profiles.
pub const GATES_TOML: &str = include_str!("../gates.toml");

/// How the assurance declaration treats what is not yet enforced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssuranceMode {
    /// A milestone of the build: the declaration is written and the floor
    /// shortfall reported, but unmet invariants do not fail it.
    Milestone,
    /// A release: every invariant must be enforced or attested (`REQ-AR-5`)
    /// and the floor met (`REQ-AR-6`).
    Release,
}

/// Every profile, and the one CI runs.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gates {
    /// The profile of the current milestone.
    pub current: String,
    /// Profiles by name.
    #[serde(rename = "profile")]
    pub profiles: BTreeMap<String, Profile>,
}

/// One gate profile.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The milestone it gates.
    pub milestone: String,
    /// Every case is required.
    #[serde(default)]
    pub all: bool,
    /// The features whose cases may be required.
    #[serde(default)]
    pub features: Vec<String>,
    /// The invariants selected.
    #[serde(default)]
    pub invariants: Vec<String>,
    /// Whether errors owned by the profile's features are selected.
    #[serde(default)]
    pub errors: bool,
    /// The epics whose acceptance criteria are selected.
    #[serde(default)]
    pub epics: Vec<String>,
    /// Stories of those epics left out.
    #[serde(default)]
    pub except_stories: Vec<String>,
    /// Single acceptance criteria the spec contradicts, left out of the
    /// requirement while the owner rules. Each names the amendment item that
    /// rules on it. They stay selected, so the CI log lists them as deferred,
    /// and a case with a body still runs and still fails on a failure.
    #[serde(default)]
    pub awaiting_ruling: BTreeMap<String, String>,
    /// The layers a required case may be at; empty means any.
    #[serde(default)]
    pub layers: Vec<Layer>,
    /// How the assurance declaration is judged.
    pub assurance: AssuranceMode,
}

impl Gates {
    /// The checked-in profiles.
    pub fn checked_in() -> Gates {
        Gates::from_toml(GATES_TOML).expect("gates.toml parses")
    }

    /// Parse a profiles file.
    pub fn from_toml(text: &str) -> Result<Gates, String> {
        let g: Gates = toml::from_str(text).map_err(|e| e.to_string())?;
        if !g.profiles.contains_key(&g.current) {
            return Err(format!(
                "gates.toml: current profile {} is not defined",
                g.current
            ));
        }
        for (name, p) in &g.profiles {
            for (id, ruling) in &p.awaiting_ruling {
                if !id.starts_with("AC-") {
                    return Err(format!(
                        "gates.toml: profile {name} awaits a ruling on {id}; only single acceptance criteria may wait"
                    ));
                }
                if !ruling.contains("amendments/") {
                    return Err(format!(
                        "gates.toml: profile {name} awaits a ruling on {id} without naming the amendment that rules on it"
                    ));
                }
            }
            for f in &p.features {
                if Feature::parse(f).is_none() {
                    return Err(format!(
                        "gates.toml: profile {name} names {f}, not a feature"
                    ));
                }
            }
        }
        Ok(g)
    }

    /// A profile by name.
    pub fn profile(&self, name: &str) -> Result<&Profile, String> {
        self.profiles.get(name).ok_or_else(|| {
            format!(
                "no gate profile {name}; gates.toml defines {}",
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }
}

/// Whether a case exercises a layer SPEC §9 names but the first release,
/// following `REQ-AR-6`, declares the invariant at another one (`floor.toml`
/// `v0_layer`). Such a case is the §9 target: it stays registered, is
/// reported, and does not back or block the V0 claim.
pub fn is_target_layer_case(case: &Case, cat: &Catalogue, floor: &Floor) -> bool {
    let (Some(inv), Some(v0)) = (cat.invariant(case.id), floor.v0_layers_of(case.id)) else {
        return false;
    };
    inv.layers.contains(&case.layer) && !v0.layers.contains(&case.layer)
}

impl Profile {
    /// Whether the profile selects this catalogue id.
    pub fn selects(&self, id: &str, owner: Feature, cat: &Catalogue) -> bool {
        if self.all {
            return true;
        }
        if id.starts_with("INV-") {
            return self.invariants.iter().any(|i| i == id);
        }
        if id.starts_with("ERR-") {
            return self.errors && self.has_feature(owner);
        }
        match cat.acceptance(id) {
            Some(a) => self.epics.contains(&a.epic) && !self.except_stories.contains(&a.story),
            None => false,
        }
    }

    fn has_feature(&self, f: Feature) -> bool {
        self.features.iter().any(|s| Feature::parse(s) == Some(f))
    }

    /// Whether the profile requires this case to pass.
    pub fn requires(&self, case: &Case, cat: &Catalogue, floor: &Floor) -> bool {
        if self.all {
            return true;
        }
        self.has_feature(case.owner)
            && self.selects(case.id, case.owner, cat)
            && !self.awaiting_ruling.contains_key(case.id)
            && (self.layers.is_empty() || self.layers.contains(&case.layer))
            && !is_target_layer_case(case, cat, floor)
    }

    /// Check the profile is not vacuous: every invariant it lists has at
    /// least one case it requires. `Ok` carries the ids it selects whose
    /// cases all belong to later features, for the CI log.
    pub fn check(
        &self,
        cat: &Catalogue,
        floor: &Floor,
        cases: &[Case],
    ) -> Result<Vec<String>, Vec<String>> {
        let mut missing = Vec::new();
        for id in &self.invariants {
            if cat.invariant(id).is_none() {
                missing.push(format!("{id} is not in the catalogue"));
            } else if !cases
                .iter()
                .any(|c| c.id == id && self.requires(c, cat, floor))
            {
                missing.push(format!("{id} has no case this profile requires"));
            }
        }
        if !missing.is_empty() {
            return Err(missing);
        }
        let mut deferred: Vec<String> = cat
            .ids()
            .filter(|id| {
                let of_id: Vec<&Case> = cases.iter().filter(|c| c.id == *id).collect();
                of_id.iter().any(|c| self.selects(id, c.owner, cat))
                    && !of_id.iter().any(|c| self.requires(c, cat, floor))
                    && !id.starts_with("ERR-")
            })
            .map(String::from)
            .collect();
        deferred.dedup();
        Ok(deferred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cases::registry;

    #[test]
    fn every_profile_parses_and_m1_is_current() {
        let g = Gates::checked_in();
        assert_eq!(g.current, "m1");
        for name in ["m0", "m1", "full"] {
            assert!(g.profile(name).is_ok(), "{name}");
        }
        assert!(g.profile("m9").is_err());
    }

    #[test]
    fn m0_requires_exactly_f001s_kernel_store_type_and_architecture_cases() {
        let (g, cat, floor, cases) = (
            Gates::checked_in(),
            Catalogue::checked_in(),
            Floor::checked_in(),
            registry(),
        );
        let m0 = g.profile("m0").unwrap();
        let required: Vec<&Case> = cases
            .iter()
            .filter(|c| m0.requires(c, &cat, &floor))
            .collect();
        assert_eq!(required.len(), 10);
        assert!(
            required
                .iter()
                .all(|c| c.owner == Feature::F001 && !c.is_pending())
        );
        // The F-004 half of INV-3 and the F-003 store half of INV-7 are not
        // required at M-0.
        assert!(
            cases.iter().any(|c| c.id == "INV-3"
                && c.owner == Feature::F004
                && !m0.requires(c, &cat, &floor))
        );
        assert_eq!(m0.check(&cat, &floor, &cases), Ok(Vec::new()));
    }

    #[test]
    fn m1_leaves_target_layer_cases_and_later_features_out() {
        let (g, cat, floor, cases) = (
            Gates::checked_in(),
            Catalogue::checked_in(),
            Floor::checked_in(),
            registry(),
        );
        let m1 = g.profile("m1").unwrap();
        let by_name = |n: &str| cases.iter().find(|c| c.name == n).unwrap();
        // INV-16 at store is §9's target; REQ-AR-6 declares service for V0.
        let store16 = by_name("inv_16_an_unauthorised_read_is_unknown_account__store");
        assert!(is_target_layer_case(store16, &cat, &floor));
        assert!(!m1.requires(store16, &cat, &floor));
        let service16 = by_name("inv_16_an_unauthorised_read_is_unknown_account__service");
        assert!(m1.requires(service16, &cat, &floor));
        // A8 is left out; K3 belongs to F-009 (M-2) and is deferred.
        let deferred = m1.check(&cat, &floor, &cases).unwrap();
        assert!(deferred.iter().any(|d| d == "AC-K3.1"), "{deferred:?}");
        assert!(
            !deferred.iter().any(|d| d.starts_with("AC-A8")),
            "{deferred:?}"
        );
        assert!(
            !cases
                .iter()
                .filter(|c| c.id.starts_with("AC-A8"))
                .any(|c| m1.requires(c, &cat, &floor))
        );
        // Errors owned by M-1 features are required; F-016's are not.
        let err_batch = cases.iter().find(|c| c.id == "ERR-BatchTooLarge").unwrap();
        assert!(m1.requires(err_batch, &cat, &floor));
        let err_leg = cases.iter().find(|c| c.id == "ERR-UnsupportedLeg").unwrap();
        assert!(!m1.requires(err_leg, &cat, &floor));
    }

    /// A profile's `awaiting_ruling` as it was while amendment 0006 item 11 was open.
    fn awaiting_c3_2() -> String {
        GATES_TOML.replace(
            "except_stories = [\"US-A8\", \"US-A9\", \"US-C6\"]\n",
            "except_stories = [\"US-A8\", \"US-A9\", \"US-C6\"]\nawaiting_ruling = { \"AC-C3.2\" = \"amendments/0006-lifecycle-and-assurance-fixes.md item 11\" }\n",
        )
    }

    /// Amendment 0006 item 11 is ruled: `AC-C3.2` waits for nothing and `m1` requires it.
    #[test]
    fn ac_c3_2_is_required_again_once_ruled() {
        let (g, cat, floor, cases) = (
            Gates::checked_in(),
            Catalogue::checked_in(),
            Floor::checked_in(),
            registry(),
        );
        let m1 = g.profile("m1").unwrap();
        assert!(m1.awaiting_ruling.is_empty());
        assert!(
            cases
                .iter()
                .filter(|c| c.id == "AC-C3.2")
                .any(|c| m1.requires(c, &cat, &floor))
        );
    }

    #[test]
    fn a_criterion_awaiting_a_ruling_is_deferred_and_nothing_else_of_its_story() {
        let (g, cat, floor, cases) = (
            Gates::from_toml(&awaiting_c3_2()).unwrap(),
            Catalogue::checked_in(),
            Floor::checked_in(),
            registry(),
        );
        let m1 = g.profile("m1").unwrap();
        assert!(m1.awaiting_ruling["AC-C3.2"].contains("amendments/0006"));
        assert!(
            !cases
                .iter()
                .filter(|c| c.id == "AC-C3.2")
                .any(|c| m1.requires(c, &cat, &floor))
        );
        // The rest of US-C3 stays required.
        assert!(
            cases
                .iter()
                .filter(|c| c.id == "AC-C3.1")
                .any(|c| m1.requires(c, &cat, &floor))
        );
        let deferred = m1.check(&cat, &floor, &cases).unwrap();
        assert!(deferred.iter().any(|d| d == "AC-C3.2"), "{deferred:?}");
    }

    #[test]
    fn a_ruling_must_be_named_and_only_criteria_may_wait() {
        let unnamed = awaiting_c3_2().replace(
            "amendments/0006-lifecycle-and-assurance-fixes.md item 11",
            "the owner",
        );
        assert!(
            Gates::from_toml(&unnamed)
                .unwrap_err()
                .contains("without naming")
        );
        let invariant = awaiting_c3_2().replace("\"AC-C3.2\" =", "\"INV-21\" =");
        assert!(
            Gates::from_toml(&invariant)
                .unwrap_err()
                .contains("only single")
        );
    }

    #[test]
    fn a_vacuous_profile_fails_its_check() {
        let text = GATES_TOML.replace(
            "features = [\"F-001\", \"F-002\", \"F-015\"]",
            "features = [\"F-002\", \"F-015\"]",
        );
        let g = Gates::from_toml(&text).unwrap();
        let err = g
            .profile("m0")
            .unwrap()
            .check(&Catalogue::checked_in(), &Floor::checked_in(), &registry())
            .unwrap_err();
        assert_eq!(err.len(), 6, "{err:?}");
    }

    #[test]
    fn full_requires_everything() {
        let (g, cat, floor, cases) = (
            Gates::checked_in(),
            Catalogue::checked_in(),
            Floor::checked_in(),
            registry(),
        );
        let full = g.profile("full").unwrap();
        assert!(cases.iter().all(|c| full.requires(c, &cat, &floor)));
        assert_eq!(full.assurance, AssuranceMode::Release);
    }
}
