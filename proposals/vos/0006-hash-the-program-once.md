<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0006 — Hash the actor program once per invocation

**Draft pull request description, 2026-10-03.** Not yet opened upstream.

> **JAM status (2026-10-04): JAM-compatible, in the default.** All three parts are guest logic in
> the standard runtime: no host call is added, and the runtime still issues only the Gray Paper's
> Refine calls 9–14 (GP 0.8.0 eq. B.6, p. 55).

| | |
|---|---|
| Branch | `perf/agent-stack` on aledgent's VOS fork, commit `a75b1316` (`exp(agent): hash the actor program once per invocation`), on `saga/agents` `e6f2bb45` with 0001–0005 merged |
| Size | 5 files: `services/agent-runtime/src/lib.rs`, `vos-agent-sdk/src/{runtime,wire}.rs`, `vos/src/agent/{standard,wire}.rs` |
| Depends on | 0004 (programs loaded in place); merges with 0001–0003 and 0005 |
| aledgent task | `F-025`, `T-025-03` follow-up (issue #66) |

**Read this first.** 0004 found three BLAKE2b passes over the actor program per Invoke and left them
for the maintainer. On today's `saga/agents` there are **six**: the decoded work is validated three
times, and each validation hashes every availability blob against its `BlobRef`; the program's
ProgramId is computed twice; and the public I/O hashes the whole work once more. Each pass over
aledgent's 2.36 MB core costs about 175 M gas (about 74 gas a byte). This change keeps every check in
the metered guest and removes five of the six passes. The runtime's own gas per Invoke falls from
1 142 M to 269 M, the same as a measurement-only stub that skips the checks outright (270 M). Three
parts, each measured on its own:

| Part | Definition change | Runtime gas per Invoke (core, 2.36 MB) | Saved per program byte |
|---|---|---:|---:|
| Before | | 1 142 M | |
| 1. Validate the decoded work once | none; transitions byte-identical | 793 M | 148 |
| 2. Public I/O by reference | the public I/O's work leaf | 618 M | 74 |
| 3. The program identified by its verified `BlobRef` | the control image gains an optional section | 269 M | 148 |

Everything below the line is the pull request text.

---

## Problem

The standard runtime receives each actor program in its work (`InvocationWork::availability`) and
authenticates it before running it. That is the right design: the runtime is the metered,
provable boundary, and the host is not trusted with the program's identity. But it authenticates the
same bytes six times:

1. The work decoder validates the Invoke (`InvocationWork::validate` → `availability_valid` →
   `RuntimeBlob::validate`), hashing every blob against its `BlobRef`.
2. `recover_clean_execution` calls `verify_clean_invocation_authorization`, which calls
   `InvocationWork::validate` again: the same hashes.
3. The commit (`commit_clean_execution_inner`) verifies authorization the same way: a third time.
4. `admit_clean_public_preflight` resolves the invocation (`resolve_clean_invocation`), computing
   `ProgramId::of_pvm` of every blob that could be the program.
5. The execution resolves it again (`resolve_clean_invocation_for_execution`): the same ProgramId.
6. After the guest returns, `runtime_transition_public_io` hashes the whole canonical work.

Each pass is BLAKE2b over the program under a different domain, about 74 gas a byte on the
interpreter's gas model. For a 2.36 MB actor that is 1.05 G of the 1.14 G the runtime spends per
Invoke.

## Design

**1. Validate once.** `ValidatedInvocationWork`, the borrowed capability the work decoder already
mints for `recover_validated_clean_invocation_error`, now also reaches recovery
(`recover_validated_clean_execution`), resolution (`resolve_validated_clean_invocation_for_execution`)
and the commit (`ResolvedCleanInvocation::work_validated`). Work from the decoder skips the
repeated `InvocationWork::validate`; constructed values still validate fully. Nothing else changes:
the same checks run on the same immutable value, once.

**2. The public I/O by reference.** `runtime_transition_public_io_by_reference` (domain
`vos/agent/runtime-public-io/v2`) commits to `runtime_work_reference_commitment(work)`: the canonical
work's bytes with each Invoke and Resume availability blob encoded by its `BlobRef` instead of its
preimage (`encode_runtime_work_body(…, include_preimages = false)`, the encoding
`invocation_work_commitment` already uses for authority receipts). The guest computes it from the
decoded work before execution consumes it (`apply_standard_runtime_input_committed`). The byte-form
`runtime_transition_public_io` stays for custom runtimes and the experimental state-block build.

**3. The program by its verified reference.** The runtime keeps `StandardProgramBinding { actor,
program: ProgramId, reference: BlobRef }` per installed actor, in an optional trailing section of the
control component (`SPB1`), absent while empty. On an actor's first invocation the guest finds the
program as today, by computing the ProgramId of each candidate blob, and records the bound
reference. Later invocations find it by `blob.reference == binding.reference`. A binding names one
ProgramId: it is dropped on upgrade to another program and on removal, and a restored image is
refused if a binding names an actor that is absent or that runs another program.

## Guarantees

- **The guest still verifies the program in every invocation.** Every availability blob is checked
  against its `BlobRef` by the work decoder (part 1 keeps one such pass). Part 3 relies on that check:
  the reference is matched, the bytes behind it were hashed in this execution.
- **Part 2 binds the same bytes.** Two decodable works with the same reference commitment differ in no
  byte unless BLAKE2b collides. Every preimage left out of the commitment was authenticated against the
  `BlobRef` that stands in for it. The reference form keeps the frame's magic and ABI identity, every
  other field and every length, so it is an unambiguous encoding. A work with a tampered blob is
  refused by the decoder, so no public I/O is produced for it: the guest traps exactly as before
  (`runtime_transition_public_io_of_work` gives verifiers the native counterpart, which also refuses
  it). The domain is new, so the two definitions cannot be confused.
- **Part 3 binds the program's identity in the guest.** The binding is written only after this guest
  computed the ProgramId of exactly the bytes that the decoder had just authenticated against the
  reference. Nothing from the host enters the binding unverified. A collision between `vos/blob` and
  `vos/program/standard-pvm` digests would be needed to run other bytes under the bound ProgramId.
- **Transitions.** Part 1 changes no byte of any transition. Part 2 changes no transition, only the
  public I/O, under a new domain. Part 3 adds the `SPB1` section to the control image after an actor's
  first invocation; outcomes, actor lanes and results are unchanged. An image without bindings encodes
  as before.
- **Proofs.** The proven statement still covers the program's authentication, because the
  `BlobRef` check runs in the guest. Part 2 changes which hash the final registers carry; a verifier
  recomputes it from the work with the decoder (`runtime_transition_public_io_of_work`).

## Tests

- VOS's own suites on x86-64 Linux, both engines (aledgent's `x86-bench.yml`, mode `vos-suites`):
  results in aledgent's `notes/MEASUREMENTS-F031.md` §8.
- aledgent's harness: every Invoke of the core through the changed runtime, at batches of 1 to 128
  (results below); on each, the runtime's transition and the core's replica agree.

## Before and after

aledgent's core (2 355 938 B program), one reservation per Invoke; gas is exact and identical on both
engines. Time is on GitHub-hosted `ubuntu-latest` (AMD EPYC 7763, 2 vCPU), all variants in one job
(run 37145564561), recompiler, batch 32 at the standard 1 G and with 8 G of actor gas:

| Runtime | Runtime gas per Invoke | Inputs/s, batch 1 | batch 32, 1 G | batch 32, 8 G |
|---|---:|---:|---:|---:|
| `perf/agent-stack` before this change | 1 142 M | 1.68 | 3.86 | 10.15 |
| Measurement-only stub: passes skipped, unsafe | 276 M | 5.02 | 9.25 | 13.72 |
| This change | 274 M | 5.22 | 9.40 | 13.96 |

## For the maintainer to decide

1. **Whether the public I/O may commit by reference** (part 2). It is the one definition change a
   proof verifier sees.
2. **Where the binding lives** (part 3). An optional control section keeps old images byte-identical.
   A field of the actor record would be cleaner but changes the state wire for everyone.
