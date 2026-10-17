// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The library's write surface (`F-013` T-013-09, `REQ-CS-5`, `REQ-AT-3`, `US-K4`).
//!
//! "The library exposes no API that writes a leaf, a balance or an operation state outside a
//! command." Two checks, both in `cargo test` and so in CI:
//!
//! - **By search.** Every public function of [`Ledger`] that takes `&mut self` is listed in
//!   [`SURFACE`], committed here, each a command of SPEC §8.1, an operator command of `CTR-OPS`,
//!   or the host's own scheduled work. A new one fails this test until it is reviewed and added.
//!   No public function anywhere in the crate hands out `&mut` to the service store or the book.
//! - **By the compiler.** The service store and the book are reachable only by shared reference
//!   ([`Ledger::state`], [`Ledger::book`]), so their writes cannot be called through a ledger:
//!   the `compile_fail` examples on [`Ledger`]'s documentation, run as doctests.
//!
//! [`Ledger`]: ledger_core::Ledger
//! [`Ledger::state`]: ledger_core::Ledger::state
//! [`Ledger::book`]: ledger_core::Ledger::book

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Every public `&mut self` function of `Ledger`, by name, and why it may write.
const SURFACE: &[(&str, &str)] = &[
    // SPEC §8.1 commands (`CTR-CMD`).
    ("open_identity", "command"),
    (
        "import_credentials",
        "operator command: credential import (REQ-MG-7, amendment 0010, F-028)",
    ),
    (
        "set_identity_status",
        "command (the operator's policy authority, F-003)",
    ),
    ("open_account", "command"),
    (
        "apply_spec_chunk",
        "operator path: one chunk of the seed genesis committed, through the same commands, at genesis only (F-036, REQ-RC-6, amendment 0022 draft)",
    ),
    ("close_account", "command"),
    ("grant_authority", "command"),
    ("revoke_authority", "command"),
    ("issue", "command"),
    ("redeem", "command"),
    ("reserve", "command"),
    ("reserve_until", "command"),
    ("capture", "command"),
    ("release", "command"),
    ("transfer", "command (F-008)"),
    (
        "issue_disclosure",
        "CTR-DSC command: an audited disclosure record; moves no value (F-017)",
    ),
    (
        "anchor_checkpoint",
        "CTR-OPS operator command: a checkpoint on demand, audited (F-017)",
    ),
    (
        "compare_settlement",
        "CTR-OPS operator command: a settlement comparison on request, audited (F-017)",
    ),
    (
        "receive_transfer",
        "a transfer's intake at Received, before its decision; moves no value (F-009, DEC-5)",
    ),
    (
        "receive_command",
        "a transfer's or reservation's intake at Received, before its decision; moves no value (amendment 0007)",
    ),
    ("withdraw", "command (F-010)"),
    ("submit_outbound", "command (F-010)"),
    (
        "configure_settlement",
        "the operator's policy authority, set once (F-010)",
    ),
    // `CTR-SET` (F-010): the settlement adapter submits evidence at coordinates, never an
    // outcome (INV-11); aledgent derives every state from it, only from the authoritative source.
    (
        "declare_support",
        "the adapter's declaration (F-010, REQ-SA-1)",
    ),
    ("acknowledge", "the adapter's acknowledgement (F-010)"),
    ("record_observation", "evidence at a coordinate (F-010)"),
    ("verify", "a canonical check (F-010)"),
    (
        "evidence_checked",
        "settlement adapter input (REQ-RC-5 step 2): the outcome follows the proof",
    ),
    (
        "record_signing",
        "the adapter's record of the one transaction it signed, or a tombstone (F-010, F-016; owner ruling 2026-09-24)",
    ),
    (
        "observe_head",
        "the finalised head and the signing keys' nonces (F-010)",
    ),
    ("record_gap", "a skipped range, recorded (F-010, REQ-SA-7)"),
    ("rescan", "a re-scan against an archive (F-010, REQ-IN-5)"),
    (
        "adapter",
        "the settlement adapter's three capabilities, as its principal only (F-016, REQ-SA-8)",
    ),
    // F-012: the credential set, custody switching, recovery and succession (SPEC §8.1).
    ("register_credential", "command (F-012)"),
    ("remove_credential", "command (F-012)"),
    ("delegate_custody", "command (F-012)"),
    ("reclaim_custody", "command (F-012)"),
    ("cancel_recovery", "command (F-012)"),
    ("name_successor", "command (F-012)"),
    ("claim_succession", "command (F-012)"),
    (
        "initiate_recovery",
        "operator command, with the frame's justification (F-012)",
    ),
    (
        "complete_recovery",
        "operator command, carrying the recovery authority's approval (F-012)",
    ),
    // `CTR-OPS` (F-013): each through the operator frame.
    ("grant_operator", "operator command"),
    (
        "register_integrating_party",
        "operator command: the operator's policy authority alone (REQ-ID-7, amendment 0011)",
    ),
    (
        "tie_principal",
        "operator command: the operator's policy authority alone (REQ-ID-7, amendment 0011)",
    ),
    (
        "register_alias",
        "operator command: the operator's policy authority alone (REQ-RC-7, amendment 0024)",
    ),
    ("revoke_operator", "operator command"),
    ("resume", "operator command"),
    ("resolve_operation", "operator command"),
    (
        "submit_evidence",
        "operator command (REQ-RC-5 step 2); moves nothing",
    ),
    ("propose_reversal", "operator command"),
    ("approve_reversal", "operator command"),
    ("freeze_account", "operator command"),
    ("unfreeze_account", "operator command"),
    ("register_asset", "operator command"),
    ("bind_asset", "issuance authority command"),
    ("deprecate_asset", "operator command"),
    ("grant_issuance", "operator command"),
    ("revoke_issuance", "operator command"),
    ("declare_adapter_support", "operator command"),
    (
        "privileged_balances",
        "operator read: writes only its audit record (REQ-PV-4)",
    ),
    // `CTR-OPS` (F-014).
    (
        "load_opening_balances",
        "operator command: the zero-divergence gate, then Opening postings (REQ-RC-4)",
    ),
    // F-018: reconciliation axis 1(c).
    (
        "reconcile_external",
        "privileged read (REQ-PV-4): every balance against an external source, mismatches only; writes only its audit record",
    ),
    (
        "export_book",
        "operator read: writes only its audit record (REQ-PV-4, REQ-RC-3)",
    ),
    // The host's scheduled work: no principal, nothing but what the lifecycle derives.
    ("tick", "deadlines from the time input (REQ-RT-5)"),
    (
        "tick_due",
        "the core Agent's replay of a tick: only due entries of the deadline index, at most \
         max_fires_per_tick, the replica's choice (finding F18 of F-032, REQ-RT-1)",
    ),
    (
        "observe_head_chosen",
        "the core Agent's replay of a finalised head: only instructions in their indexes, at most \
         MAX_HEAD_NONCES and MAX_HEAD_LEGS, the replica's choice (finding F18's class, REQ-RT-1)",
    ),
    (
        "adapter_chosen",
        "the core Agent's replay of an adapter input, with a head's work as the replica chose it \
         (finding F18's class, REQ-RT-1); the adapter's principal only, as adapter",
    ),
    ("ship_audit", "copies audit records to the audit store"),
    (
        "acknowledge_audit",
        "drops from the audit outbox the records the audit store confirmed (T-013-13); writes \
         no leaf",
    ),
    // F-005: the record of the calls a run made, for the core Agent's driver. Neither writes the
    // book: they turn the recorder on and hand back what it holds.
    ("record_calls", "turns call recording on or off (F-005)"),
    ("take_calls", "takes the recorded calls (F-005)"),
    // T-037-03: the writer's code version and mode, from its hot row, which a checkpoint binds.
    // Not service state and no write: it changes what the next checkpoint's commitment binds,
    // never a balance, a record or a leaf.
    (
        "set_code",
        "names the writer's code version and mode a checkpoint binds (T-037-03)",
    ),
    // T-031-19: when the kernel's leaves are committed, not what is written. Every command still
    // decides alone, through the command surface; these only group the kernel's commits.
    (
        "begin_batch",
        "groups the kernel commits of the commands that follow (T-031-19)",
    ),
    (
        "end_batch",
        "commits the grouped kernel leaves as one commit (T-031-19)",
    ),
    // F-005: the core Agent's side of a privileged read the host ran: the same audit record,
    // through the same operator checks, and nothing else. It reads and returns no data.
    (
        "replay_audited_read",
        "commits a privileged read's audit record inside the core Agent (REQ-PV-4, F-005)",
    ),
    ("check_audit_head", "raises an incident on an audit gap"),
    // The conformance suite's raw engine, compiled only under `conformance-ports`, which no
    // production crate may enable (tools/ci/check-conformance-ports.sh).
    ("port_receive", "conformance-ports only"),
    ("port_arrive", "conformance-ports only"),
    (
        "port_fault_next_commit",
        "conformance-ports only: ERR-Internal's case (REQ-ER-2)",
    ),
    ("port_fire", "conformance-ports only"),
];

