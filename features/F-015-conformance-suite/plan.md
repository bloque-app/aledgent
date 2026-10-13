<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-015 — Conformance suite

| | |
|---|---|
| Milestone | M-0 — Foundations |
| Track | A |
| Home | `crates/conformance` |
| Depends on | nothing to start. `F-001` for the kernel and store cases to pass |
| Unblocks | every Track A feature (it is their CI gate), `F-005` (the suite through the Agent), `F-009` (the suite against a deployment), `F-011` and `F-017` (they serve the assurance declaration) |
| Realises | `DEC-1` (testable on a laptop), the mitigation of `RISK-6` |

## Goal
One suite names a test for every invariant in SPEC §9, every error in §10 and every acceptance
criterion of the V0 stories. Each test runs at the layer §9 names. The suite writes the
machine-readable assurance declaration from its own results, so no claim can exceed what passed. It
runs in process, through the Agent and against a deployment, and it is the CI gate for the library.

## Spec traces
- `NFR-14`: exercise every `INV-*` and `ERR-*`, and re-run against each deployment's declared level.
- `REQ-AR-4`: test each invariant at its named layer. A weaker layer is a failure, not a pass.
- `REQ-AR-5`: the declaration, in machine-readable form.
- `REQ-AR-6`: the floor the first release must declare.
- `REQ-ER-1`: the error list is closed, and each error's runtime shape is tested against its published description.
- `REQ-CS-4`: the library-level half of the refusal-shape test. The wire half is `F-009`'s.
- `REQ-PR-6`: disclosures declared as signed assertions until `DEF-3`.

## Scope
**In:**
- the spec catalogue and its generator;
- the case registry and the runner;
- the harness trait and the in-process harness;
- closure checks;
- one registered case per `INV-*` and `ERR-*`, and one per V0 `AC-*` (owned by the feature that makes it pass);
- shared generators and a plaintext shadow book;
- the assurance generator, with its floor and no-weakening checks;
- gate profiles and CI wiring.

**Out:**
- The bodies of feature cases. Each feature writes its own, and this suite registers them.
- The harness through the Agent (`F-005`) and against a deployment (`F-009`).
- Serving `assurance()` (`F-011`, `F-017`).
- The continuous checks in production (`F-018`).

## Design

**1. Spec catalogue.** `cargo xtask spec-ids` reads SPEC §9, §10 and the `AC-*` lines of the V0
stories (§15). It writes `crates/conformance/spec/catalogue.toml`: every `INV-*` with its named
layers (`INV-2` → kernel and store), every `ERR-*` with its condition, and every V0 `AC-*`. A
hand-written `spec/floor.toml` holds the three lines of `REQ-AR-6`. CI regenerates the catalogue and
fails on any diff, so a spec change cannot land without a matching suite change. **Proposal:** a
checked-in catalogue. *Alternative:* parse `SPEC.md` at build time, tying every build to it.

**2. Cases.**

```rust
pub struct Case {
    id: SpecId,                 // "INV-3", "ERR-ZeroAmount", "AC-B2.1" — must exist in the catalogue
    name: &'static str,         // "inv_03_customer_never_negative__kernel"
    layer: Layer,               // Kernel | Store | Service | Credential | Architecture
    owner: Feature,             // F-004
    traces: &'static [&'static str],
    needs: Port,                // Kernel | Store | Service
    run: fn(&mut dyn Harness) -> Outcome,   // Pass | Fail(String) | Pending(Feature) | NotRunnableHere
}
```

**Proposals:** registration by a distributed slice (`linkme`), so each feature registers cases next
to its code (*alternative:* `inventory`, or one hand-kept table); a custom runner (`harness = false`,
`libtest-mimic`) that lists cases by id and writes `target/conformance/report.json` (*alternative:*
plain `#[test]` with parsed JUnit output). A case whose feature is not built returns
`Pending(owner)`, which fails the full run. So at `M-0` the suite exists and fails (PLAN §5).

