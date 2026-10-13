<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-015 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-015-01 | `cargo xtask spec-ids`: read SPEC §9, §10 and the V0 `AC-*` lines, and write `spec/catalogue.toml`. Hand-write `spec/floor.toml` from `REQ-AR-6`. Add the CI check that regenerates and diffs | `NFR-14`, `REQ-ER-1`, `REQ-AR-6` | The catalogue holds 25 `INV-*` and 36 `ERR-*`. Removing a row from a SPEC copy makes CI fail |
| T-015-02 | The `Case` type, registration by distributed slice, and the custom runner that lists cases by id and writes `report.json` | `NFR-14` | `cargo test -p conformance` lists and runs registered cases. The report names each case's outcome |
| T-015-03 | The `Harness` trait, the ports, the `ClockPort`, and the `InProcess` harness over `F-001` (both stores) and `ledger-core`. Add the CI check that no production crate enables `conformance-ports` | `REQ-AR-4`, `REQ-CS-5`, `REQ-RT-7` | The kernel and store ports drive `F-001` directly. The CI check fails on a crate that enables the feature |
| T-015-04 | Closure meta-tests: every id has a case, every `INV-*` has a case at each named layer, and no case cites an unknown id | `NFR-14`, `REQ-AR-4` | The fixture crates in the test plan fail as expected |
| T-015-05 | Register one case per `INV-*`, `ERR-*` and V0 `AC-*` with its owner. Cases for unbuilt features return `Pending(owner)` | `NFR-14`, every `INV-*` and `ERR-*` | Closure passes. The full run is red, with every failure attributed to an owner |
| T-015-06 | The `refusal` module in `ledger-core`: a closed `Refusal` enum and its serialised shape. The two-way check against the catalogue, and the shape assertion used by every `ERR-*` case | `REQ-ER-1`, `REQ-CS-4` | Adding or removing a variant without a catalogue change fails. Every `ERR-*` case asserts `Err(Refusal)` with the expected id |
| T-015-07 | Shared tools: `proptest` strategies, `ShadowBook`, and `rederive(delta_log)` | `INV-5`, `INV-6`, `NFR-4` | `F-001`'s differential test and `F-004`'s concurrency test use them. `rederive` matches stored balances on 10 000 random units |
| T-015-08 | The assurance generator: the declaration JSON, the enforced and attested rules, the below-named-layer flag, the floor check, the baseline no-weakening check and verify mode | `REQ-AR-4`, `REQ-AR-5`, `REQ-AR-6`, `REQ-PR-6`, `NFR-14` | The fixture reports in the test plan give the expected pass or fail. `assurance.json` is produced in CI |
| T-015-09 | Gate profiles `m0` and `m1` in `gates.toml`, and CI wiring as required checks on `crates/` | `NFR-14` | The `m0` profile is a required check. With `F-001` done, it is green |
| T-015-10 | §9's V0 column in the catalogue and checked against the floor; `AC-C3.2` required again in `m1`; cases for `ERR-DecisionUnavailable` and `ERR-Internal`; the catalogue at SPEC v0.4.1 (amendments 0006, 0007) | `REQ-AR-4`, `REQ-AR-6`, `NFR-14`, `REQ-ER-1` | The catalogue is current; the floor matches the V0 column; the `m1` profile passes with `AC-C3.2` required |
| T-015-11 | The catalogue at SPEC v0.4.2 (amendment 0008): `AC-M5.1`, the revised `AC-C1.1`, `AC-K5.1` and `ERR-DecisionUnavailable`; the refusal's description to match | `REQ-CS-4`, `REQ-ER-1`, `NFR-14` | The catalogue is current and every refusal matches its published description |
| T-015-12 | The catalogue at SPEC v0.4.3 (amendment 0009): its version, and `AC-A2.1`'s reworded text (the owner's answer to `OQ-64`), whose case also traces `REQ-ID-6`; no invariant or error changed; the `AC-M2.3` case no longer cites the closed `OQ-51` | `NFR-14`, `REQ-ID-6` | `cargo xtask spec-ids --check` passes at v0.4.3 |
