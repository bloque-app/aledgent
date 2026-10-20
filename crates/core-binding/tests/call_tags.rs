// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The production `Call` variants' tags, pinned (review of #44, N2; `REQ-RT-3`, `REQ-RT-17`).
//!
//! A ring entry encodes its calls with postcard, which writes a variant as its index in the enum. So
//! a variant's position is part of the log hash and of every archived entry: a reorder, or two
//! changes that each append a variant (#44 and #45 both add theirs after `ApplySpecChunk`), would
//! give a variant another tag than the one the archive holds. This test reads the variants in
//! declaration order from serde's own list and pins every production one by name and index. The
//! conformance suite's raw ports may only follow them.

use serde::de::{self, Visitor};

/// The variant names serde derives for `Call`, in declaration order: what `deserialize_enum` is
/// handed. Nothing is decoded.
struct Variants(Option<&'static [&'static str]>);

#[derive(Debug)]
struct Stop;

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stop")
    }
}

impl std::error::Error for Stop {}

impl de::Error for Stop {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Stop
    }
}

impl<'de> de::Deserializer<'de> for &mut Variants {
    type Error = Stop;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
        Err(Stop)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        variants: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Stop> {
        self.0 = Some(variants);
        Err(Stop)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option
        unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

/// Every production variant of `ledger_core::call::Call`, at its tag (its index). A new variant is
/// appended here, after the last, by the change that adds it; never reordered.
const PRODUCTION: &[&str] = &[
    "OpenIdentity",
    "SetIdentityStatus",
    "OpenAccount",
    "CloseAccount",
    "GrantAuthority",
    "RevokeAuthority",
    "AnchorCheckpoint",
    "CompareSettlement",
    "RegisterCredential",
    "RemoveCredential",
    "DelegateCustody",
    "ReclaimCustody",
    "InitiateRecovery",
    "CancelRecovery",
    "CompleteRecovery",
    "NameSuccessor",
    "ClaimSuccession",
    "IssueDisclosure",
    "Issue",
    "Redeem",
    "Tick",
    "Resume",
    "Reserve",
    "ReserveUntil",
    "Capture",
    "Release",
    "Adapter",
    "ConfigureSettlement",
    "DeclareSupport",
    "Withdraw",
    "SubmitOutbound",
    "Acknowledge",
    "RecordObservation",
    "Verify",
    "EvidenceChecked",
    "RecordSigning",
    "ObserveHead",
    "RecordGap",
    "Rescan",
    "Transfer",
    "ReceiveTransfer",
    "ReceiveCommand",
    "DeclareAdapterSupport",
    "FreezeAccount",
    "UnfreezeAccount",
    "GrantOperator",
    "RevokeOperator",
    "LoadOpeningBalances",
    "RegisterAsset",
    "DeprecateAsset",
    "BindAsset",
    "GrantIssuance",
    "RevokeIssuance",
    "ResolveOperation",
    "SubmitEvidence",
    "ImportCredentials",
    "RegisterIntegratingParty",
    "TiePrincipal",
    "RegisterAlias",
    "ProposeReversal",
    "ApproveReversal",
    "PrivilegedRead",
    "Unsupported",
    "ApplySpecChunk",
    // `T-037-15`: the way out of a scheduled upgrade.
    "RescheduleUpgrade",
    "CancelUpgrade",
];

/// The conformance suite's raw ports: only ever after every production variant.
const PORTS: &[&str] = &["PortReceive", "PortArrive", "PortFire"];

/// Review of #44, N2: every production `Call` variant keeps its tag, and nothing but the
/// conformance ports follows them.
#[test]
fn t037_15_the_production_call_tags_are_pinned() {
    let mut v = Variants(None);
    let _ = <ledger_core::call::Call as serde::Deserialize>::deserialize(&mut v);
    let all = v.0.expect("Call is an enum");
    assert!(all.len() >= PRODUCTION.len(), "{all:?}");
    for (tag, name) in PRODUCTION.iter().enumerate() {
        assert_eq!(all[tag], *name, "tag {tag}");
    }
    for name in &all[PRODUCTION.len()..] {
        assert!(
            PORTS.contains(name),
            "{name} follows the production variants: append it to PRODUCTION"
        );
    }
    // The two way outs, by number, as the archive holds them.
    let tag = |name: &str| all.iter().position(|n| *n == name).unwrap();
    assert_eq!(
        (
            tag("ApplySpecChunk"),
            tag("RescheduleUpgrade"),
            tag("CancelUpgrade")
        ),
        (63, 64, 65)
    );
}