**3. Harness and ports.** The same cases run on three targets:

```rust
pub trait Harness {
    fn kernel(&mut self)  -> Option<&mut dyn KernelPort>;   // raw transfers; ledger-core bypassed
    fn store(&mut self)   -> Option<&mut dyn StorePort>;    // raw writes to the leaf store
    fn service(&mut self) -> Option<&mut dyn ServicePort>;  // ledger-core commands and queries as a principal
    fn clock(&mut self)   -> &mut dyn ClockPort;            // time as input (REQ-RT-7)
}
```

The targets are `InProcess`, over `F-001` and `ledger-core`, parameterised by `MemStore` and
`FileStore`; `ThroughAgent` (`F-005`); and `Deployment` (`F-009`, service port only). A case whose
port is `None` returns `NotRunnableHere`. It then cannot back an "enforced" claim for that target.
The kernel and store ports exist only under `ledger-kernel`'s `conformance-ports` feature, which
production crates must not enable (`F-001` §11, `REQ-CS-5`).

**4. What a case at each layer must show** (`REQ-AR-4`):

| Layer | The case must show |
|---|---|
| Kernel | A validly signed violation submitted to `KernelPort` is refused by CipherClerk, with `ledger-core` bypassed |
| Store | A forbidden write submitted to `StorePort` is refused, and state after a reopen is unchanged |
| Service | The violation, attempted through `ServicePort` only, is refused or cannot be formed. Each case pairs a negative test with a positive one |
| Credential | A widened or forged credential fails cryptographic verification without consulting service state |
| Architecture | A structural check: for `INV-25`, a second journal cannot be bootstrapped and every asset shares one book |

**5. Closure checks**, run first: every catalogue id has a case; each `INV-*` has a case at each
named layer; no case cites an id missing from the catalogue; and the codes of `ledger-core`'s closed
`Refusal` enum equal the catalogue's 36 `ERR-*` ids, both ways (`REQ-ER-1`).

**6. Invariant map.** Owners follow PLAN §4. "M-1" marks the invariants in the `M-1` gate.

| ID | Named layer | Owner | The case, in one line | M-1 |
|---|---|---|---|---|
| `INV-1` | Kernel | `F-001` | unbalanced, single-entry and cross-asset raw transfers refused | ● |
| `INV-2` | Kernel + store | `F-001` | a void is a new transfer. Overwriting a transfer, external-id or voided leaf is refused | ● |
| `INV-3` | Kernel | `F-004` | raw overdraw of a Customer account refused while reservations are open | ● |
| `INV-4` | Kernel | `F-001` | per-asset zero-sum. An entry in the wrong asset is refused (see Open points) | ● |
| `INV-5` | Store | `F-011` | the trial balance is exactly zero after every random unit | ● |
| `INV-6` | Store + service | `F-006`, `F-011` | balances rebuilt from the delta log equal stored balances | ● |
| `INV-7` | Store + type | `F-001`, `F-003` | the float gate, `Minor` types, and excess precision refused | ● |
| `INV-8` | Service | `F-011` | reads come only from the book. No writable copy is exported | ● |
| `INV-9` | Service | `F-003` | a new account is credited in an asset it has never held | ● |
| `INV-10`–`INV-13` | Service, Store | `F-010` | against the simulated settlement layer | |
| `INV-14` | Service | `F-004` | capture and release are keyed by operation. A supplied amount never sets the release | ● |
| `INV-15` | Store + service | `F-007` | no non-terminal record without a deadline | ● |
| `INV-16` | Store | `F-003`, `F-011` | an unauthorised read gives `ERR-UnknownAccount` | ● |
| `INV-17` | Store + build gate | `F-003`, `F-017` | the identifier grammar, and a build gate on anchored values | ● |
| `INV-18`, `INV-21` | Store | `F-007` | repeated transitions are no-ops. Terminal states absorb | ● |
| `INV-19` | Service | `F-017` | an anchor commits to a stated sequence | |
| `INV-20` | Service | `F-003`, `F-011` | every entry is visible with the authority that caused it | ● |
| `INV-22` | Credential | `F-003` | no API widens a grant. Effective scope is the meet of the chain | ● |
| `INV-23` | Service | `F-010` | a kernel refusal never ends an externally observable leg | ● |
| `INV-24` | Store | `F-003` | a second registry or a precision outside it cannot be formed | ● |
| `INV-25` | Architecture | `F-001` | one journal | ● |

