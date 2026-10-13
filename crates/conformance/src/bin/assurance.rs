// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The assurance declaration generator (`F-015` §9, T-015-08).
//!
//! ```text
//! assurance [generate] --report <report.json> --target <t>
//!           [--out <assurance.json>] [--mode milestone|release]
//!           [--baseline <baseline.json>] [--amendment amendments/NNNN-*.md]
//! assurance verify --declaration <assurance.json> --report <report.json>
//! ```
//!
//! `--mode` defaults to the current gate profile's (`gates.toml`). The
//! baseline is the last released declaration; a weakening against it fails
//! unless `--amendment` names a file in `amendments/` (`REQ-AR-4`).
//!
//! Exit codes: 0 written or verified, 1 the declaration fails its rules,
//! 2 the command could not run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use conformance::assurance::{
    Attestations, Declaration, Inputs, check_no_weakening, generate, verify,
};
use conformance::gates::{AssuranceMode, Gates};
use conformance::report::Report;
use conformance::spec::{Catalogue, Floor};

fn usage() -> ExitCode {
    eprintln!(
        "usage: assurance [generate] --report <r> --target <t> [--out <o>] [--mode milestone|release] \
         [--baseline <b>] [--amendment <a>]\n       assurance verify --declaration <d> --report <r>"
    );
    ExitCode::from(2)
}

fn read(path: &Path) -> Result<String, ExitCode> {
    std::fs::read_to_string(path).map_err(|e| {
        eprintln!("assurance: cannot read {}: {e}", path.display());
        ExitCode::from(2)
    })
}

fn fail(lines: &[String]) -> ExitCode {
    for l in lines {
        eprintln!("assurance: {l}");
    }
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let verifying = args.first().is_some_and(|a| a == "verify");
    if args
        .first()
        .is_some_and(|a| a == "verify" || a == "generate")
    {
        args.remove(0);
    }
    let mut opts = std::collections::BTreeMap::new();
    let mut it = args.into_iter();
    while let Some(k) = it.next() {
        let Some(v) = it.next() else { return usage() };
        match k.as_str() {
            "--report" | "--target" | "--out" | "--mode" | "--baseline" | "--amendment"
            | "--declaration" => {
                opts.insert(k, v);
            }
            _ => return usage(),
        }
    }
    let Some(report_path) = opts.get("--report").map(PathBuf::from) else {
        return usage();
    };
    let report = match read(&report_path).map(|t| Report::from_json(&t)) {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            eprintln!("assurance: {}: {e}", report_path.display());
            return ExitCode::from(2);
        }
        Err(code) => return code,
    };
    let catalogue = Catalogue::checked_in();
    let floor = Floor::checked_in();
    let attestations = Attestations::checked_in();
    let gates = Gates::checked_in();
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");

    if verifying {
        let Some(path) = opts.get("--declaration").map(PathBuf::from) else {
            return usage();
        };
        let declaration: Declaration = match read(&path).map(|t| serde_json::from_str(&t)) {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => {
                eprintln!("assurance: {}: {e}", path.display());
                return ExitCode::from(2);
            }
            Err(code) => return code,
        };
        let inputs = Inputs {
            catalogue: &catalogue,
            floor: &floor,
            attestations: &attestations,
            target: &declaration.target,
            mode: declaration.mode,
            kernel_rev: ledger_kernel::KERNEL_REV,
        };
        return match verify(&declaration, &report, &inputs) {
            Ok(()) => {
                println!(
                    "assurance: every enforced claim of {} is backed by the run.",
                    path.display()
                );
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        };
    }

    let Some(target) = opts.get("--target") else {
        return usage();
    };
    let mode = match opts.get("--mode").map(String::as_str) {
        Some("milestone") => AssuranceMode::Milestone,
        Some("release") => AssuranceMode::Release,
        Some(_) => return usage(),
        None => match gates.profile(&gates.current) {
            Ok(p) => p.assurance,
            Err(e) => {
                eprintln!("assurance: {e}");
                return ExitCode::from(2);
            }
        },
    };
    let inputs = Inputs {
        catalogue: &catalogue,
        floor: &floor,
        attestations: &attestations,
        target,
        mode,
        kernel_rev: ledger_kernel::KERNEL_REV,
    };
    let declaration = match generate(&report, &inputs) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    if let Some(path) = opts.get("--baseline").map(PathBuf::from) {
        let baseline: Declaration = match read(&path).map(|t| serde_json::from_str(&t)) {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => {
                eprintln!("assurance: {}: {e}", path.display());
                return ExitCode::from(2);
            }
            Err(code) => return code,
        };
        match check_no_weakening(
            &declaration,
            &baseline,
            opts.get("--amendment").map(String::as_str),
            &repo_root,
        ) {
            Ok(allowed) => {
                for a in allowed {
                    println!("assurance: weakening {a}");
                }
            }
            Err(e) => return fail(&e),
        }
    }
    let out = opts
        .get("--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| report_path.with_file_name("assurance.json"));
    let json = serde_json::to_string_pretty(&declaration).expect("a declaration serialises");
    if let Err(e) = std::fs::write(&out, json) {
        eprintln!("assurance: cannot write {}: {e}", out.display());
        return ExitCode::from(2);
    }
    let enforced = declaration
        .invariants
        .iter()
        .filter(|c| c.status == conformance::assurance::Status::Enforced)
        .map(|c| c.id.as_str())
        .collect::<Vec<_>>();
    println!(
        "assurance: wrote {} ({:?} mode, target {}): {} of {} invariants enforced: {}.",
        out.display(),
        declaration.mode,
        declaration.target,
        enforced.len(),
        declaration.invariants.len(),
        enforced.join(", ")
    );
    if declaration.floor.met {
        println!(
            "assurance: the floor of {} is met.",
            declaration.floor.basis
        );
    } else {
        println!(
            "assurance: the floor of {} is not met yet:\n  {}",
            declaration.floor.basis,
            declaration.floor.shortfall.join("\n  ")
        );
    }
    ExitCode::SUCCESS
}
