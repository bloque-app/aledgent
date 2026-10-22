// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Cases (`F-015` §2, T-015-02).
//!
//! A [`Case`] is one registered test: the spec id it verifies, the layer it
//! exercises (`REQ-AR-4`), the feature that owns it, the port it needs, and
//! its body. A case whose feature is not built yet has [`Body::Pending`]: it
//! is registered, attributed, and fails the full run, so the suite "exists
//! and fails" for it (PLAN §5, `M-0`).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::harness::Harness;

/// The layers SPEC §9 names, plus the two its "Enforced by" column adds for
/// `INV-7` (type) and `INV-17` (build gate).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layer {
    /// The accounting core makes a violation impossible to express.
    Kernel,
    /// The enforcement survives any application change.
    Store,
    /// aledgent's own logic.
    Service,
    /// A cryptographic property, not a check.
    Credential,
    /// The type system: the violation cannot be written down.
    Type,
    /// A build-time gate.
    BuildGate,
    /// A structural property of the deployment (`INV-25`).
    Architecture,
}

impl Layer {
    /// Every layer.
    pub const ALL: [Layer; 7] = [
        Layer::Kernel,
        Layer::Store,
        Layer::Service,
        Layer::Credential,
        Layer::Type,
        Layer::BuildGate,
        Layer::Architecture,
    ];

    /// The catalogue spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Layer::Kernel => "kernel",
            Layer::Store => "store",
            Layer::Service => "service",
            Layer::Credential => "credential",
            Layer::Type => "type",
            Layer::BuildGate => "build-gate",
            Layer::Architecture => "architecture",
        }
    }

    /// Parse the catalogue spelling.
    pub fn parse(s: &str) -> Option<Layer> {
        Layer::ALL.into_iter().find(|l| l.as_str() == s)
    }

    /// Whether this is the service layer, the one `REQ-AR-4` treats as
    /// weaker than the others. SPEC does not rank the others against each
    /// other, and neither does the suite (`F-015` Open points).
    pub const fn is_service(self) -> bool {
        matches!(self, Layer::Service)
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A feature of PLAN §4, `F-001`…`F-029`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Feature(pub u16);

impl Feature {
    /// Kernel binding.
    pub const F001: Feature = Feature(1);
    /// Identities, accounts, assets, authority.
    pub const F003: Feature = Feature(3);
    /// Reservation model.
    pub const F004: Feature = Feature(4);
    /// Leaf store and witness path.
    pub const F006: Feature = Feature(6);
    /// Operation lifecycle.
    pub const F007: Feature = Feature(7);
    /// Internal transfers and multi-leg.
    pub const F008: Feature = Feature(8);
    /// Ingress process.
    pub const F009: Feature = Feature(9);
    /// Evidence and settlement legs.
    pub const F010: Feature = Feature(10);
    /// Queries and statements.
    pub const F011: Feature = Feature(11);
    /// Custody switching, credentials and recovery.
    pub const F012: Feature = Feature(12);
    /// Audit trail and operator operations.
    pub const F013: Feature = Feature(13);
    /// Export, import, opening balances.
    pub const F014: Feature = Feature(14);
    /// Conformance suite.
    pub const F015: Feature = Feature(15);
    /// Settlement adapter.
    pub const F016: Feature = Feature(16);
    /// Anchoring and disclosure.
    pub const F017: Feature = Feature(17);
    /// Operations.
    pub const F018: Feature = Feature(18);
    /// The Shared (replicated) core: ephemeral horizontal scaling (amendment 0016, `NFR-21`).
    pub const F021: Feature = Feature(21);
    /// Credential schemes, and the credential import of amendment 0010.
    pub const F028: Feature = Feature(28);
    /// Book-only assets and their binding (amendment 0011, `DEC-21`).
    pub const F029: Feature = Feature(29);
    /// Seeded genesis (amendment 0022 draft, `REQ-RC-6`).
    pub const F036: Feature = Feature(36);
    /// Code history and upgrades (amendment 0023, `REQ-RT-15` to `REQ-RT-18`).
    pub const F037: Feature = Feature(37);
    /// Per-transaction approval at ingress (amendment 0037, `REQ-AU-23`).
    pub const F040: Feature = Feature(40);

    /// Whether PLAN §4 has this feature.
    pub const fn is_planned(self) -> bool {
        (self.0 >= 1 && self.0 <= 29) || self.0 == 36 || self.0 == 37 || self.0 == 40
    }

    /// Parse `F-004`.
    pub fn parse(s: &str) -> Option<Feature> {
        let n: u16 = s.strip_prefix("F-")?.parse().ok()?;
        (s.len() == 5).then_some(Feature(n))
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "F-{:03}", self.0)
    }
}