**7. Error map.** Each `ERR-*` case triggers the condition, asserts `Err(Refusal)` (commands return
`Result<Ack, Refusal>`, so a success body cannot carry an error), and checks the code and the
serialised shape against the catalogue entry.

| Owner | Errors |
|---|---|
| `F-003` | `ERR-NotAuthorised`, `ERR-AuthorityRevoked`, `ERR-CredentialInvalid`, `ERR-UnboundedGrant`, `ERR-BoundExceeded`, `ERR-UnknownAccount`, `ERR-AccountClosed`, `ERR-AccountNotEmpty`, `ERR-IdentityRestricted`, `ERR-UnknownAsset`, `ERR-AssetDeprecated`, `ERR-PrecisionExceeded`, `ERR-ZeroAmount` |
| `F-004` | `ERR-InsufficientAvailable`, `ERR-AmountMismatch` |
| `F-007` | `ERR-ReleaseUnsafe`, `ERR-OperationTerminal`, `ERR-InvalidTransition`, `ERR-UnknownOperation`, `ERR-DeadlineRequired`, `ERR-ClientKeyReused` |
| `F-008` | `ERR-BatchTooLarge` |
| `F-010` | `ERR-ObservationDuplicate`, `ERR-ObservationUnassociated`, `ERR-EvidenceInsufficient` |
| `F-011` | `ERR-QueryUnbounded` |
| `F-012` | `ERR-RecoveryNotElapsed` |
| `F-013` | `ERR-AccountFrozen`, `ERR-JustificationRequired`, `ERR-ApprovalRequired` |
| `F-016` | `ERR-UnsupportedLeg`, `ERR-SettlementLayerUnavailable` |
| `F-017` | `ERR-DisclosureUnsupported`, `ERR-DisclosureFalse`, `ERR-NotDisclosable`, `ERR-AnchorNotFinal` |

**8. Shared tools.** `proptest` strategies for books and command schedules; `ShadowBook`, a plaintext
model of every entry per (account, asset), for `INV-5` and `INV-6`; and `rederive(delta_log)`, which
rebuilds balances from committed deltas alone (`NFR-4`).

**9. Assurance declaration.** `cargo run -p conformance --bin assurance -- --report <r> --target <t>`
writes `assurance.json`. **Proposal** for its shape:

```json
{ "schema": "aledgent/assurance/v1", "spec": "0.3.3", "target": "in-process",
  "kernel": { "crate": "cipher-clerk", "rev": "e9a7b08" }, "suite_rev": "<git sha>",
  "invariants": [ { "id": "INV-3", "named_layers": ["kernel"], "realised_layer": "kernel",
                    "status": "enforced", "cases": ["inv_03_customer_never_negative__kernel"] } ],
  "disclosures": { "mode": "signed-assertion", "basis": "REQ-PR-6" } }
```

The generator's rules:

- **Enforced** at layer L: every case for the id passed on this target, and one of them is at L.
- **Attested:** only for a reviewed entry with a justification in `attestations.toml`. Any id that
  is neither makes generation fail (`REQ-AR-5` needs every invariant declared).
- **Below the named layer:** `service` for an id §9 names store, kernel or credential is flagged, and
  fails unless `floor.toml` lists it (see Open points).
