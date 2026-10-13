<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Opt-in recompiled execution for Agents, gas-exact

Agents run only on the PVM interpreter today (PVM: the Polkadot Virtual Machine that VOS executes).
This PR lets a host choose VOS's existing x86-64 recompiler for the Agent path: for the outer
AgentRuntime and for every actor machine it starts. The interpreter stays the default everywhere,
nothing selects the recompiler implicitly, and gas is identical to the unit. On real x86-64 hardware,
one reservation in our ledger's core Agent drops from 3.15 s to 0.28 s at p50 (3.32 s to 0.34 s at
p99), with the same gas and the same bytes out.

## Problem

`pvm/runtime/src/refine.rs:10-12` at `2bb78760` says it outright: Refine execution "always uses the
interpreter — never the kernel or the recompiler — on every target". Refine is the execution model the
Agent path uses: `RefineContext` runs the AgentRuntime, and the runtime starts each actor as an inner
machine through the standard host calls 9–14 (`machine`, `peek`, `poke`, `pages`, `invoke`, `expunge`).
The recompiler exists (`pvm/runtime/src/recompiler/`: `mod.rs`, `codegen.rs`, `asm.rs`, `signal.rs`),
but only the kernel path uses it: `pvm/runtime/src/backend.rs:73-108` picks it by default on Linux x86-64, or by
`GREY_PVM`. As we discussed, interpreted-only Agents are a prototype artefact that has to change.

Measured on an AMD EPYC Genoa (16 vCPU, x86-64 Linux), 2026-09-29: one reservation in our core Agent
(a 2 168 623-byte actor program) costs 1 747 M gas per Invoke. The interpreter runs it at about
0.56 G gas per second, so the whole round takes 3.15 s at p50 and 3.32 s at p99. We need a decision
per command in tens of milliseconds.

Nothing about gas ties it to the interpreter. The Gray Paper v0.8.0 (the JAM specification VOS pins)
charges gas per basic block, in advance, at block entry (A.8), and defines a block's cost as a static
function of its instructions (§A.9, A.54). VOS computes that cost once, for both engines: the
interpreter's `compute_block_gas_costs` "uses the same GasSimulator as the recompiler — single code
path" (`pvm/runtime/src/interpreter/mod.rs:3254-3256`), and the generated code charges it at each block
entry (`emit_gas_block_start`, `recompiler/codegen.rs:1358-1394`). `pvm/SPECIFICATION.md:7-9` already
requires interpreter and recompiler conformance whenever the pin moves, and `:14-15` says VOS "must not
add host instructions or change standard PVM execution". This PR adds no host instruction and changes
no execution.

## Change

One commit, 10 files, +2 210 −392.

**`pvm/runtime/src/refine.rs`: explicit engine selection.**
- `RefineBackend { Interpreter (default), Recompiler }` (`:129`), with `is_available()`: true only with
  `std` on Linux x86-64.
- `PreparedProgram::with_backend(blob, backend)` (`:186`) compiles the outer program once. `new` keeps
  meaning the interpreter.
- `RefineError::BackendUnavailable` (`:81`): selecting an engine that does not exist here is an error,
  never a fallback.
- `Machine` holds either engine. The host uses engine-neutral accessors with the interpreter's exact
  rules (no wrap past 2³², zero-length ranges always accessible): `is_readable`, `is_writable`,
  `read_bytes_checked`, `write_bytes_checked`, and `charge` and `credit`, rewritten over
  `gas_remaining`/`set_gas`. `Invocation::read_memory` is new and `output_bounded` works on both
  engines.
- Interpreter-only by design, and they panic on a recompiled machine: `Machine::interpreter()`,
  `memory()`, `memory_mut()`, `resume_observed()` and `Invocation::memory()`. The prover and every
  observed or profiled run therefore stay on the interpreter.

**`pvm/runtime/src/refine_host.rs`.** `RefineContext::load_with_backend` (`:98`). `load_prepared` takes
its inner machines from the preparation, so a recompiled preparation runs its actors natively too.
`run_observed` asserts the interpreter. The inner-machine host calls reach outer memory through the
neutral accessors.

**`pvm/runtime/src/inner.rs`: native inner machines.** `InnerMachines::with_backend` (`:397`). Each
native inner machine is a `RecompiledPvm` on its own reserved 4 GiB mapping, every page inaccessible
until `pages` maps it. `NativeInnerProgramCache` (`:321`) keeps four compiled inner programs per
preparation, most recent first, keyed by exact code-blob bytes.

