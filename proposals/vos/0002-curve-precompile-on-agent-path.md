<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0002 — Serve the curve precompile ECALLs 110–114 on the Agent path

**Draft pull request description, 2026-09-23.** Not yet opened upstream.

| | |
|---|---|
| Branch | `agent/curve-precompile-dispatch` on aledgent's VOS fork, one commit on `saga/agents` at `2bb7876` |
| Commit | `5f5929dc` `feat(agent): serve curve precompile ECALLs to Agent actors` |
| Size | 4 files (`vos/src/agent/execution.rs`, `vos/Cargo.toml`, `Cargo.lock`, `services/agent-runtime/Cargo.lock`), +269 −1; about 130 lines are a test |
| Independent of | 0001: no file in common. Each applies alone to `saga/agents`; together they merge cleanly |
| aledgent task | `T-025-02` (issue #66), `DEC-16` |

**Read this first.** The patch does what the task asked: an actor built with the curve precompile now
runs on the Agent path, deterministically and gas-metered, with the same results as the software path.
It does **not** make the curve arithmetic native, and it cannot without a design decision that belongs
to @olanod (VOS's maintainer). The standard AgentRuntime has, by design, no private outer-host ABI (`vos/src/crypto/blake2b.rs:44-46`),
so the runtime computes each operation as its own interpreted guest code, exactly as it already does
for BLAKE2b. On the interpreter this is *slower* than CipherClerk's software arithmetic (table
below), so it buys correctness and parity, not speed. The native design is written up at the end as
a question, not a patch.

Everything below the line is the pull request text.

---

## Problem

`cipher-clerk` with `pvm-precompile` replaces its Ristretto scalar multiplications, point additions and
scalar reductions with the constrained precompile ECALLs 110–114 (`vos_pvm_precompiles::ecalls`). The
standard AgentRuntime's actor host-call dispatch (`vos/src/agent/execution.rs:951-1237` at `2bb7876`)
serves `GAS`, `ACTOR_EFFECT_EXPORT`, `STORAGE_R`, `PREIMAGE_LOOKUP`, `FETCH`, `GROW_HEAP`, BLAKE2b (100)
and `DEBUG_WRITE`, and nothing in 110–114: an actor that issues one ends in `UnsupportedHostCall`. The
only handler for them is the older service runtime's, host-native (`vos/src/runtime.rs:286-336`), which
Agent actors never reach.

A second, separate reason the precompile build fails today: `cipher-clerk` `e9a7b08` pins its shim to
`zkpvm-precompiles` at VOS `9f5337f`, which emits a bare `ecall`. Today's PVM compiler translates an
unmarked `ecall` into a trap (`pvm/compiler/src/riscv.rs:316-328`); only `csrw 0x801` before it makes an
`ecalli`. So the pinned build panics at its first curve operation *before* it reaches any dispatch. The
current shim in this repository (`pvm/proof/precompiles`, `vos-pvm-precompiles`) emits the marker.
Moving CipherClerk to it is a CipherClerk change; the measurements below build the probe with that
one dependency line changed.

## Design

Serve 110–114 in the runtime's actor host-call loop the way BLAKE2b is served there:

- **Computed as runtime guest code.** `curve_precompile` (`execution.rs`) reads the operands from the
  actor's memory, computes with `curve25519-dalek` 4.1 (already in the runtime's dependency graph
  through `ed25519-dalek`; now a direct optional dependency of `vos` under `agent-runtime`), and
  writes the result through the output pointer. Semantics are byte-for-byte those of the shims' host
  fallback and the prover's tracer: a non-canonical scalar or an invalid point encoding gives the
  all-zero sentinel. The argument registers are preserved, as for BLAKE2b.
- **Metered.** Each operation is charged to the **actor's own gas** before it runs, at a fixed price:
  scalar mult 4.5 M, point add 520 k, wide reduction 14 k, scalar mul 30 k, scalar add 26 k. The
  prices are the operations' interpreted cost, measured on this crate's `curve25519-dalek` release
  built with the runtime's toolchain, target and release profile (3.92 M, 449 k, 12.1 k, 26.3 k,
  22.8 k gas), plus about 15 %. An actor without enough gas for the next operation ends `OutOfGas`
  and nothing is computed. Each call also counts against the existing per-invocation host-call budget
  (`MAX_EXECUTION_HOST_CALLS`).