impl Serialize for Feature {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Feature {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Feature::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("not a feature: {s}")))
    }
}

/// The port a case drives (`F-015` §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Port {
    /// Raw kernel transfers, `ledger-core` bypassed.
    Kernel,
    /// Raw writes to a leaf store, the kernel bypassed.
    Store,
    /// The kernel binding's own API: posting drafts and units.
    Binding,
    /// `ledger-core` commands and queries, as a principal.
    Service,
    /// Raw writes to an empty `ledger-core` service store, the commands bypassed (`F-003`).
    ServiceStore,
    /// Nothing at run time: a build or source check.
    Build,
}

/// What a case's passing means for the assurance declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// The case shows the invariant holds at its layer.
    Check,
    /// The case pins a known gap: it passes while the layer does **not**
    /// enforce the invariant, so its passing denies a claim at that layer.
    /// When the gap closes, the case flips to a refusal and fails, and is
    /// then rewritten as a [`Kind::Check`].
    KnownGap,
}

/// A case's result on one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The case passed.
    Pass,
    /// The case failed, with why.
    Fail(String),
    /// The feature that owns the case is not built yet.
    Pending(Feature),
    /// The target does not offer the port the case needs. It cannot back an
    /// "enforced" claim for that target.
    NotRunnableHere,
}

/// A case's body.
#[derive(Clone, Copy)]
pub enum Body {
    /// Not written yet: the owning feature writes it.
    Pending,
    /// Run the case against a fresh harness. A panic is a failure.
    Run(fn(&mut dyn Harness) -> Outcome),
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Body::Pending => f.write_str("Pending"),
            Body::Run(_) => f.write_str("Run"),
        }
    }
}

/// One registered case.
#[derive(Clone, Copy, Debug)]
pub struct Case {
    /// The catalogue id it verifies: `INV-3`, `ERR-ZeroAmount`, `AC-B2.1`.
    pub id: &'static str,
    /// A unique name, `inv_3_…__kernel`, so tests can be found by id.
    pub name: &'static str,
    /// The layer it exercises (`REQ-AR-4`).
    pub layer: Layer,
    /// The feature that owns its body.
    pub owner: Feature,
    /// What its passing means.
    pub kind: Kind,
    /// Other ids it serves.
    pub traces: &'static [&'static str],
    /// The port it needs.
    pub needs: Port,
    /// Its body.
    pub body: Body,
}

impl Case {
    /// A case with a body.
    pub const fn run(
        id: &'static str,
        name: &'static str,
        layer: Layer,
        owner: Feature,
        needs: Port,
        traces: &'static [&'static str],
        body: fn(&mut dyn Harness) -> Outcome,
    ) -> Case {
        Case {
            id,
            name,
            layer,
            owner,
            kind: Kind::Check,
            traces,
            needs,
            body: Body::Run(body),
        }
    }

    /// A case its owner has not written yet. The port follows the layer.
    pub const fn pending(
        id: &'static str,
        name: &'static str,
        layer: Layer,
        owner: Feature,
        traces: &'static [&'static str],
    ) -> Case {
        let needs = match layer {
            Layer::Kernel | Layer::Architecture => Port::Kernel,
            Layer::Store => Port::Store,
            Layer::Type => Port::Binding,
            Layer::BuildGate => Port::Build,
            Layer::Service | Layer::Credential => Port::Service,
        };
        Case {
            id,
            name,
            layer,
            owner,
            kind: Kind::Check,
            traces,
            needs,
            body: Body::Pending,
        }
    }

    /// The same case, pinning a known gap.
    pub const fn known_gap(mut self) -> Case {
        self.kind = Kind::KnownGap;
        self
    }

    /// Whether the body is still to be written.
    pub const fn is_pending(&self) -> bool {
        matches!(self.body, Body::Pending)
    }
}
