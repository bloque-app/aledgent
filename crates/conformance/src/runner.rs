// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The runner (`F-015` §2, T-015-02): `cargo test -p conformance`.
//!
//! A custom test harness (`harness = false`, `libtest-mimic`) that runs, in
//! order of the listing:
//!
//! 1. `catalogue::is_current`: the checked-in catalogue matches `SPEC.md`;
//! 2. `closure::*`: the closure checks;
//! 3. `gate::<profile>::*`: the profile is not vacuous;
//! 4. `<target>::<id>::<case>`: every registered case on every target.
//!
//! The profile is `CONFORMANCE_PROFILE`, or `gates.toml`'s `current`. A
//! pending case the profile does not require is listed as ignored, with its
//! owner as the test kind; `--include-ignored` runs it, and it fails as
//! pending. `CONFORMANCE_REPORT_ONLY=1` writes the report and exits 0
//! whatever failed: CI uses it to publish the full run.
//!
//! The report goes to `CONFORMANCE_REPORT`, or to `conformance/report.json`
//! in the target directory.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use libtest_mimic::{Arguments, Failed, Trial};

use crate::case::{Body, Case, Outcome};
use crate::cases::registry;
use crate::closure;
use crate::gates::{Gates, Profile};
use crate::harness::{Target, targets};
use crate::report::{REPORT_SCHEMA, Report, Row, Tag};
use crate::spec::{CATALOGUE_TOML, Catalogue, Floor, SPEC_MD, check_current};

thread_local! {
    /// The last panic on this thread, with where it happened (see [`capture_panics`]).
    static LAST_PANIC: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Keep each panic's message and location for the thread it happened on, so a failing case
/// reports where it panicked as well as why (issue #48). The previous hook still runs.
pub fn capture_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let at = info
                .location()
                .map(|l| format!(" at {}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_default();
            let why = panic_message(info.payload());
            LAST_PANIC.with(|p| *p.borrow_mut() = Some(format!("panicked{at}: {why}")));
            previous(info);
        }));
    });
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "the case panicked with a payload that is not a message".into())
}

/// Run one case on a fresh harness of `target`. A panic is a failure, and its message and
/// location are the failure's detail.
pub fn run_case(case: &Case, target: &Target) -> Outcome {
    let body = match case.body {
        Body::Pending => return Outcome::Pending(case.owner),
        Body::Run(body) => body,
    };
    LAST_PANIC.with(|p| p.borrow_mut().take());
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut harness = (target.make)();
        if !harness.offers(case.needs) {
            return Outcome::NotRunnableHere;
        }
        let outcome = body(&mut *harness);
        if outcome == Outcome::Pass && !harness.exercised() {
            return Outcome::NotRunnableHere;
        }
        outcome
    }));
    match result {
        Ok(outcome) => outcome,
        Err(payload) => Outcome::Fail(
            LAST_PANIC
                .with(|p| p.borrow_mut().take())
                .unwrap_or_else(|| format!("panicked: {}", panic_message(&*payload))),
        ),
    }
}