- **Floor** (`REQ-AR-6`): `INV-1`–`INV-4`, `INV-7` and `INV-12` at kernel or store; `INV-15`,
  `INV-16`, `INV-20` and `INV-22` enforced at service or stronger; disclosures as signed assertions.
- **No weakening:** compared with `assurance/baseline.json`, the last released declaration. A move to
  attested or to a weaker layer fails CI unless the commit cites a file in `amendments/`
  (`REQ-AR-4`, `REQ-AR-6`).
- **Verify mode** (`NFR-14`): `assurance verify --declaration <d> --report <r>` checks each enforced
  claim of a deployment against a passing case in that deployment's own run.

**10. Gates.** Profiles live in `gates.toml`:

- **`m0`:** closure checks pass, the catalogue is current, and the kernel and store cases of
  `INV-1`, `INV-2`, `INV-3`, `INV-4`, `INV-7` and `INV-25` pass. Other cases may be `Pending`.
- **`m1`:** the `M-1` gate of PLAN §5. That is Epics A (except A8), B, C, F, G, H and K, plus
  `INV-1`–`INV-9`, `INV-14`–`INV-18` and `INV-20`–`INV-25`.

CI runs the current profile on every change to `crates/`. The full run, which is red until `M-2`, is
published as a report.

## Test plan
- Catalogue: the generator run on a copy of SPEC with one `INV-*` row removed gives a diff, and CI
  fails.
- Closure: a fixture crate with a case citing an id that is not in the catalogue, and another with no case for one `ERR-*`,
  each fail the closure check.
- `Refusal` ↔ catalogue: a variant added without a catalogue entry fails, and so does the reverse
  (`REQ-ER-1`).
- Assurance generator, on fixture reports: an `INV-3` kernel case failing gives no kernel claim, and
  the floor check fails (`REQ-AR-6`). A baseline weakening fails without an amendment reference and
  passes with one (`REQ-AR-4`). Verify mode rejects a declaration that claims more than the report.
- Ports: CI shows that no production crate enables `conformance-ports` (`REQ-CS-5`).
- End to end at `M-0`: `InProcess` over both `F-001` stores meets the `m0` profile.

## Exit criterion
From PLAN §4: *the suite is the CI gate for the library.* Made concrete:

- all 25 `INV-*`, all 36 `ERR-*` and every V0 `AC-*` have a registered case, passing or `Pending`
  with an owner;
- the closure checks and the catalogue check are required CI checks;
- the `m0` profile is a required check on `crates/`;
- `assurance.json` is generated in CI and checked against the floor and the baseline.

## Open points
- **Layers in §9 and in `REQ-AR-6` differ.** §9 names Store for `INV-16`, Credential for `INV-22`
  and Store + service for `INV-15`. `REQ-AR-6` asks V0 to declare them service-enforced. `REQ-AR-4`
  forbids a weaker layer without an amendment. The generator records both layers and allows the gap
  only for ids listed in `floor.toml`. The owner should confirm that `REQ-AR-6` governs for V0, or
  write an amendment. Not decided here.
- **Order of layers.** SPEC does not rank the layers, beyond saying store must not become an
  application check. The generator treats service as weaker than the other three and does not rank
  kernel, store and credential against each other.
- **"Attested".** SPEC does not say what evidence an attestation needs. The proposal is a reviewed
  entry with a justification.
- **`INV-4`.** It cannot be claimed at kernel strength until the currency check is upstream or in a
  pinned fork (`F-001` Open points).
- **Layout.** The closed `Refusal` enum must live in `ledger-core`, because every feature returns it.
  `DEC-15` lists `F-015` only under `crates/conformance`. The proposal is that `F-015` owns the
  `refusal` module in `ledger-core` and checks it against the catalogue.
- **Proposals to review:** a checked-in catalogue, `linkme`, the custom runner, and the JSON shape.
- Exposure: the `Deployment` harness waits for `F-009`. `NFR-14` binds before production use.
  `RISK-6` is this feature's reason to exist.
