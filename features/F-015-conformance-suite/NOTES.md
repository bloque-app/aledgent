<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-015 — implementation notes

Written while implementing `F-015` (issue #15), 2026-09-23. The plan's proposals were followed as
accepted, except where a proposal could not be built under the workspace's rules (decision 1). Where
the plan was silent, the more conservative behaviour was chosen; each such choice is listed here for
review.

## The layer disagreement: SPEC §9 against `REQ-AR-6` (amendment 0006, item 2)

SPEC §9 names **store + service** for `INV-15`, **store** for `INV-16` and **credential** for
`INV-22`. `REQ-AR-6` asks the first release to declare all three **service-enforced**. `REQ-AR-4`
forbids satisfying an invariant at a weaker layer than §9 names without an amendment. Item 2 of the
proposed amendment 0006 (`amendments/0006-lifecycle-and-assurance-fixes.md`) resolves it by keeping
§9 as the target layer and adding a V0 column that says "service" for these three. **Ruled on
2026-09-29 as recommended** (SPEC v0.4.0). The catalogue now carries both columns (`layers`, the
target, and `v0`), and a test checks `floor.toml`'s `v0_layer` entries against the V0 column: they
are exactly the invariants whose V0 layer differs from the target. `AC-C3.2` no longer waits in the
`m1` profile: its ruling (item 11) landed, and it is required again.

What the suite does, unchanged by the ruling:

- **The declaration follows `REQ-AR-6`.** `spec/floor.toml` lists the three as `v0_layer =
  ["service"]`, with the disagreement in its note. In `assurance.json` each carries
  `declared_layers: ["service"]`, `below_named_layer: true`, `basis: "REQ-AR-6"` and that note.
  Every other invariant is declared at exactly the layers §9 names.
- **The §9 layer stays registered as the target.** The closure check still requires a case at every
  layer §9 names, so `INV-15` (store), `INV-16` (store) and `INV-22` (credential) keep pending cases.
  They are listed in the declaration as `target_layer_cases`, do not back or block the V0 claim, and
  are not required by the `m0` or `m1` profiles. They are required by `full`, so the full run stays
  red on them until the amendment is ruled, and that is deliberate: the gap is visible.
- If the owner rules item 2 as recommended, nothing in the suite changes but the wording of the
  notes. If the owner rules the other way, remove the three `v0_layer` entries from `floor.toml`,
  and the §9-layer cases become required at `m1`.

## Decisions taken during implementation

### Registry and runner

1. **No `linkme`: a hand-kept table per feature.** `linkme`'s `#[distributed_slice]` expands to
   `#[link_section]` statics, which Rust 2024 treats as an unsafe attribute; the workspace's
   `unsafe_code = "forbid"` rejects it (checked on a scratch crate), and the brief forbids `unsafe`.
   `inventory` has the same problem. The suite uses the plan's stated alternative: one `CASES` table
   per owning feature in `crates/conformance/src/cases/fNNN.rs`, joined by `cases::registry()`. A
   feature writes its bodies in its module and turns `Case::pending` into `Case::run`.
2. **Case bodies live in `crates/conformance`, not next to each feature's code.** A case drives the
   harness ports, and `conformance` depends on `ledger-kernel` and `ledger-core`; a feature crate
   cannot register into a binary that depends on it without a dependency cycle. `F-001`'s ten cases
   were moved, unchanged in substance, from `crates/ledger-kernel/tests/conformance_m0.rs` to
   `cases/f001.rs`; the file and its `[[test]]` target are removed. This closes `F-001`'s
   `TODO(F-015)` (its `NOTES.md` still describes the interim state, and is left as `F-001`'s record).
3. **`inv_7_…__store_and_type` is renamed `…__type`** and registered at the type layer: it shows the
   binding and the draft types refusing, not the store refusing a raw write. `INV-7`'s store half is
   a pending `F-003` case (excess precision refused at the registry's precision).
4. **`inv_25_a_second_journal_is_refused__store` is registered at the store layer** under `INV-25`,
   as extra evidence beside the two architecture cases. It is required by `m0`'s layer list and
   counts towards the declaration as a realised store layer.
5. **Known gaps are a case kind.** `inv_4_open_point_kernel_does_not_check_entry_asset_against_
   account__kernel` pins today's kernel behaviour (a mis-tagged entry is accepted). It is registered
   as `Kind::KnownGap`: it runs and must pass, but a passing known gap *denies* a claim at its layer.
   So `INV-4` is not declared kernel-enforced, as `F-001`'s notes require, until the upstream check
   lands and the case is rewritten as a check.
6. **The runner** is `libtest-mimic` (`harness = false`), as proposed. Trials are named
   `<target>::<id>::<case>` with the owner as the test kind, so `cargo test -p conformance --test
   suite -- --list` lists every case by id. A pending case the profile does not require is *ignored*
   (and fails as pending under `--include-ignored`); a required pending case fails. A case with a
   body always runs, and a failure always fails, whoever owns it. A panic in a body is a failure.
   `CONFORMANCE_REPORT_ONLY=1` writes the report and exits 0, for the published full run.
7. **One outcome per case per store.** Each case runs on a fresh harness per target
   (`in-process/mem-store`, `in-process/file-store`). The assurance target `in-process` covers both;
   a case passes there only if it passed on both.

### Harness

8. **Two ports beyond the plan's three.** `Port::Binding` (with `Harness::binding()` and
   `bootstrap_again()`) is `F-001`'s own API — units of posting drafts, the settled net, the delta
   log — which the type and architecture cases use and which the plan's `Kernel | Store | Service`
   did not cover. `Port::Build` is for build-time checks (`INV-17`'s build gate). The layer enum has
   `Type` and `BuildGate` beside the plan's five, because §9's "Enforced by" column names them for
   `INV-7` and `INV-17`.
9. **The store port is a separate, empty store** of the target's kind, not the book's own: the store
   cases write their own bootstrap, as `F-001` wrote them. `reopen_store()` closes and reopens the
   file; in memory it is a no-op.
10. **`ServicePort` is an empty trait** and the in-process harness offers none: `ledger-core` has no
    commands yet. `TODO(F-003)` marks where they go. Every service-layer case is pending until then.
11. **The ports CI check needed no change.** `tools/ci/check-conformance-ports.sh` (from `F-001`)
    already allows exactly `conformance`; it still passes, and its fixture self-test still rejects a
    production crate that enables the ports.

### Catalogue, owners and profiles

12. **Checked-in catalogue, as proposed.** `cargo xtask spec-ids` (an alias for `cargo run -p
    conformance --bin xtask`, in `.cargo/config.toml`) parses `SPEC.md` and writes
    `spec/catalogue.toml`: 25 `INV-*` with their layers, 36 `ERR-*` with their conditions verbatim,
    and the 109 `AC-*` of the stories §15 puts in V0 (124 in §6, less the 15 of `US-C5`, `US-F2`,
    `US-G4`, `US-I2`, `US-I3` and `US-J2`). The suite also compiles `SPEC.md` in and checks the
    catalogue is current as its first trial, so `cargo test` catches drift even without CI.
13. **Owner of each acceptance criterion.** From the feature exit criteria of PLAN §4 and the
    feature plans. Where two features claim one criterion, the more specific claim wins: `AC-A1.3`
    → `F-007` and `AC-A2.2` → `F-008` (PLAN §5 says so); `AC-C3.2`, `AC-C4.*` → `F-007`;
    `AC-E1.1`–`E1.3` → `F-010`, `AC-E1.4`–`E1.5` → `F-016`; `AC-D2.*` → `F-016`; `AC-J3.*` →
    `F-017`; `AC-K3.*` → `F-009`; `AC-K5.*` → `F-014`. Five criteria no feature plan names were
    assigned by the invariant or error they trace: `AC-A6.*` → `F-003` (`ERR-AccountNotEmpty`),
    `AC-I1.2` → `F-011` (`INV-16`), `AC-I4.*` → `F-003` (`INV-17`), `AC-M4.1` → `F-011` (`INV-8`).
14. **Invariants with two owners get two cases.** `INV-16`: store (`F-003`, the §9 target) and
    service (`F-011`). `INV-20`: the causing authority recorded (`F-003`) and every entry visible to
    the account's authority holders (`F-011`). `INV-6`: store (`F-006`), service (`F-011`).
    `INV-3`: `F-001`'s kernel case and a pending `F-004` kernel case with open reservations.
15. **What a profile requires.** A case is required when its owner is a feature of the profile, its
    id is selected (a listed invariant, an error owned by a profile feature, or a criterion of a
    listed epic), its layer is allowed, and it is not a §9-target case (above). `m1` therefore does
    not require cases in its epics that belong to later features — `AC-K3.*` (`F-009`, `M-2`) — nor
    `INV-6` at store (`F-006`) or `INV-17`'s build gate (`F-017`). The gate prints these as deferred.
    Each profile's listed invariants must have at least one required case, so a profile cannot pass
    vacuously. PLAN §5's `M-1` gate does not mention errors; `m1` requires the errors its features
    own, which is the stricter reading.

### Assurance declaration

16. **`realised_layers` is a list**, not the proposal's single `realised_layer`: `INV-2`, `INV-6`,
    `INV-7`, `INV-15` and `INV-17` name two layers, and `INV-25` is also shown at store.
17. **Enforced** means: every case bearing on the id passed as a check on every store of the target,
    and the id is realised at every declared layer. A layer is realised only if every case at it
    passed as a check, so one pending case at a layer (`F-004`'s `INV-3` kernel case) withholds the
    claim at that layer. At `M-0` this gives `INV-1` (kernel), `INV-2` (kernel, store) and `INV-25`
    (architecture, store); `INV-3`, `INV-4` and `INV-7` wait on `F-004`, the upstream currency check
    and `F-003` respectively.
18. **Milestone and release mode.** The plan says generation fails on any id neither enforced nor
    attested (`REQ-AR-5`) and on the floor (`REQ-AR-6`). Applied at `M-0` that makes CI red for
    features that do not exist yet, which the brief rules out. So each gate profile names a mode:
    `m0` and `m1` are **milestone** mode (the declaration is written, unmet invariants and the floor
    shortfall are reported), and `full` is **release** mode (both fail generation). The release
    declaration is the one `REQ-AR-5` and `REQ-AR-6` bind.
19. **No baseline is committed.** The baseline is "the last released declaration"; nothing has been
    released. CI compares against `crates/conformance/assurance/baseline.json` when it exists, and a
    weakening then fails unless the commit message cites `amendments/NNNN-*.md` and that file exists.
    Losing any realised layer counts as a weakening, since SPEC does not rank kernel, store and
    credential against each other.
20. **Attestations** need a justification and a reviewer (`assurance/attestations.toml`); an entry
    without either is refused. Nothing is attested. SPEC does not say what an attestation needs.
21. **Verify mode** regenerates the declaration from the deployment's own report and checks that
    every enforced claim, at every layer it claims, is backed; attested claims need an entry;
    the disclosure posture must equal the floor's (`signed-assertion`, `REQ-PR-6`).

### Refusals

22. **`ledger-core` exists with `lib.rs` and `refusal.rs` only**, `no_std`, as the brief asks; `F-003`
    builds on it. `Refusal` is deliberately not `#[non_exhaustive]`: adding a variant must break
    exhaustive matches and the two-way closure check.
23. **Serialised shape**: `{ "error": "ERR-…", "description": "<§10 condition verbatim>" }`, behind a
    `serde` feature. Deserialising is strict: both fields, known values, naming the same refusal, no
    other field. The description is carried so the shape test compares the runtime text with the
    published one (`REQ-ER-1`). The wire shape of the command surface stays `F-009`'s.

### Shared tools

24. **`F-001`'s differential test does not use the shared tools.** It is a unit test inside
    `ledger-kernel`; `conformance` depends on `ledger-kernel`, so the reverse dependency would link
    two copies of the crate. `F-004`'s concurrency test can use them from `ledger-core`'s integration
    tests, where a dev-dependency on `conformance` resolves to one copy.
25. **The 10 000 random units run as 400 books of 25 units**, in four parallel tests, rather than one
    book of 10 000: re-deriving checks the whole log each time, and small books shrink better on
    failure. `CONFORMANCE_REDERIVE_UNITS` changes the total. The generator issues value two moves in
    five so that most steps apply and overdraws are still refused.
26. **`rederive` refuses a log with pending-transfer statuses** as unsupported: under `DEC-2` there
    are none, and guessing the kernel's layer movements would make the check weaker than it looks.

## Open points for the owner

- **Amendment 0006, item 2** (above): ruled; the declaration follows §9's V0 column.
- **Required checks.** The `conformance` job should be made a required status check on `main`
  (branch protection is a repository setting this PR cannot change). `conformance-full` is a report.
- **`CLAUDE.md` "Commands"** still lists `m0: cargo test -p ledger-kernel --features
  conformance-ports --test conformance_m0`, which no longer exists. The replacement is `cargo test -p
  conformance` (and `cargo xtask spec-ids --check`). Not edited here.

## Deferred

- `TODO(F-003)`: the `ServicePort` surface and `ledger-core` in the in-process harness; the rest of
  `ledger-core` on top of `refusal`.
- `F-005`: the `ThroughAgent` harness. `F-009`: the `Deployment` harness (service port only) and
  the wire half of the refusal-shape test.
- `F-011`, `F-017`: serving `assurance()` and carrying the level on disclosures.
- Every feature: its case bodies, in `crates/conformance/src/cases/fNNN.rs`.