/// The failures of a run, one per line with its detail, for the end of the log (issue #48).
pub fn failures(report: &Report) -> String {
    report
        .results
        .iter()
        .filter(|r| r.outcome == Tag::Fail)
        .map(|r| {
            let detail = r.detail.as_deref().unwrap_or("no detail");
            format!(
                "conformance: FAILED {}::{}::{}: {}",
                r.target,
                r.id,
                r.case,
                detail.replace('\n', "\n    ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a case's outcome means for the run under `required`, on a target that offers every
/// port (`complete`) or not ([`Target::complete`]).
fn judge_on(
    outcome: &Outcome,
    required: bool,
    profile: &str,
    complete: bool,
) -> Result<(), Failed> {
    if !complete && *outcome == Outcome::NotRunnableHere {
        return Ok(());
    }
    match outcome {
        Outcome::Pass => Ok(()),
        Outcome::Fail(why) => Err(why.as_str().into()),
        Outcome::Pending(f) if required => {
            Err(format!("pending on {f}, and profile {profile} requires it").into())
        }
        Outcome::Pending(f) => Err(format!("pending on {f}").into()),
        Outcome::NotRunnableHere if required => Err(format!(
            "this target does not offer the case's port, and profile {profile} requires it"
        )
        .into()),
        Outcome::NotRunnableHere => Ok(()),
    }
}

/// The git commit the suite runs from, or `unknown`.
pub fn suite_rev() -> String {
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        return sha;
    }
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Where the report goes.
pub fn report_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CONFORMANCE_REPORT") {
        return PathBuf::from(p);
    }
    // The test binary is <target-dir>/<profile>/deps/suite-<hash>.
    let target_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.parent()?.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target_dir.join("conformance").join("report.json")
}

fn trial_name(target: &str, case: &Case) -> String {
    format!("{target}::{}::{}", case.id, case.name)
}

/// Build the trials, and the rows they record into.
pub fn trials(
    cat: &Catalogue,
    floor: &Floor,
    profile_name: &str,
    profile: &Profile,
    cases: &[Case],
    targets: &[Target],
    rows: &Arc<Mutex<Vec<Row>>>,
) -> Vec<Trial> {
    let mut out = Vec::new();
    out.push(Trial::test("catalogue::is_current", || {
        check_current(SPEC_MD, CATALOGUE_TOML).map_err(Failed::from)
    }));
    for (name, verdict) in closure::run_all(cat, cases) {
        out.push(Trial::test(format!("closure::{name}"), move || {
            verdict.map_err(|f| f.join("\n").into())
        }));
    }
    let gate = profile.check(cat, floor, cases);
    out.push(Trial::test(
        format!("gate::{profile_name}::every_listed_invariant_has_a_required_case"),
        move || gate.map(|_| ()).map_err(|f| f.join("\n").into()),
    ));
    for target in targets {
        for case in cases {
            let required = profile.requires(case, cat, floor);
            let (case, target, rows) = (*case, *target, Arc::clone(rows));
            let profile_name = profile_name.to_string();
            let name = trial_name(target.name, &case);
            let trial = Trial::test(name.clone(), move || {
                let outcome = run_case(&case, &target);
                if let Outcome::Fail(why) = &outcome {
                    // Printed as it happens, not only in the list at the end (issue #48).
                    eprintln!(
                        "conformance: FAILED {name}: {}",
                        why.replace('\n', "\n    ")
                    );
                }
                rows.lock()
                    .expect("no case holds the lock while panicking")
                    .push(Row::new(&case, target.name, &outcome));
                judge_on(&outcome, required, &profile_name, target.complete)
            })
            .with_kind(case.owner.to_string())
            .with_ignored_flag(case.is_pending() && !required);
            out.push(trial);
        }
    }
    out
}

/// Fill in a row for every case and target the run did not reach.
pub fn complete(rows: &mut Vec<Row>, cases: &[Case], targets: &[Target]) {
    let seen: BTreeMap<(String, String), ()> = rows
        .iter()
        .map(|r| ((r.case.clone(), r.target.clone()), ()))
        .collect();
    for target in targets {
        for case in cases {
            if seen.contains_key(&(case.name.to_string(), target.name.to_string())) {
                continue;
            }
            rows.push(if case.is_pending() {
                Row::new(case, target.name, &Outcome::Pending(case.owner))
            } else {
                Row::skipped(case, target.name)
            });
        }
    }
    rows.sort_by(|a, b| (&a.target, &a.id, &a.case).cmp(&(&b.target, &b.id, &b.case)));
}

/// A summary for the log: outcomes, and pending cases per owner.
pub fn summary(report: &Report) -> String {
    let mut by_tag: BTreeMap<Tag, usize> = BTreeMap::new();
    let mut by_owner: BTreeMap<String, usize> = BTreeMap::new();
    for r in &report.results {
        *by_tag.entry(r.outcome).or_default() += 1;
        if r.outcome == Tag::Pending {
            *by_owner.entry(r.owner.to_string()).or_default() += 1;
        }
    }
    let tags: Vec<String> = by_tag.iter().map(|(t, n)| format!("{t:?} {n}")).collect();
    let owners: Vec<String> = by_owner.iter().map(|(o, n)| format!("{o} {n}")).collect();
    format!(
        "conformance: profile {}, {} rows over {} targets: {}.\nconformance: pending by owner: {}.",
        report.profile,
        report.results.len(),
        report.targets.len(),
        tags.join(", "),
        if owners.is_empty() {
            "none".into()
        } else {
            owners.join(", ")
        }
    )
}

/// The test binary's `main`.
pub fn main() {
    capture_panics();
    let args = Arguments::from_args();
    let cat = Catalogue::checked_in();
    let floor = Floor::checked_in();
    let gates = Gates::checked_in();
    let profile_name =
        std::env::var("CONFORMANCE_PROFILE").unwrap_or_else(|_| gates.current.clone());
    let profile = match gates.profile(&profile_name) {
        Ok(p) => p.clone(),
        Err(e) => {
            eprintln!("conformance: {e}");
            std::process::exit(2);
        }
    };
    let report_only = std::env::var("CONFORMANCE_REPORT_ONLY").is_ok_and(|v| v == "1");
    let cases = registry();
    let targets = targets();
    let rows = Arc::new(Mutex::new(Vec::new()));
    let listing = args.list;
    let trials = trials(
        &cat,
        &floor,
        &profile_name,
        &profile,
        &cases,
        &targets,
        &rows,
    );
    let conclusion = libtest_mimic::run(&args, trials);
    if listing {
        conclusion.exit();
    }

    let mut results = std::mem::take(&mut *rows.lock().unwrap_or_else(|e| e.into_inner()));
    complete(&mut results, &cases, &targets);
    let report = Report {
        schema: REPORT_SCHEMA.into(),
        spec: cat.spec_version.clone(),
        profile: profile_name,
        suite_rev: suite_rev(),
        targets: targets.iter().map(|t| t.name.to_string()).collect(),
        results,
    };
    let path = report_path();
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&report).expect("a report serialises"),
            )
        });
    match written {
        Ok(()) => println!("conformance: report written to {}", path.display()),
        Err(e) => eprintln!("conformance: cannot write {}: {e}", path.display()),
    }
    println!("{}", summary(&report));
    let failed = failures(&report);
    if !failed.is_empty() {
        println!("{failed}");
    }
    if report_only {
        std::process::exit(0);
    }
    conclusion.exit();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{Feature, Layer, Port};
    use crate::harness::Harness;

    fn panics(_: &mut dyn Harness) -> Outcome {
        panic!("the probe saw {} rows", 3)
    }

    fn fails(_: &mut dyn Harness) -> Outcome {
        Outcome::Fail("line one\nline two".into())
    }

    fn case(body: fn(&mut dyn Harness) -> Outcome) -> Case {
        Case::run(
            "AC-K3.1",
            "ac_k3_1_runner_self_test__service",
            Layer::Service,
            Feature::F015,
            Port::Service,
            &[],
            body,
        )
    }

    /// Issue #48: a panicking case fails with its message and where it panicked.
    #[test]
    fn a_panic_is_reported_with_its_message_and_location() {
        capture_panics();
        let target = targets()[0];
        let Outcome::Fail(why) = run_case(&case(panics), &target) else {
            panic!("a panicking case fails");
        };
        assert!(why.contains("the probe saw 3 rows"), "{why}");
        assert!(why.contains("runner.rs:"), "{why}");
    }

    /// Issue #48: the end of the log lists each failure with its detail.
    #[test]
    fn failures_are_listed_with_their_detail() {
        let target = targets()[0];
        let c = case(fails);
        let outcome = run_case(&c, &target);
        let report = Report {
            schema: REPORT_SCHEMA.into(),
            spec: "test".into(),
            profile: "test".into(),
            suite_rev: "test".into(),
            targets: vec![target.name.into()],
            results: vec![
                Row::new(&c, target.name, &outcome),
                Row::new(&c, "other", &Outcome::Pass),
            ],
        };
        let listed = failures(&report);
        assert_eq!(listed.lines().count(), 2, "{listed}");
        assert!(
            listed.starts_with(&format!(
                "conformance: FAILED {}::AC-K3.1::ac_k3_1_runner_self_test__service: line one",
                target.name
            )),
            "{listed}"
        );
        assert!(listed.ends_with("\n    line two"), "{listed}");
    }
}