- **Why a price and not only a count cap, as BLAKE2b has.** A BLAKE2b compression costs thousands of
  gas, so its count cap bounds the runtime's work well inside the management budget. A scalar
  multiplication costs about 4 M; 4 096 of them would exceed the 5 G management budget and fail the
  whole runtime execution instead of the actor. Charging the actor at least the interpreted cost keeps
  the runtime's curve work inside `invocation.gas`, which the outer budget already includes.
- **Invalid pointers** end the invocation as `InvalidInput`, as BLAKE2b does on the Agent path (the
  tracer skips the write instead).

No change to the ABI, the SDK, the continuation encoding (no new budget field) or the bundled
artifact. The runtime program identity changes when rebuilt from this source.

## Guarantees preserved

- **Determinism.** The operation is ordinary guest code in the interpreted runtime: same inputs, same
  bytes, same gas, on every host. No outer host call is added; the runtime's outer surface is still
  the standard machine calls 9–14 (checked by `validate_refine_host_calls` when linking).
- **Metering.** Every operation is paid for twice over: the actor's gas is charged the price, and the
  runtime's own interpreted execution is metered by the outer budget.
- **Proof soundness.** Because the arithmetic runs inside the runtime guest, a transition proof of the
  runtime constrains it as ordinary RISC-V execution. This is stronger than the service runtime's
  host-native handler, whose ECALL AIR records inputs and outputs without constraining the curve
  relation (`vos/src/runtime.rs:268-272`).
- **Parity with the software path.** The probe's reply checks the kernel's resulting root against the
  expected one; it matches with the precompile build (below).

## Tests

- `inner_curve_precompiles_match_host_fallback_and_charge_actor_gas` (`vos/src/agent/execution.rs`): an
  assembled actor issues all five ECALLs; each output equals `vos_pvm_precompiles`' host fallback, the
  scalar multiplication equals `15·B` computed independently, invalid inputs give the sentinel, every
  argument register is preserved, the five prices are charged, and an actor one gas short of the first
  price ends `OutOfGas`.

Suites run (Linux `aarch64` container, `nightly-2025-05-09`):

| Command | Result |
|---|---|
| `cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'` | 1 779 passed, 17 ignored, **7 failed — the same 7 fail on unmodified `2bb7876`** (two need the `x86_64` recompiler, five need prebuilt artifacts) |
| the same, `--ignored compiled_runtime compiled_shared_genesis`, with `AGENT_RUNTIME_CANDIDATE_ELF` built from this branch | 4 passed |
| `cargo test -p vos --lib --features '… experimental-state-blocks'` on `agent::wire`, `agent::replay`, `agent::state_block*`, `agent::execution`, `agent::actor_storage` | 220 passed, 26 ignored |

## Before and after

aledgent's harness (`spikes/vos-host`), the kernel probe inside a Local Agent, through the runtime ABI.
"Before" is the runtime built from `2bb7876`, "after" from this branch, by the same pipeline. "Plain" is
the probe with software curve arithmetic; "precompile" is the probe with `cipher-clerk/pvm-precompile`
on the current shim.

