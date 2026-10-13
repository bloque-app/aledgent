// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `cargo xtask` for the conformance suite (`F-015` §1, T-015-01).
//!
//! ```text
//! cargo xtask spec-ids            # regenerate crates/conformance/spec/catalogue.toml
//! cargo xtask spec-ids --check    # fail if the checked-in catalogue is not current
//! cargo xtask spec-ids --spec <SPEC.md> --out <catalogue.toml> [--check]
//! ```
//!
//! Exit codes: 0 done or current, 1 not current, 2 the command could not run.

use std::path::PathBuf;
use std::process::ExitCode;

use conformance::spec::{check_current, parse_spec};

fn usage() -> ExitCode {
    eprintln!("usage: cargo xtask spec-ids [--check] [--spec <SPEC.md>] [--out <catalogue.toml>]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("spec-ids") {
        return usage();
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut spec = root.join("../../SPEC.md");
    let mut out = root.join("spec/catalogue.toml");
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--spec" => match args.next() {
                Some(p) => spec = PathBuf::from(p),
                None => return usage(),
            },
            "--out" => match args.next() {
                Some(p) => out = PathBuf::from(p),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let text = match std::fs::read_to_string(&spec) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("spec-ids: cannot read {}: {e}", spec.display());
            return ExitCode::from(2);
        }
    };
    if check {
        let current = std::fs::read_to_string(&out).unwrap_or_default();
        return match check_current(&text, &current) {
            Ok(()) => {
                println!("spec-ids: {} is current.", out.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("spec-ids: {e}");
                ExitCode::from(1)
            }
        };
    }
    let catalogue = match parse_spec(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("spec-ids: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = std::fs::write(&out, catalogue.render()) {
        eprintln!("spec-ids: cannot write {}: {e}", out.display());
        return ExitCode::from(2);
    }
    println!(
        "spec-ids: wrote {} ({} invariants, {} errors, {} acceptance criteria).",
        out.display(),
        catalogue.invariants.len(),
        catalogue.errors.len(),
        catalogue.acceptance.len()
    );
    ExitCode::SUCCESS
}
