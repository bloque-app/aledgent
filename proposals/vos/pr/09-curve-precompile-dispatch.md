<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Discussion: serve curve precompile ECALLs 110–114 to Agent actors (not for merge)

> **For discussion, not for merge.** The patch works: an actor built with CipherClerk's
> `pvm-precompile` feature now runs on the Agent path, gas-metered, with the same results as the
> software build. But on the interpreter it is **slower** than CipherClerk's own software arithmetic.
> Making curve arithmetic fast for Agents is a design decision that belongs to you. The code is here to
> make the questions concrete.

## Problem

CipherClerk (a confidential ledger kernel built on Ristretto, the prime-order group over Curve25519)
has a `pvm-precompile` feature that replaces its scalar multiplications, point additions and scalar
reductions with the precompile ECALLs 110–114 (an ECALL is a guest-to-host call;
`pvm/proof/precompiles/src/ecalls.rs:16-30`, BLAKE2b is 100 at `:36`).

The Agent path cannot serve them. At `2bb78760` the AgentRuntime's actor host-call dispatch
(`vos/src/agent/execution.rs:951-1237`, inside `run_inner_actor_with_storage` at `:743`) handles
`SUSPEND` (before the match, `:928-950`), `GAS`, `ACTOR_EFFECT_EXPORT`, `STORAGE_R`, `PREIMAGE_LOOKUP`,
`FETCH`, `GROW_HEAP`, BLAKE2b (100) and `DEBUG_WRITE`, and ends anything else in
`UnsupportedHostCall` (`:1237`). The only handler for 110–114 is the service runtime's host-native one
(`vos/src/runtime.rs:286-336`), which Agent actors never reach.

A second, separate obstacle, which is CipherClerk's to fix: CipherClerk `e9a7b08` pins
`zkpvm-precompiles` at VOS `9f5337f`, whose shims emit a bare `ecall`. Today's compiler turns an
unmarked `ecall` into a trap (`pvm/compiler/src/riscv.rs:328`); only `csrw 0x801` before it makes an
`ecalli` (`:322-327`). The current shims (`pvm/proof/precompiles`, package `vos-pvm-precompiles`) emit
the marker. Our measurements build the probe with that one dependency line changed.

## Change

One commit, 4 files, +269 −1; 129 lines are the test.

- **`vos/src/agent/execution.rs`.**
  - A dispatch arm for 110–114 (`:1237-1259`, under `agent-runtime`, as BLAKE2b is) charges the
    operation's price to the **actor's gas** first. Too little gas ends the invocation `OutOfGas` with
    nothing computed. The call also counts against `MAX_EXECUTION_HOST_CALLS` (4 096, `:57`), as every
    host call does.
  - `curve_precompile` (`:1302-1377`) reads the operands from actor memory, computes with
    `curve25519-dalek` directly, and writes the result back: 110 `k·P`, 111 `P+Q`, 113 `a·b` and 114
    `a+b` write through φ9; 112 reduces 64 bytes at φ7 and writes through φ8. The register ABI matches
    the shims and the prover's tracer (`pvm/proof/src/core/tracing.rs:1063-1116`). A non-canonical
    scalar or an invalid point gives the all-zero sentinel, as the shims' host fallback does. φ7 and φ8
    are written back with their own values; argument registers are preserved.
  - An invalid actor pointer ends the invocation `InvalidInput`, as BLAKE2b does on this path.
  - Prices (`:73-104`): scalar mult 4.5 M, point add 520 k, wide reduction 14 k, scalar mul 30 k, scalar
    add 26 k gas. They are each operation's measured interpreted cost with the runtime's toolchain and
    profile (3.92 M, 449 k, 12.1 k, 26.3 k, 22.8 k) plus about 15 %.
- **`vos/Cargo.toml`:** `curve25519-dalek = { version = "4.1", default-features = false, optional = true }`,
  enabled by `agent-runtime`. **`Cargo.lock`, `services/agent-runtime/Cargo.lock`:** one edge each; 4.1.3
  was already in the graph through `ed25519-dalek`.

No change to the ABI, the SDK, the continuation encoding or the bundled artifact.

## Design choices and alternatives

- **Computed as runtime guest code, not by an outer host call.** `vos/src/crypto/blake2b.rs:44-46`
  states the rule: "The infrastructure runtime deliberately has no private outer-host ABI. Hashing here
  is ordinary deterministic guest computation so its only outer host calls are standard PVM machine
  management (9..=14)." This PR follows it: the runtime's outer surface is still calls 9–14. As a
  consequence, a transition proof of the runtime constrains the curve arithmetic as ordinary RISC-V.
  The service runtime's handler cannot say that: "the current AIR records the observed inputs and
  outputs without constraining the curve/scalar relation" (`runtime.rs:269-272`).
- **A price, not only a count cap as for BLAKE2b.** A BLAKE2b compression costs thousands of gas, so a
  count cap bounds the runtime's work. A scalar multiplication costs about 4 M; 4 096 of them would
  exceed the 5 G management budget (`DEFAULT_MANAGEMENT_GAS`, `vos/src/agent/driver.rs:44`) and fail
  the whole runtime execution instead of the actor. Charging the actor keeps the work inside its own
  gas. This is the first Agent host call that charges actor gas, which sets a precedent.
- **The cost on the interpreter.** The software path multiplies by the basepoint with dalek's
  precomputed table. The shim turns that into a variable-base multiplication, every call decompresses
  and recompresses its points, and each call is a round trip through the runtime. So the precompile
  build does more work, not less.