fn sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).expect("a source directory").flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
    out
}

/// The public functions taking `&mut self` inside `impl … Ledger<S, V>` blocks of `text`.
fn mutating_ledger_fns(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_ledger = false;
    let mut signature: Option<(String, String)> = None;
    for line in text.lines() {
        if line.starts_with("impl") && line.contains("Ledger<S, V>") {
            in_ledger = true;
            continue;
        }
        if in_ledger && line == "}" {
            in_ledger = false;
            continue;
        }
        if !in_ledger {
            continue;
        }
        if let Some((name, mut sig)) = signature.take() {
            sig.push_str(line);
            if line.contains('{') || line.trim_end().ends_with(';') {
                if sig.contains("&mut self") {
                    out.push(name);
                }
            } else {
                signature = Some((name, sig));
            }
            continue;
        }
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("pub fn ") {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if line.contains('{') {
                if line.contains("&mut self") {
                    out.push(name);
                }
            } else {
                signature = Some((name, line.to_string()));
            }
        }
    }
    out
}

#[test]
fn req_cs_5_no_write_path_outside_the_commands() {
    let mut found = BTreeSet::new();
    for f in sources() {
        let text = fs::read_to_string(&f).expect("a readable source file");
        found.extend(mutating_ledger_fns(&text));
        // Nothing hands out the service store or the book to write.
        for forbidden in [
            "-> &mut State",
            "-> &mut Book",
            "-> &mut crate::state::State",
        ] {
            assert!(
                !text
                    .lines()
                    .any(|l| l.contains("pub fn") && l.contains(forbidden)),
                "{} exposes {forbidden}",
                f.display()
            );
        }
    }
    let expected: BTreeSet<String> = SURFACE.iter().map(|(n, _)| (*n).to_string()).collect();
    assert_eq!(
        found, expected,
        "Ledger's write surface changed (REQ-CS-5, REQ-AT-3): a public `&mut self` function was \
         added or removed. Review it, then update SURFACE in this file."
    );
}

#[test]
fn the_search_finds_multi_line_signatures() {
    let text = "impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {\n    pub fn a(&mut self) {\n    }\n    pub fn b(\n        &mut self,\n        x: u8,\n    ) -> u8 {\n    }\n    pub fn c(&self) {}\n}\nimpl Other {\n    pub fn d(&mut self) {}\n}\n";
    assert_eq!(mutating_ledger_fns(text), vec!["a", "b"]);
}
