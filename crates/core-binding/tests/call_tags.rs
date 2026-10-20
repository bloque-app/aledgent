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
    variants_of::<ledger_core::call::Call>()
}

/// The variant names serde derives for the enum `T`, in declaration order.
fn variants_of<T: serde::de::DeserializeOwned + core::fmt::Debug>() -> Vec<String> {
    let e = serde_json::from_str::<T>(r#""__not_a_variant__""#)
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
    // `T-037-05`: the upgrade-authority inputs, one variant whose input says which.
    "UpgradeAuthority",
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
    // The upgrade-authority inputs, after them (`T-037-05`).
    assert_eq!(tag("UpgradeAuthority"), 66);
}

/// `T-037-05`: inside `Call::UpgradeAuthority`, the four inputs, the kinds an approval names and
/// the modes' rules keep their tags too: postcard writes each as its index, under the log hash.
/// Appended to, never reordered.
#[test]
fn t037_05_the_upgrade_authority_tags_are_pinned() {
    use ledger_core::code::authority::{Input, Kind, Rule};
    assert_eq!(
        variants_of::<Input>(),
        [
            "SetAuthority",
            "ProposeUpgrade",
            "ApproveUpgrade",
            "ProposeMigration"
        ]
    );
    assert_eq!(
        variants_of::<Kind>(),
        ["Upgrade", "Migration", "SetAuthority"]
    );
    assert_eq!(
        variants_of::<Rule>(),
        ["Single", "ProposeApprove", "Threshold"]
    );
}

/// Review of #45 at `4cc6ae9`, F4: the bytes a ring entry holds for the upgrade-authority inputs,
/// pinned. Postcard writes struct fields in declaration order, so reordering a field of `Member`,
/// `Mode` or an `Input` variant would change every log hash over them; this fails first.
#[test]
fn t037_05_the_upgrade_authority_call_bytes_are_pinned() {
    use ledger_core::call::Call;
    use ledger_core::code::authority::{Input, Kind, Member, Mode, Rule};
    use ledger_core::id::{CredentialId, IdentityId, OperationId};
    use ledger_core::ledger::Ctx;
    use ledger_core::principal::Caller;
    use ledger_kernel::posting::OperationRef;
    let op = |n: u8| OperationId::from_ref(OperationRef([n; 16]));
    let m = |n: u8| Member {
        principal: IdentityId::mint(&op(n), 0),
        verifier: [n; 32],
    };
    let ctx = Ctx {
        caller: Caller::new(IdentityId::mint(&op(1), 0), CredentialId::mint(&op(1), 1)),
        op: op(9),
        now: 1_234,
    };
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let set = Call::UpgradeAuthority {
        ctx,
        input: Input::SetAuthority {
            mode: Mode {
                rule: Rule::ProposeApprove {
                    proposers: [m(1)].into(),
                    approvers: [m(2)].into(),
                },
                delay: 7,
            },
            from: 90,
        },
        justification: b"why".to_vec(),
    };
    let approve = Call::UpgradeAuthority {
        ctx,
        input: Input::ApproveUpgrade {
            proposal: op(5),
            kind: Kind::Migration,
        },
        justification: b"why".to_vec(),
    };
    assert_eq!(hex(&core_binding::envelope::encode(&set)), SET_BYTES);
    assert_eq!(
        hex(&core_binding::envelope::encode(&approve)),
        APPROVE_BYTES
    );
}

/// [`t037_05_the_upgrade_authority_call_bytes_are_pinned`]'s change of mode.
const SET_BYTES: &str = concat!(
    "42206bde34e7e2b732f4c3913adb8774741ac3bb219d28ec4419f2d1509c5959",
    "c60009090909090909090909090909090909d209000101206bde34e7e2b732f4",
    "c3913adb87747401010101010101010101010101010101010101010101010101",
    "0101010101010101aee10d93d1696c9642da89479c1ab07c0202020202020202",
    "020202020202020202020202020202020202020202020202075a03776879",
);

/// [`t037_05_the_upgrade_authority_call_bytes_are_pinned`]'s approval.
const APPROVE_BYTES: &str = concat!(
    "42206bde34e7e2b732f4c3913adb8774741ac3bb219d28ec4419f2d1509c5959",
    "c60009090909090909090909090909090909d209020505050505050505050505",
    "05050505050103776879",
);