**`pvm/runtime/src/recompiler/mod.rs`: what the recompiler lacked.**
- `NativeProgram` (`:707`): compile once, position-independent, shared through an `Arc`; any number of
  machines are built over it with `RecompiledPvm::from_program` (`:848`).
- `set_page_range` (`:1400`) changes permissions in the software table and with `mprotect` together;
  `clear_pages` (`:1440`) zeroes with `madvise(MADV_DONTNEED)`. This is what `pages` needs.
- Bulk, range-checked `read_bytes_checked` and `write_bytes_checked` (`:1359`, `:1378`) for the host.
- **A fix: the funded-block flag after halt and panic** (`normalize_standard_terminal_boundary`,
  `:1058`). The interpreter leaves a halted or panicked machine at counter zero with the exiting block
  funded (A.11). The generated code clears the flag before every terminator, so re-invoking a halted
  inner machine charged block zero a second time (2 gas more on `alu32_add_32_sign_extends`). An outer
  program may re-invoke a halted inner machine, so on the Agent path this was observable. The fix sits
  in `RecompiledPvm`, gated on the standard (conformance) ISA mode; the kernel path runs compiled code
  through its own loop in `kernel.rs` and is not affected.

**`vos/src/agent`: selection per host or per Agent.**
- `AgentExecutionConfig { default, agents: BTreeMap<AgentId, RefineBackend> }` (`runtime_pvm.rs:25`),
  with `validate`, which refuses an unavailable engine.
- `AgentHost::configure_execution` (`host.rs:2235`) applies it to every hosted Agent and to Agents
  created later. `LocalJournalAgentDriver::set_refine_backend` and `StandardLocalReplayExecutor` carry
  the engine; the executor's `RuntimePreparationCache` is keyed by program bytes **and** engine.
- `state_block_pvm.rs` switches three memory calls to the neutral accessors; it still loads on the
  interpreter.
- `driver.rs` and `private_host.rs` (`runtime_pvm::execute_*`) are untouched and stay on the
  interpreter. These `vos` items are crate-private and nothing calls `configure_execution` yet, so no
  daemon, `vosx` flag or external host can select the recompiler through `vos` today. Our measurements
  select it through the public `vos-pvm` API (`PreparedProgram::with_backend`,
  `RefineContext::load_with_backend`).

**Public API.** Additive in `vos-pvm`, with one source-level caveat: `RefineError` gains a variant and is
not `#[non_exhaustive]`, so an exhaustive `match` outside the workspace would need an arm (none exists
inside it). No change in `vos`'s public API, the runtime ABI, the SDK, journals, wire formats or the gas
schedule.

## Design choices and alternatives

- **Opt-in and explicit, not by platform.** The kernel path's `GREY_PVM` and platform default do not
  reach Refine (a test sets `GREY_PVM=recompiler` to prove it). An engine that changes the trust
  boundary (below) should be an operator's choice per Agent.
- **An error, not a fallback, when the engine is missing.** A silent fallback would make a
  configuration mistake look like a slow host.
- **Host configuration, not state.** The engine is not recorded in any descriptor, journal record,
  transition or proof, because nothing they contain can tell the engines apart. Nodes may pick
  different engines and produce the same transitions; a driver opens and replays its journal on the
  interpreter, and the host's selection applies from then on.
- **The prover stays on the interpreter.** Transitions are byte-identical, so the public IO a proof
  binds is the same; a node can serve an Agent natively and prove the transition by interpreted
  re-execution.
- **Deferred: ahead-of-time compilation at install.** Compile at actor install, cache keyed by
  `(ProgramId, compiler identity, target)` in a host-owned directory, never shipped with a package.
  It needs a code-generator version constant you own, a place in the host layout and eviction. The
  in-memory compile-once here already removes compilation from every call but the first; compiling
  the 1.97 MB core took 136 ms (27.8 MB of native code) against 113 ms of interpreter predecode.
- **Deferred: a sandbox process for untrusted actors,** as PolkaVM does (a separate minimal process,
  seccomp, shared memory). It costs a context switch per `machine` and `invoke` call; the host could
  select it per Agent through the same configuration.
- **Not attempted: an AArch64 code generator.** On aarch64 `Recompiler` is unavailable and the
  interpreter runs as today. The lockstep tests here compare engines, not architectures, so they would
  be its acceptance suite.

