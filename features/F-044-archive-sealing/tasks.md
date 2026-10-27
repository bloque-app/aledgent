<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-044 — tasks

**Ruled 2026-10-27: the owner accepted D1–D9 of `plan.md` as recommended.** `T-044-01` to `T-044-03` are the pull request that
carries this file; the code follows each decision's recommended option. All traces: `NFR-4`, `NFR-10`, `NFR-22`,
`NFR-21` (c), `REQ-AU-8`; `T-005-15`, `T-005-16`.

| ID | Task | Done when |
|---|---|---|
| T-044-01 | **The design.** `plan.md`, `tasks.md` | The decisions are ruled, and the plan says so |
| T-044-02 | **The seal in `core-archive`.** age v1 (`age`), the envelope, the seal file, Transit's public-key wrap, the reader's keys (Vault Transit with a token file, the recovery identity file), the layer under `Archive` with write-once by seal line, keyless heads and the archiver's resume from them, the spec's options, `check_writer(sealed)`, the lock's default to 180 days, `seal::copy` | The tests of `plan.md` §"What is built" pass; every existing test of the crate passes; the core host's, `opsctl`'s and `chainspec.rs`'s calls are unchanged; `agents/Cargo.lock` carries the new dependencies |
| T-044-03 | **Against a real Vault.** `tests/transit.rs` in the `vault transit (F-042)` job | The job is green: Vault's `decrypt` opens the writer's wraps, per version after a rotation, under a decrypt-only policy; a token without it is denied |
| T-044-04 | **Infrastructure** (the infrastructure repository; David applies). The new bucket, the Transit mount and key, the IAM and Vault policies, the copy job's role: as specified in the pull request that carries this file (Vault mount `transit-aledgent-archive` with an `rsa-4096` key that is neither exportable nor deletable and whose old versions always decrypt; a new Object Lock bucket, compliance 180 days, no lifecycle; a decrypt-only restore policy) | Applied in test, then prod; the plan matches the spec |
| T-044-05 | **The recovery key.** One per deployment, generated 2026-10-27; the public halves are in `plan.md` §"The recovery recipients" and go in each seal file. **Left:** the private halves are only in Pablo's Keychain (`kenv` group `archive`); Pablo makes the offline copies | Offline copies exist in two places, and the identity opens a sealed object on an offline machine with stock `age` |
| T-044-06 | **`opsctl archive copy` and the runbooks** (`ops/`): the copy over `seal::copy`; `archive verify` reports sealed and unsealed objects and the Transit versions; `core-archive.md`, `total-loss-recovery.md`, `restore-drill.md` | A copy of test's archive into its new bucket, run twice, writes once; the runbooks name the seal file, the reader's token file and the recovery identity |
| T-044-07 | **Cutover**, test then prod (rides the node change, bloque-ledgent#289): the seal file on the node, `--archive` to the new bucket with `seal=` and `retain-days=180`, the copy, a restore drill from the new bucket with each key alone, then `unsealed=refuse` (D6) | The drill passes from the new bucket with Transit only and with the recovery key only |
| T-044-08 | **The restore's Vault login** (D8 (b)): `opsctl` and `aledgent-core-host restore` log in with `F-042`'s session under an `archive-restore` role | A restore reads a sealed archive with no token file |
| T-044-09 | **The plan's row**: `F-005`'s archive sentence and a row for `F-044` in `PLAN.md`, and the status line | `PLAN.md` says the archive is sealed by aledgent, KMS optional |
| T-044-10 | **The seal mandatory** (D5 (b)), in the release after the cutover | An S3 writer without `seal=` does not start |
