<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0005 — Recompiled execution for Agents: opt-in and gas-exact

**Draft pull request description and design note, 2026-09-24.** Not yet opened upstream.

| | |
|---|---|
| Branch | `agent/recompiled-agent-execution` on aledgent's VOS fork, one commit on `saga/agents` at `2bb7876` |
| Commit | `3140c3d5` |
| Size | 10 files changed, 2210 insertions(+), 392 deletions(-) |
| Independent of | 0001–0004; applies alone to `saga/agents`. |
| aledgent task | `T-025-06` (issue #66), `DEC-16` |

**Read this first.** @olanod (VOS's maintainer) confirmed on 2026-09-24 that Agents running only on the interpreter is a
prototype artefact that has to change. This branch is that change, and it is opt-in. The
interpreter stays the default everywhere. A host can select VOS's existing x86-64 recompiler
for all its Agents or for named ones. Nothing selects it by platform or environment. On the
Agent path the recompiler now runs the outer AgentRuntime and every actor it starts. The two
engines are indistinguishable, and the tests hold them to that: the same exits and counters,
registers, memory, page faults, host-call exits, and gas to the unit, including out-of-gas at
every block boundary.

Three things are in the branch:

1. **The wiring** (`pvm/runtime`, `vos/src/agent`). The recompiler lacked five things the
   Agent path needs. The largest was a native Refine inner machine. The branch adds them, plus
   one boundary fix, and the list is below.
2. **Differential conformance.** The Gray Paper v0.8.0 vectors run on both engines, as outer
   machines (VOS's existing test) and now also as inner machines at every gas budget. There
   is a lockstep test of the real AgentRuntime with real actors. Every execution in
   `vos/tests/agent_runtime_pvm.rs` now runs on both engines and is compared.
3. **Ahead-of-time compilation.** It is a design section, not code. The branch compiles once
   per runtime preparation and caches native code in memory. Persisting it at install is
   specified below and left to a follow-up.

Everything ran under Rosetta 2 on Apple silicon (`linux/amd64` Docker). JIT code, the
SIGSEGV-based page-fault path and all the tests work there. **Timing under emulation is not a
result.** It appears below only as a ratio between the two engines under the same emulation.
The absolute numbers need a real x86-64 host.

Everything below the line is the pull request text.

---

## Problem

An Agent's code runs only on the interpreter. `RefineContext`, the outer AgentRuntime machine and
every inner actor machine it starts have no other engine. The comment at
`pvm/runtime/src/refine.rs:10-12` says so: execution "always uses the interpreter — never the
kernel or the recompiler — on every target". The recompiler in `pvm/runtime/src/recompiler/`
serves only the older kernel path. @olanod confirmed this is a prototype artefact that has to
change.

It is also the remaining term in aledgent's reservation budget. With 0001 to 0004 applied, one
reservation in aledgent's core Agent is about 1.14 G gas, and it takes 2.36 s at p99 on the
reference host (about 500 M gas per second, interpreted). The target is 50 ms.

## Why this stays inside VOS's contract

**VOS already binds both engines to the same machine.** `pvm/SPECIFICATION.md` pins Gray Paper
v0.8.0 and requires "interpreter and recompiler conformance" whenever the pin moves. It also says
that VOS-specific behaviour "must not add host instructions or change standard PVM execution".
This branch adds no host instruction and changes no execution. It lets the Agent path use the
second conforming engine the specification already names.

**Gas is a property of the program, not of the engine.** Gray Paper 0.8.0:

- **A.8** (p. 38) charges gas per basic block, in advance, on entering the block. If the block is
  underfunded, execution stops with out-of-gas and "the gas counter remains unchanged".
- **§A.9 and A.54** (pp. 47–48) define a block's cost as a static function of its instructions.
  It is a pipeline simulation, `max(x_c − 3, 1)` cycles of a virtual CPU. Memory access costs
  `m ≡ 25` (**A.64**, p. 49).

Nothing in either depends on how the block is executed. VOS already computes it once, in one
place: the interpreter's `compute_block_gas_costs` "uses the same GasSimulator as the recompiler
— single code path" (`interpreter/mod.rs:3254`). The recompiler charges the same costs at the
same block entries (`recompiler/codegen.rs:310`). An engine that charges those costs at those
entries is gas-exact by definition. The tests below check the rest: the same exits, memory and
continuations.

**The recompiler exists and conforms.** It has four files: `mod.rs`, `codegen.rs`, `asm.rs` and
`signal.rs`. It meters per block at entry with the same `GasSimulator`. `PvmBackend`
(`backend.rs`) selects it by default on Linux x86-64 for the kernel path. `tests/VECTORS.md`
runs 170 of the 172 v0.8.0 instruction vectors and all 16 block-gas vectors on both engines.
The two that run on the interpreter only are specific to the historical per-instruction gas.

**Refine gets its code from outside the VM.** In §B.3–B.6 (pp. 54–55), Refine's code is
`Λ(δ[w_s], (p_c)_t, w_c)`: a historical lookup by code hash, done by the invocation, not by the
guest. Its standard host calls include `fetch` and `historical_lookup`. This matters for the
last section.

## The change

### 1. Explicit engine selection (`pvm/runtime`)

`RefineBackend { Interpreter (default), Recompiler }` names the engine. It is selected in three
places:

- `PreparedProgram::with_backend(blob, backend)`, which compiles the outer program once;
- `RefineContext::load_with_backend`;
- `InnerMachines::with_backend`.

`PreparedProgram::new`, `RefineContext::load` and `InnerMachines::new` keep their meaning:
interpreter. **Nothing selects the recompiler implicitly.** The platform default and `GREY_PVM`
of the kernel path do not reach Refine, and a test sets `GREY_PVM=recompiler` to show it.
Selecting the recompiler where it does not exist is `RefineError::BackendUnavailable`, never a
fallback. A context loaded from a recompiled preparation runs its outer machine and every inner
machine natively.

`Machine` holds either engine. The host calls use engine-neutral accessors (`is_readable`,
`is_writable`, `read_bytes_checked`, `write_bytes_checked`, `charge`, `credit`). They follow
exactly the interpreter's rules: no wrap past 2^32, and zero-length ranges are always
accessible. `Invocation::read_memory` and `output_bounded` work on either engine.

`memory()`, `interpreter()`, `run_observed` and `invoke_observed` are interpreter-only by
design, and they panic on a recompiled machine. **The proof tracer and every observed or
profiled run stay on the interpreter.**

### 2. What the recompiler lacked, and what this branch adds

| Needed by the Agent path | At `2bb7876` | This branch |
|---|---|---|
| Host-call exits back to the runtime, resumable | Present: `ecalli` exits with `host_pending`, and `acknowledge_host_call` resumes at the successor without re-charging | Used as is |
| Page faults | Present, via the SIGSEGV handler and trap table. Permissions were installed once (`set_page_perms`) | `set_page_range` changes a page range in the software table and in `mprotect` together. `clear_pages` zeroes with `MADV_DONTNEED`. This is what `pages` needs |
| Refine inner machines (`machine`, `peek`, `poke`, `pages`, `invoke`, `expunge`) | Absent: inner machines were interpreters only (`inner.rs`) | A native inner machine on its own reserved 4 GiB mapping, all pages inaccessible until mapped. Its Ω_K boundary matches the interpreter's (below) |
| Compiled code shared between machines | Absent: `RecompiledPvm::new` compiled per instance | `NativeProgram` compiles once. It is position-independent, so `RecompiledPvm::from_program` builds any number of machines over one `Arc`. Inner programs are cached per runtime preparation by exact code-blob bytes (`NativeInnerProgramCache`, four programs, most recent first) |
| The "gas already charged for this block" flag | Kept across host calls and faults (`gas_charged`, `fast_reentry`), **but not across halt and panic** | Fixed. See below |
| Bulk memory access for the host | Byte by byte | Range-checked bulk copies |

**The funded-block flag after halt and panic was a real divergence.** The interpreter returns
from halt or panic with the exiting block still funded, as A.11 says, and Ψ resets the counter
to zero. Re-invoking that machine starts block zero without charging it again. The generated
code clears the flag before every terminator and did not reset `entry_pc`. So a halted or
panicked inner machine, invoked again, was charged for block zero a second time: 2 gas more on
`alu32_add_32_sign_extends`. An outer program may invoke a halted inner machine again, so this is
observable on the Agent path.
`RecompiledPvm` now leaves the standard terminal boundary where the interpreter does: counter
zero, block funded. This happens only for exits that ran native code. A refused entry leaves the
state unchanged, as in the interpreter. Removing the fix makes two of the new tests fail, so the
tests do cover it.

### 3. Selection per Agent or per host (`vos/src/agent`)

- `AgentExecutionConfig { default, agents }` (`runtime_pvm.rs`) is host-local configuration: an
  engine for all Agents and per-Agent overrides. `validate` refuses an unavailable engine.
- `AgentHost::configure_execution` applies it to every hosted Agent and keeps it for Agents
  created later.
- `LocalJournalAgentDriver::set_refine_backend` and `StandardLocalReplayExecutor` carry the
  engine. The executor's `RuntimePreparationCache` is keyed by the exact bytes **and** the
  engine.
- It is not state. No descriptor, journal record, transition or proof carries it, because
  nothing they contain can tell the engines apart. A driver opens and replays its journal on the
  interpreter, and the host's selection applies from then on.
- `state_block_pvm.rs`'s block-fetch host call uses the neutral accessors, so it can run on
  either engine. It still loads on the interpreter.

The older `driver.rs` and `private_host.rs` paths (`runtime_pvm::execute_*`) are untouched and
stay on the interpreter. No daemon or `vosx` flag is added yet. That is a one-line call to
`configure_execution` once @olanod chooses where the setting lives.

## Guarantees

- **Determinism, by the Gray Paper's own definition of gas.** A block's cost comes from the one
  `GasSimulator`, is charged at the same entries, and underfunding leaves gas unchanged on both
  engines (A.8). The same program with the same inputs gives the same result and the same gas on
  every node, whichever engine each node picked. The differential tests hold both engines to
  that at every exit and at every gas budget they sweep.
- **Conformance.** The v0.8.0 vectors run on both engines as outer machines (VOS's existing
  `pvm_vectors`). They now also run as inner machines, with continuations, at every budget
  (`tests/refine_backends.rs`).
- **Proofs.** The transition proof is unaffected. Transitions are byte-identical, so the public
  IO the proof binds is the same. The prover keeps executing on the interpreter:
  `run_observed` refuses a recompiled context, and `pvm/proof` never loads one. A node may serve
  an Agent natively and prove the same transition by interpreted re-execution.
- **Replay and journals.** Unchanged. The engine is not recorded anywhere, and replay on either
  engine reproduces the same transitions.

## Tests

In VOS's own suite:

- `vos-pvm`, new `tests/refine_backends.rs` (Linux x86-64). Each test runs both engines in
  lockstep and compares every observable:
  - `v080_vectors_as_inner_machines_agree_across_backends`: all 189 v0.8.0 vectors (173
    instruction, 16 block-gas) as inner machines. Each maps its pages, pokes its memory and is
    invoked. It then continues as an outer program can: again after a host call, after mapping a
    faulting page, with more gas after out-of-gas, and once more after halt or panic. Every
    step compares exit, gas, registers, every mapped byte and the expunge counter.
  - `every_gas_budget_exits_identically_across_backends`: every vector at every budget from 0 to
    past its whole run, with one-unit continuations after each out-of-gas. **Out-of-gas at every
    block boundary** is compared.
  - `nested_refine_programs_agree_across_backends_at_every_budget`: outer programs that create an
    inner machine and invoke it (host call, then trap), fault and map and retry, call the
    embedder, fault at the outer level and write read-only memory. Each runs at every budget
    across its last 8 000 gas.
  - `inner_machines_of_one_program_are_independent_on_the_recompiler`,
    `prepared_recompiled_programs_reuse_code_not_state`,
    `refine_never_selects_the_recompiler_implicitly`, and
    `native_code_is_deterministic_and_position_independent` (compiling a program twice gives the
    same bytes at different addresses).
- `vos-pvm` `refine`: `engine_selection_is_explicit_and_defaults_to_the_interpreter`, on every
  target. It checks the unavailable error off x86-64.
- `vos/tests/agent_runtime_pvm.rs`: **every AgentRuntime execution in the file now runs on both
  engines**, and exit, counter, registers, gas and output bytes are compared. That covers the
  bundled runtime creating, installing, invoking, retrying, acknowledging, yielding and resuming
  actors. Two new tests:
  - `faulting_and_spinning_actors_fail_identically_on_both_engines`: an actor page fault, and
    an actor out of gas at a one-instruction block at six budgets;
  - `agent_executions_match_across_engines_at_sampled_gas_budgets`: 75 budgets across a yielding
    invocation.
- `vos` `local_journal_driver`: `runtime_preparation_cache_is_keyed_by_engine_and_engines_agree`
  and `execution_config_is_explicit_per_agent_and_refuses_unavailable_engines`.

**Suite results are not recorded for this pass.** The tests above were written and run while the branch was developed, but their final output was not kept. Re-run `cargo test -p vos --lib`, the PVM runtime suite and the v0.8.0 vectors on both engines on x86-64 Linux before opening this pull request, and paste the counts here.

aledgent's harness drives the runtime ABI as the Local host does (`vos-host backends`, and the
core's own host), with `F025_LOCKSTEP=1`, which runs every execution on both engines:

| Run | Executions | Result |
|---|---|---|
| `state-blob` (66 KB actor) | 63: create, install, 30 invocations and acknowledgements | identical on both engines |
| same, one more Invoke, swept over budgets | 276 outer and 147 actor gas budgets | identical: 256 halts, 165 out-of-gas, 2 out-of-gas at load |
| `kernel-probe` (291 KB actor) | 63, plus the same budget sweep | identical: 254 halts, 167 out-of-gas, 2 out-of-gas at load |
| aledgent's core (1.97 MB actor), reservations | 75 | identical, gas 33.7 M to 37.3 M per execution |

Identical means exit, instruction counter, registers, gas and output bytes, on every execution.

## Under emulation

One reservation in aledgent's core (1.97 MB actor), 26 samples per round, Rosetta 2:

| Engine | p50 | Invoke gas |
|---|---:|---:|
| Interpreter | ≈ 5.1 s | 1,488 M |
| Recompiler | ≈ 0.48 s | 1,488 M |

About **10.8× less wall time at identical gas.** A second round of each was disturbed by host load and its tails are not usable. Only the ratio carries over to real hardware.

## Security: native actor code in the host process

The interpreter confines a guest by construction: a bug gives a wrong result. The recompiler
turns actor bytes into machine code in the host process, and confinement then rests on the
code generator:

- R15-relative addressing into a reserved 4 GiB mapping, with a guard page after it;
- `mprotect` permissions for every page;
- the SIGSEGV handler that maps faults inside generated code back to guest page faults;
- W^X (code is mapped read-write while it is emitted, then read-execute).

A code-generation bug can therefore become a host compromise, not a wrong answer. The trust
boundary changes, and three things follow:

1. **Opt-in per Agent is the right default.** An operator selects the recompiler for Agents
   whose actors it trusts or has reviewed. aledgent's core is aledgent's own code in aledgent's
   own host.
2. **Untrusted actors belong in a sandbox process.** PolkaVM does this: guest code runs in a
   separate, minimal process with seccomp filtering, its own address space and no host file
   descriptors, and the host talks to it over shared memory. For VOS that is an executor worker
   process per untrusted Agent, or per pool, driven over the same `RefineContext` boundary. It
   adds a context switch per host call (the `machine` and `invoke` calls). It is later work,
   and the host would pick it per Agent through the same `AgentExecutionConfig`.
3. **The generator needs fuzzing as well as vectors.** VOS has a differential fuzz harness for
   the kernel path (`pvm_vectors.rs` mentions its deterministic slice). Pointing it at Refine
   inner machines, with the lockstep in `refine_backends.rs` as the oracle, is the natural
   follow-up.

There is also an operational change. The handler is process-wide, and the alternate signal
stack is installed once, on the first thread that installs the handler. A host that runs
recompiled Agents on many worker threads should install an alternate stack per worker. This
branch does not change that.

## ARM: later work

The recompiler emits x86-64 only. On aarch64 (Apple silicon, Graviton, Ampere) `Recompiler` is
unavailable and the interpreter runs as today. An AArch64 backend is a new code generator over
the same gas tables, and gas stays unchanged by construction. The differential tests in this
branch would be its acceptance suite unchanged, since they compare engines, not architectures.
Until it exists, a mixed fleet is still deterministic. Each node picks its own engine, and all
produce the same transitions.

## Ahead-of-time compilation at install: design, not in this branch

This branch compiles once per runtime preparation: the outer runtime when the executor first
prepares it, and each actor the first time a context of that preparation creates it (then
reused by exact bytes). The first invocation after start pays both compilations. The next step
is compiling at install and caching the native code:

- **When.** At actor install admission, and when a runtime package is admitted: the moments the
  host first holds the verified program bytes. Compilation is host work outside the gas schedule
  and never changes a result, so it may happen whenever the host likes, before the first
  invocation.
- **Key.** `(ProgramId, compiler identity, target)`. The compiler identity is the `vos-pvm`
  version plus a code-generator ABI constant, bumped whenever codegen changes. The target is
  `x86_64-linux` plus the CPU features the generator uses. The ISA mode and memory latency are
  fixed by the standard profile, but they belong in the key so the JAR profile cannot collide.
- **What is stored.** The native bytes, the dispatch table, the fault-resume offsets, the
  per-PC gas-block starts, the block costs, the trap table, the block starts and the exit label
  offset: exactly `CompiledCode`. `native_code_is_deterministic_and_position_independent` shows
  that the bytes are the same on every compilation and contain no absolute address. A cached
  entry can be mapped at any address without relocation.
- **Trust.** Loading cached native code means executing bytes from disk, so the cache has the
  same trust as the journal store: a host-owned directory, never replicated. Each node compiles
  its own. An entry is checked against its key and a MAC under a host key, or recompiled and
  compared in the background. It is never shipped with a package.
- **Why not here.** It needs a serialisation of `CompiledCode`, a cache directory in the host's
  layout, a version constant the maintainer owns, and eviction. None of it changes execution, and
  the in-memory compile-once already removes compilation from every invocation but the first.

Compiling is not free, which is why it belongs at install, not per invocation (median, under emulation):

| Program | Interpreter predecode | Recompiler compile | Native code |
|---|---:|---:|---:|
| AgentRuntime (1.07 MB) | 90 ms | 100 ms | — |
| `kernel-probe` actor (291 KB) | 23 ms | 30 ms | 4.5 MB |
| aledgent's core (1.97 MB) | 113 ms | 136 ms | 27.8 MB |

With 0004's preparation cache the cost is paid once per program, not per call.

## With 0004's option C: the program by hash, through standard calls

0004 found that most of what an invocation pays for a large actor is three BLAKE2b passes over
the program: about 880 M of the core's 1.14 G gas. Option C takes the program out of the work,
and the runtime gets it by hash. The recompiler makes those passes cheaper in time, but not in
gas.

Option C and this change fit together, and C fits VOS's own rule if the lookup is Refine's
standard one:

- **Within the no-new-host-instructions rule.** Gray Paper Refine obtains its code by hash
  through Λ, outside the VM (B.5). A guest reads preimages with the standard `historical_lookup`
  and `fetch` host calls (B.6). If the AgentRuntime fetches its actor's program the same way,
  it uses standard Refine calls. It adds no VOS-specific host instruction, and
  `pvm/SPECIFICATION.md`'s boundary holds. The runtime's static allowlist
  (`REFINE_HOST_CALL_ALLOWLIST`, today calls 9–14) grows by standard calls only.
- **The executor resolves the hash natively,** from the Agent's catalog, as Λ resolves from a
  service's preimages. It hands the runtime bytes it has verified once. The runtime no longer
  hashes the program to authenticate it (passes 2 and 3), and the public IO commits to the
  reference instead of the bytes (pass 1). The transition proof has to constrain the lookup, as
  a JAM auditor re-executes Refine against the same lookup anchor.
- **Native code is keyed by that hash.** With the program looked up by hash, the executor
  already knows the program's identity before the runtime asks for a machine. The
  ahead-of-time cache above can be keyed by it directly, not by comparing 2 MB of bytes.
- **JAM service portability.** An AgentRuntime that fetches its code by hash and runs it in
  inner machines is shaped like a JAM service's Refine. It moves toward running unchanged as
  one.

Whether to take C is @olanod's call. It changes the runtime's public IO definition and the proof
statement. This branch does not depend on it, and C does not depend on this branch. Their
effects multiply.

## Expected gain: gas does not move, wall time does

**Gas is unchanged, by definition and by measurement.** Every comparison above is equal to the
unit. So the budget arithmetic of aledgent's `F-002` and `F-005` (gas per reservation) is the
same on either engine. Only the time per unit of gas changes.

**Wall time is gas divided by the engine's speed.** On the reference host the interpreter runs
the Agent path at about 500 M gas/s (0004, the core's Invoke). What the recompiler reaches on
a real x86-64 host is not measured here: emulation times are not results. What bounds it:

- The gas model is designed as a cycle estimate for a modern out-of-order core (§A.9). Code the
  model prices close to its native cost runs at roughly one gas per cycle natively: 3–4 G gas/s
  at 3–4 GHz.
- Code the model prices above its native cost runs faster. Every load and store costs 25 gas
  (A.64) but hits L1 in about 4 cycles. The runtime's BLAKE2b is exactly that code: 32 working
  words spilling to memory on 16 registers (0004).

Gas stays at about 1.14 G per reservation with 0001–0004; only wall time moves. Under emulation the recompiler cuts it about tenfold, so on real hardware a reservation would drop from 2.36 s toward roughly 0.2–0.3 s. About 77 % of that remaining gas is still the program hashing (0004). With 0004's option C removing it as well, what is left is the core's own work and runtime overhead, about 0.26 G gas, compiled. Whether that lands under 50 ms has to be measured on x86-64 hardware; these figures only bound it.

## For the maintainer to decide

1. **Where the host setting lives.** `AgentExecutionConfig` exists on `AgentHost`. It still
   needs a node or daemon configuration key and a `vosx` flag. The default must stay the
   interpreter.
2. **Whether the funded-block fix belongs in the recompiler generally.** Refine needs it for
   re-entry after halt or panic. The branch applies it to the standard profile only, the one
   Refine uses. Whether the JAR/kernel profile needs the same is not tested here.
3. **Untrusted actors.** In-process native execution only for trusted Agents, with a sandbox
   worker process as the path for the rest. Or not recompiling untrusted actors at all until it
   exists.
4. **Ahead-of-time compilation at install**, as designed above: the compiler-identity constant
   and the cache's place in the host layout.
5. **0004's option C through `historical_lookup` / `fetch`**, which this change makes worth
   more.
6. **Order against 0004.** This branch and 0004 both touch `refine.rs`, `refine_host.rs` and
   `inner.rs` (nine conflicting hunks under `git merge-tree`). Both add a per-preparation cache of
   inner programs: 0004's predecoded interpreter programs, this branch's native programs. They
   should become one cache holding either. Whichever merges second is rebased onto the first;
   aledgent will do that rebase.

