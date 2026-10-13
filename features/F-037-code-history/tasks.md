<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->
# F-037 — tasks

Amendment 0023 (code history and upgrades), ruled by the owner on 2026-10-08 (`a0023-q1` to `a0023-q9`);
`PLAN.md` `DEC-26`. The cases `AC-K7.1` to `AC-K7.8` are registered as pending
(`crates/conformance/src/cases/f037.rs`). After `F-036`, before `F-021`.

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-037-01 | The code manifest (`aledgent/code/v1`): artefacts with role, `blake2b-256`, length, `ProgramId` and build profile; the kernel's identity; constructions read and message formats; *H* as the manifest's digest. The chainspec's header names *H₀*, the construction and the initial mode (with `F-036`) | `REQ-RT-15`, `REQ-RT-16`, `AC-K7.6` | Two builds of one source at different `opt-level`s give two code versions; the chainspec's *H₀* is checked at founding |
| T-037-02 | The hot row's `code`, `construction`, `upgrade`, `authority`, `authority_next`; each ring entry's `code`, covered by the log hash | `REQ-RT-15`, `DEC-4` | Every archived entry names its code; the hot row stays bounded |
| T-037-03 | Checkpoint `v3` (with amendment 0022's genesis digest): `construction`, `code` and `authority` in the checkpoint, `consistent` on the checkpoint's own construction; the shard leaf `aledgent/anchor/shard/v3` | `REQ-RT-15`, `REQ-PR-10`, `a0023-q3`, `a0023-q4` | A checkpoint and its anchor bind code, construction and mode; earlier `v2` anchors still check |
| T-037-04 | `Upgrade { code, from }` through `CTR-OPS`: fetched and checked before it is logged; scheduled in the writer; the host switches the Agent's program (VOS registry upgrade) before input `n`; input `n` moves `upgrade` into `code` | `REQ-RT-16`, `REQ-OS-2`, `REQ-AT-3` | `AC-K7.1` and `AC-K7.2` pass |
| T-037-05 | The upgrade-authority modes (`Single`, `ProposeApprove`, `Threshold`, optional delay) and `SetAuthority`, `ProposeUpgrade`, `ApproveUpgrade`, `ProposeMigration`: deterministic validation, cancellation of pending proposals | `REQ-RT-18`, `a0023-q2` | `AC-K7.7` and `AC-K7.8` pass |
| T-037-06 | `CodeSource`: the fetcher's mandatory length and digest check, the content-addressed cache, `fs`, `s3`, `https`, `exec`; `aledgent_code_source_mismatch_total` | `REQ-RT-17`, `REQ-AU-8` | Bytes that do not match are never used, whatever the source |
| T-037-07 | C-ABI plugins (ABI version 1, `aledgent_code_source.h`, the `aledgent-code-source-plugin` crate), loaded by digest, isolated in `aledgent-code-fetch` by default | `REQ-RT-17`, `a0023-q7` | A plugin with the wrong digest or ABI is refused at start; a crashed plugin is restarted and cannot make a node run wrong code |
| T-037-08 | `kreivo-preimage` in the open-source distribution, and the release pipeline noting each published code version as preimages | `REQ-RT-17`, `a0023-q8` | A code version is fetched from Kreivo through a light client and checked |
| T-037-09 | Per-range restore and replay: `roll_forward_agent` split at the code map's boundaries; the restore plan records the map; versioned call formats under the frozen host spec | `REQ-RT-3`, `REQ-RT-17`, `a0023-q6` | `AC-K7.3` passes |
| T-037-10 | `MigrateConstruction { to }` and the bridge anchor | `REQ-PR-11`, `DEC-25` | `AC-K7.4` passes |
| T-037-11 | Snapshots with leaf chunks; fast and full resync; the per-shard archive and per-shard warp sync | `NFR-21`, `REQ-RT-17` | `AC-K7.5` passes |
| T-037-12 | The `code-history` CI gate: a fixture archive extended by every release, replayed from genesis under its code map; a deployment's archive replayed read-only on each release (CI and bench runners only) | `REQ-RT-3`, `REQ-RT-17` | The gate runs on every change to a guest, the runtime, the kernel or the call format |
| T-037-13 | The S3 legal hold on code, input segments, anchored checkpoints and the chainspec; the break-glass role and its alert | `NFR-22`, `a0023-q5` | No workload role holds `s3:PutObjectLegalHold`; every object `NFR-22` lists carries a hold |