**Security: the trust boundary moves.** The interpreter confines a guest by construction; a bug gives a
wrong answer. The recompiler turns actor bytes into machine code in the host process, and confinement
then rests on the code generator: R15-relative addressing into a 4 GiB mapping with a guard page,
`mprotect` per page, the SIGSEGV handler, and W^X (code mapped read-write while emitted, then
read-execute). A code-generation bug can become a host compromise. Hence opt-in per Agent, for actors the
operator trusts. Two operational notes: the signal handler is process-wide and its alternate stack is
installed on the first thread only (`signal.rs:51-123`, unchanged here); and `RecompiledPvm` gains an
`unsafe impl Send` (`mod.rs:782`), which deserves your review.

## Compatibility

- **Existing hosts and Agents:** unchanged. Every executor starts on the interpreter, byte for byte as
  before, and stays there unless a host calls `configure_execution`.
- **Storage, journals, wire, ABI, proofs, gas:** unchanged. Replay on either engine gives the same
  transitions.
- **Targets:** the recompiler exists only with `std` on Linux x86-64. Elsewhere, including macOS
  x86-64 and every aarch64 host, selecting it returns `BackendUnavailable`.
- **Migration:** none. A host that wants the recompiler calls `configure_execution` (inside `vos`) or
  prepares with `RefineBackend::Recompiler` (through `vos-pvm`).

## Tests

New, in `pvm/runtime/tests/refine_backends.rs` (Linux x86-64 only). Each runs both engines in lockstep
and compares every observable: exit, counter, registers, gas, every mapped byte and the expunge counter.

- `v080_vectors_as_inner_machines_agree_across_backends`: all 189 Gray Paper v0.8.0 vectors (173
  instruction, 16 block-gas) as inner machines, continued after a host call, a page fault, out-of-gas,
  halt and panic.
- `every_gas_budget_exits_identically_across_backends`: each vector at every budget from 0 up to its
  full cost, capped at 602 gas for the long vectors, with one-unit continuations after each out-of-gas.
- `nested_refine_programs_agree_across_backends_at_every_budget`: outer programs that create, invoke,
  fault, map and retry inner machines, across their last 8 000 gas.
- `inner_machines_of_one_program_are_independent_on_the_recompiler`,
  `prepared_recompiled_programs_reuse_code_not_state`, `refine_never_selects_the_recompiler_implicitly`,
  `native_code_is_deterministic_and_position_independent`.

Also, on every target: `refine::tests::engine_selection_is_explicit_and_defaults_to_the_interpreter`
(`vos-pvm`), and `runtime_preparation_cache_is_keyed_by_engine_and_engines_agree` and
`execution_config_is_explicit_per_agent_and_refuses_unavailable_engines` (`vos`,
`local_journal_driver`). In `vos/tests/agent_runtime_pvm.rs`, every AgentRuntime execution now runs on
both engines and compares exit, counter, registers, gas and output (not memory), plus two new tests:
`faulting_and_spinning_actors_fail_identically_on_both_engines` and
`agent_executions_match_across_engines_at_sampled_gas_budgets` (75 budgets across a yielding
invocation). Off x86-64 Linux they exercise the interpreter only.

**Run on x86-64 Linux** (AMD EPYC Genoa, 16 vCPU, `rust:1-bookworm`, `nightly-2025-05-09`, this PR's
head `3140c3d5`, 2026-09-25), with the kernel path on each engine in turn. No failures:

| Suite | Passed | Failed | Ignored |
|---|---:|---:|---:|
| `vos-pvm` library | 264 | 0 | 2 |
| `vos-pvm` `pvm_vectors` (v0.8.0 instruction and block-gas vectors, both engines) | 20 | 0 | 0 |
| `vos-pvm` `refine_backends` | 7 | 0 | 0 |
| `vos-pvm` `spi` | 4 | 0 | 0 |
| `vos --lib` | 1 511 | 0 | 2 |
| `vos --test agent_runtime_pvm` | 6 | 0 | 1 |

The ignored tests are VOS's own perf, measurement and fixture tests. The commands, from the VOS root:

```sh
cargo test --locked --no-fail-fast -p vos-pvm
GREY_PVM=interpreter cargo test --locked --no-fail-fast -p vos-pvm
# the two fixtures `just test` builds first
(cd examples/agent-runtimes/custom-linear && \
  CARGO_TARGET_DIR=$T/scripted cargo actor --locked --features scripted-fixture)
export AGENT_SCRIPTED_RUNTIME_ELF=$T/scripted/riscv64em-vos/release/custom_linear_agent_runtime.elf
cargo build --locked -p echo-extension
cargo test --locked --no-fail-fast -p vos --lib
GREY_PVM=interpreter cargo test --locked --no-fail-fast -p vos --lib
cargo test --locked --no-fail-fast -p vos --test agent_runtime_pvm
# only the new cross-engine tests
cargo test -p vos-pvm --test refine_backends
```