**Does it run?** (the kernel's resulting root is checked against the expected one inside the probe)

| Probe | Before | After |
|---|---|---|
| plain | runs | runs; same gas to the unit on every shape |
| precompile, S2 (transfer, reserves) | `UnsupportedHostCall(112)` at the first curve operation | **runs, root correct** |
| precompile, S3 (witness verification, 1 000 000 accounts) | same refusal | ends `OutOfGas` at the stock per-invocation budgets; with the host-call and BLAKE2b budgets raised 16× in a throwaway build, runs, root correct |

S3 in precompile mode also routes every BLAKE2b hash of the witness through ECALL 100
(`zkpvm_precompiles::blake2b_hash`), one host call per 128-byte block, and a verification at
1 000 000 accounts needs more of them than the Agent path's per-invocation budgets allow
(`MAX_EXECUTION_HOST_CALLS` 4 096, `MAX_EXECUTION_BLAKE2B_COMPRESS_CALLS` about 2 570). That is an
existing limit of the BLAKE2b path, not of this change.

**Cost** (200 kernel and 50 no-op invocations per shape; S3 diagnostic 20). "Kernel runtime gas" is
the Invoke's runtime gas minus the same actor's no-op Invoke: the work the kernel adds, whoever does
it. The precompile actor is 17.8 KB smaller (272 826 B against 290 665 B), so its no-op Invoke is
11.8 M gas cheaper.

| Shape | Plain, before: Invoke gas / kernel runtime gas / caller ms p50, p99 | Precompile, after: Invoke gas / kernel runtime gas / caller ms p50, p99 |
|---|---:|---:|
| S2 transfer | 262.8 M / 40.6 M / 440.1, 489.6 | 269.9 M / 59.5 M / 453.1, 515.7 |
| S2 reserve, new holding position | 268.2 M / 45.9 M / 448.1, 485.9 | 278.9 M / 68.4 M / 464.4, 504.2 |
| S2 reserve, pooled | 262.7 M / 40.6 M / 442.6, 474.9 | 269.9 M / 59.5 M / 435.1, 460.6 |
| S3 transfer, 1 000 000 accounts | 315.3 M / — / 545.0, 609.3 | 385.7 M / — / 577.6, 589.4 (raised budgets) |
| S3 reserve, new holding | 320.8 M / — / 578.7, 617.5 | 399.4 M / — / 592.4, 623.9 (raised budgets) |

Per-operation interpreted gas (`curve-bench`, the runtime's toolchain and profile, 20 iterations):
scalar mult 3 921 241, point add 448 733, wide reduction 12 058, scalar mul 26 251, scalar add 22 830.

**Reading.** On the interpreter the precompile path is *more* work, not less: 40–50 % more kernel
gas in S2 and about 22 % more Invoke gas in S3. The software path multiplies by the basepoint with
dalek's precomputed table; the shim turns that into a variable-base multiplication of the compressed
basepoint (`RistrettoBasepointTable`'s `Mul`, `pvm/proof/precompiles/src/ristretto.rs:92-97`), every
operation decompresses and recompresses its points, and each call adds a round trip through the
runtime. The smaller actor offsets part of it at the caller.
The precompile only pays off where the operation runs natively, which is the decision below.

## For the maintainer to decide: native curve arithmetic for Agents

The speed the precompile exists for (the service runtime's host-native handler) needs the Agent
runtime to leave the interpreter for each operation. On this path that means one of:

1. **A metered outer host call for the runtime guest**, like the experimental state path's block
   fetch (`0x180`, `vos/src/agent/state_block_pvm.rs:286-331`). It conflicts with the stated design
   that the infrastructure runtime's only outer host calls are the standard machine calls
   (`vos/src/crypto/blake2b.rs:44-46`), and with transition proofs: the precompile AIR does not
   constrain the curve relation today (`vos/src/runtime.rs:268-272`), so a proved transition could not
   rely on it until that AIR is complete.
2. **The inner-machine host serving 110–114 natively**, below the runtime. Same two objections, and it
   moves actor host-call semantics out of the runtime that owns them.
3. **A compiled execution path for Agents** (F-025 `T-025-06`), which would speed up this code and
   everything else interpreted, and keeps the arithmetic inside the proved guest.

For scale: natively, on the same host, the whole kernel reserve takes about 0.3 ms and a witness
verification at 1 000 000 accounts about 1.4 ms (aledgent `notes/MEASUREMENTS-F002.md` §4, §6),
against 45–70 M and 90–100 M gas, tens to hundreds of milliseconds, in the interpreter.

Smaller points in this branch:

- **Price versus count cap.** This branch charges actor gas. If you prefer BLAKE2b's model, a count cap
  alone must stay near 1 000 scalar multiplications per invocation to fit the 5 G management budget.
- **Per-invocation budgets for precompile-heavy actors.** A witness verification in precompile mode
  exceeds the host-call and BLAKE2b budgets (above). Pricing BLAKE2b by gas as well, instead of by
  count, would lift that limit without letting the runtime's work escape the actor's gas.
- **The shim CipherClerk pins.** `zkpvm-precompiles` at `9f5337f` emits unmarked `ecall`s, which the
  current compiler traps. Moving it to the current shim is a CipherClerk change; it is
  why the probe here builds against `pvm/proof/precompiles`.
