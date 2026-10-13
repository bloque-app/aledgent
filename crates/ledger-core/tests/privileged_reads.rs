// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `REQ-PV-4`: "The number of distinct places that use the mechanism MUST be asserted against a
//! committed expected value. A new place then fails a build." (`F-013`, amendment 0006 item 9.)
//!
//! The one mechanism is `Ledger::privileged_read`, in `ledger/operator/mod.rs`. This test walks
//! every Rust source file of every crate in the workspace and counts the places that call it. The
//! expected value is [`EXPECTED`], committed here: adding a place fails `cargo test`, and so CI,
//! until this list is changed in review.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Every place that uses the privileged-read mechanism: (file, calls in it).
const EXPECTED: &[(&str, usize)] = &[
    // `Ledger::privileged_balances`: an operator's support read of an account's balances.
    ("crates/ledger-core/src/ledger/operator/mod.rs", 1),
    // `Ledger::export_book`: the whole book, for a re-import (`F-014`, `REQ-RC-3`).
    ("crates/ledger-core/src/ledger/operator/export.rs", 1),
    // `Ledger::reconcile_external`: every balance against an external source, mismatches only
    // (`F-018`, `NFR-9` axis 1(c)).
    ("crates/ledger-core/src/ledger/operator/external.rs", 1),
];

const MECHANISM: &str = "privileged_read(";

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if name != "target" && !name.starts_with('.') {
                walk(&p, out);
            }
        } else if name.ends_with(".rs") {
            out.push(p);
        }
    }
}

/// The calls of the mechanism in `text`: every occurrence that is not its definition, and not in
/// a comment.
fn calls(text: &str) -> usize {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .map(|l| {
            l.match_indices(MECHANISM)
                .filter(|(i, _)| !l[..*i].trim_end().ends_with("fn"))
                .count()
        })
        .sum()
}

#[test]
fn req_pv_4_the_privileged_read_mechanism_is_used_in_the_committed_places_only() {
    let root = workspace();
    let mut files = Vec::new();
    for top in ["crates", "agents", "services", "ops"] {
        walk(&root.join(top), &mut files);
    }
    let this = Path::new(file!())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        if f.file_name().and_then(|n| n.to_str()) == Some(this) {
            continue;
        }
        let text = fs::read_to_string(&f).expect("a readable source file");
        let n = calls(&text);
        if n > 0 {
            let rel = f
                .strip_prefix(&root)
                .expect("inside the workspace")
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(rel, n);
        }
    }
    let expected: BTreeMap<String, usize> = EXPECTED
        .iter()
        .map(|(f, n)| ((*f).to_string(), *n))
        .collect();
    assert_eq!(
        found, expected,
        "the places that use the privileged-read mechanism changed (REQ-PV-4): review the new \
         place and update EXPECTED in this file"
    );
}

#[test]
fn the_counter_ignores_the_definition_and_comments() {
    assert_eq!(calls("    fn privileged_read(\n"), 0);
    assert_eq!(calls("    // self.privileged_read(x)\n"), 0);
    assert_eq!(calls("        self.privileged_read(ctx, a)?;\n"), 1);
}
