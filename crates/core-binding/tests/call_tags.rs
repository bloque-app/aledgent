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

/// The variant names serde derives for `Call`, in declaration order: an unknown variant's error
/// lists them ("expected one of `A`, `B`, …"). Nothing is decoded.
fn variants() -> Vec<String> {
    let e = serde_json::from_str::<ledger_core::call::Call>(r#""__not_a_variant__""#)
        .expect_err("no such variant")
        .to_string();
    let list = e
        .split("expected one of ")
        .nth(1)
        .unwrap_or_else(|| panic!("{e}"));
    list.split(", ")
        .map(|v| {
            v.trim()
                .trim_end_matches(|c: char| c != '`')
                .trim_matches('`')
                .to_string()
        })
        .collect()
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
    // `T-037-04`: the upgrade itself.
    "Upgrade",
];

/// The conformance suite's raw ports: only ever after every production variant.
const PORTS: &[&str] = &["PortReceive", "PortArrive", "PortFire"];

/// Review of #44, N2: every production `Call` variant keeps its tag, and nothing but the
/// conformance ports follows them.
#[test]
fn t037_15_the_production_call_tags_are_pinned() {
    let all = variants();
    assert!(all.len() >= PRODUCTION.len(), "{all:?}");
    for (tag, name) in PRODUCTION.iter().enumerate() {
        assert_eq!(all[tag], *name, "tag {tag}");
    }
    for name in &all[PRODUCTION.len()..] {
        assert!(
            PORTS.contains(&name.as_str()),
            "{name} follows the production variants: append it to PRODUCTION"
        );
    }
    // The two way outs and the upgrade, by number, as the archive holds them.
    let tag = |name: &str| all.iter().position(|n| *n == name).unwrap();
    assert_eq!(
        (
            tag("ApplySpecChunk"),
            tag("RescheduleUpgrade"),
            tag("CancelUpgrade"),
            tag("Upgrade")
        ),
        (63, 64, 65, 66)
    );
}