## Compatibility

- Actors that do not issue 110–114 are unaffected; the software build's gas is the same to the unit
  before and after.
- No ABI, SDK, wire or continuation change. The runtime's ProgramId changes when
  `vosx/blobs/agent_runtime.pvm` is rebuilt from this source; until then hosts on the bundled runtime
  see no change.
- Builds without `agent-runtime` still return `UnsupportedHostCall`.

## Tests

New: `agent::execution::tests::inner_curve_precompiles_match_host_fallback_and_charge_actor_gas`
(`vos/src/agent/execution.rs:3233`, `agent-runtime`). An assembled actor issues all five ECALLs; each
output equals `vos_pvm_precompiles`' host fallback; the scalar multiplication equals an independent
`15·B`; invalid points give the sentinel (for 110 and 111); φ7 is preserved; the five prices are
charged; an actor given one gas less than the scalar-mult price ends `OutOfGas`.

```sh
cargo test -p vos --lib --features agent-runtime inner_curve_precompiles_match_host_fallback_and_charge_actor_gas
cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'
```

Run on Linux aarch64, `nightly-2025-05-09`:

| Command | Result |
|---|---|
| `cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'` | 1 779 passed, 17 ignored, 7 failed; the same 7 fail on unmodified `2bb78760` |
| the same with `--ignored compiled_runtime compiled_shared_genesis` and `AGENT_RUNTIME_CANDIDATE_ELF` built from this branch | 4 passed |
| `vos` with `experimental-state-blocks`, filtered to `agent::wire`, `replay`, `state_block*`, `execution`, `actor_storage` | 220 passed, 26 ignored |

**Not run:** the two recompiler tests (x86-64 only), the five that need prebuilt artifacts, and this
branch on x86-64. The test checks register preservation for φ7 only, not every argument register.

**Numbers.** A CipherClerk kernel probe in a Local Agent, 200 invocations per shape. "Kernel gas" is the
Invoke's runtime gas minus the same actor's no-op Invoke:

| Case | Software arithmetic, before: Invoke / kernel gas | Precompile, this PR: Invoke / kernel gas |
|---|---:|---:|
| Transfer | 262.8 M / 40.6 M | 269.9 M / 59.5 M |
| Reservation, new holding position | 268.2 M / 45.9 M | 278.9 M / 68.4 M |
| Witness verification, 1 000 000 accounts | 315.3 M | 385.7 M (only with host-call budgets raised 16×) |

Before this PR the precompile build ended in `UnsupportedHostCall(112)`. The precompile path costs
40–50 % more kernel gas. The witness verification exceeds `MAX_EXECUTION_HOST_CALLS` (4 096) and the
BLAKE2b budget (`MAX_EXECUTION_BLAKE2B_COMPRESS_CALLS`, 2 568, `:70-71`), because the precompile build
also sends each BLAKE2b block through ECALL 100: an existing limit of the BLAKE2b path. Natively, the
same kernel reservation takes about 0.3 ms.

## Relation to the other pull requests

All five VOS pull requests (#6 to #10) are one commit each on `saga/agents` at `2bb78760`, still the tip
(checked 2026-09-29), and each applies alone. This one shares `vos/Cargo.toml` and
`vos/src/agent/execution.rs` with #7, in different regions. `git merge-tree` merges it cleanly with each
of #6, #7, #8 and #10. #10 answers most of question 1 below.

## Questions for you

1. **Native curve arithmetic for Agents, and how.** (a) A metered outer host call for the runtime guest:
   it breaks the 9–14 rule, and a proved transition could not rely on it until the precompile AIR
   constrains the curve relation. (b) The inner-machine host serving 110–114 natively, below the
   runtime: the same objections, and it moves actor host-call semantics out of the runtime that owns
   them. (c) Compiled Agent execution (#10): speeds this and everything else, keeps the arithmetic in the
   proved guest, but only on x86-64 Linux. Our preference is (c).
2. **Merge this interim path at all**, for correctness and parity, or wait for (1)?
3. **The actor gas cap.** `MAX_EXECUTION_GAS` is 1 G (`execution.rs:42`), so at 4.5 M an invocation fits
   at most about 222 scalar multiplications. With a count cap instead, it would have to stay near 1 000
   to fit the 5 G management budget. Price, count, or both?
4. **Basepoint multiplication.** The prover already detects a basepoint input
   (`detect_scalar_mult_kind`, `tracing.rs:132-141`). The runtime could do the same and use a
   precomputed table, recovering much of the loss, at the cost of dalek's `precomputed-tables` and a
   larger runtime.
5. **Pricing BLAKE2b by gas** too, which would lift the host-call limit for precompile-heavy actors.
6. **Proof cost.** Constraining the arithmetic as ordinary RISC-V means proving about 3.9 M
   instructions per scalar multiplication. Acceptable for Agent transitions?
7. **CipherClerk's shim pin** needs to move to `pvm/proof/precompiles`, which also means
   `package = "vos-pvm-precompiles"` on that dependency line. Some comments there still say
   `ecalli 200-204`.

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent and applies CipherClerk
transfers inside the guest, within a completion budget of tens of milliseconds per command. Curve
arithmetic is a large part of the ledger's own work, and CipherClerk already has a precompile build for
it. We need to know which way VOS will make that arithmetic cheap on the Agent path, so that we build
the kernel the way the platform intends. Measured today, the answer for us is compiled execution (#10),
with the software build.
