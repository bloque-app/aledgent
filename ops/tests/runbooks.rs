// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The runbooks (`T-018-08`, `REQ-AT-3`, `REQ-IN-5`, `REQ-SA-7`, `NFR-9`): each exists, each alert
//! that names a runbook names one that exists, each says how it is exercised, and none changes
//! storage outside the command surface — no SQL write, no edit of a store's files.

use std::path::Path;

use ops::alerts::Rules;

const RUNBOOKS: [&str; 21] = [
    "restore.md",
    "anchoring.md",
    "gap-filling.md",
    "key-loss.md",
    "crank-outage.md",
    "reconciliation-failure.md",
    "release/audit-shipping.md",
    "release/core-archive.md",
    "release/core-halt.md",
    "release/divergence.md",
    "release/key-rotation.md",
    "release/kreivo-finality-stall.md",
    "release/partition-lag.md",
    "release/r03-provenance.md",
    "release/replace-node.md",
    "release/restore-drill.md",
    "release/scale-out-in.md",
    "release/settlement-adapter-outage.md",
    "release/total-loss-recovery.md",
    "release/vos-authority-backlog.md",
    "system-accounts.md",
];

fn dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/runbooks"))
}

#[test]
fn req_at_3_no_runbook_changes_storage_outside_the_command_surface() {
    for name in RUNBOOKS {
        let text = std::fs::read_to_string(dir().join(name)).unwrap();
        assert!(
            text.contains("## Exercised"),
            "{name} says how it is exercised"
        );
        assert!(
            text.contains("REQ-AT-3") || text.contains("CTR-OPS"),
            "{name}"
        );
        let upper = text.to_uppercase();
        for forbidden in [
            "UPDATE ",
            "DELETE FROM",
            "INSERT INTO",
            "TRUNCATE",
            "ALTER TABLE",
            "DROP ",
        ] {
            assert!(
                !upper.contains(forbidden),
                "{name} writes storage: {forbidden}"
            );
        }
        for forbidden in ["sed -i", "rm -rf", "redb", "seg-0"] {
            assert!(
                !text.contains(forbidden),
                "{name} edits a store's files: {forbidden}"
            );
        }
    }
}

#[test]
fn every_alert_that_names_a_runbook_names_one_that_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for r in Rules::committed().unwrap().0 {
        if let Some(path) = r.annotations.get("runbook") {
            assert!(root.join(path).is_file(), "{}: {path}", r.alert);
        }
    }
    for name in RUNBOOKS {
        assert!(dir().join(name).is_file());
    }
}