**Lockstep through the runtime ABI** (our host runs every execution on both engines and compares exit,
counter, registers, gas and output): `state-blob` (66 KB actor) and `kernel-probe` (291 KB), 63
executions each plus one Invoke swept over 276 outer and 147 actor gas budgets, identical under
emulation; our core Agent, 75 executions identical (1.97 MB build, 2026-09-25) and 227 identical
(2.17 MB build, 2026-09-29), both on the EPYC.

**Timing** (EPYC, 2026-09-29; the core Agent's reservation, 100 samples per run after 3 warm-up, two
runs per engine; the runtime built from this branch with #6 applied and both program limits raised to
2 560 KiB so the core fits):

| Engine | Round p50 | Round p99 | Gas per Invoke | Actor gas |
|---|---:|---:|---:|---:|
| Interpreter | 3 151 / 3 161 ms | 3 317 / 3 277 ms | 1 747.19 M | 294.56 M |
| Recompiler | 284 / 285 ms | 344 / 344 ms | 1 747.19 M | 294.56 M |

About 11× less wall time at p50, 9.6× at p99, at identical gas: 6.4 G gas per second against 0.56 G.
The Acknowledge drops from 28 ms to 4.7 ms.

**Not run:** VOS's `just test` recipe as such (it passes `--offline --test-threads=1`; the run above is
multi-threaded); the ignored `compiled_runtime*` gates; this branch combined with #8; any code-generator
fuzzing; any multi-threaded host with recompiled Agents on several worker threads.

## Relation to the other pull requests

All five VOS pull requests (#6 to #10) are one commit each on `saga/agents` at `2bb78760`, which is still
the tip (checked 2026-09-29). Each applies alone without conflict. This one shares
`vos/tests/agent_runtime_pvm.rs` with #7 (merges cleanly) and `pvm/runtime/src/inner.rs`, `refine.rs`
and `refine_host.rs` with #8. `git merge-tree` finds **nine conflicting hunks with #8** (five in
`inner.rs`, two each in the others): both add a per-preparation cache of inner programs, #8 of
predecoded interpreter programs and this one of native programs, with the same shape (four entries,
most recent first, exact bytes). They should become one cache holding either payload, handed to
`InnerMachines` on both engines. We suggest merging #8 first; we will rebase this branch onto it and
unify the cache. #6 + #7 + #9 + #10 merge together cleanly. This PR also answers most of #9's question
about native curve arithmetic: compiled execution speeds up the curve code while keeping it inside the
proved guest.

## Questions for you

1. **Where the setting lives.** `AgentExecutionConfig` exists but nothing outside `vos` can reach it.
   A node or daemon configuration key and a `vosx` flag, or a public entry point on the host? The
   default stays the interpreter.
2. **Failure handling.** Some native failures panic rather than return errors: an inner program that
   fails to compile or map (`.expect` in `inner.rs`), and `mprotect` or `madvise` failures (asserts).
   A guest that alternates page permissions could push the host towards `vm.max_map_count`. Should
   these become refusals?
3. **Address space.** Each native inner machine reserves about 4 GiB plus a 1 MiB permission table. At
   63 inner machines per context that is about 250 GiB of virtual space. Acceptable, or pool mappings?
4. **Untrusted actors.** Recompile only trusted Agents in process, with a sandbox worker later, or no
   recompiling of untrusted actors until then? Fuzzing the code generator against the lockstep oracle is
   the natural follow-up.
5. **Ahead-of-time compilation** as designed above, and the code-generator version constant it needs.
6. **Order with #8** and the unified cache.
7. **Runtimes above 1 280 KiB** bypass the `RuntimePreparationCache` and would recompile on every call.
   The bundled runtime (1.08 MB) fits; worth a bound that follows the limit.
8. A doc comment in `refine_backends.rs:566` refers to "VOS proposal 0005", our internal draft name.
   We will reword it to describe the design directly if you want the PR amended.

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent: every balance-affecting
command is decided inside the Agent, and the caller needs that decision within a completion budget of
tens of milliseconds. With this PR the engine stops being the obstacle: a reservation goes from over
three seconds to under 0.35 s at p99 with the gas unchanged. What remains is gas, mostly the
AgentRuntime hashing the actor program about three times per call (once #8 is in, about 880 M of a
1 136 M-gas reservation, 77 %), which is the question #8 puts to you.
