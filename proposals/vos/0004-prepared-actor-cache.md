<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0004 — Load the actor in place and keep its preparation across invocations

**Draft pull request description, 2026-09-24.** Not yet opened upstream.

| | |
|---|---|
| Branch | `agent/prepared-actor-cache` on aledgent's VOS fork, one commit on `saga/agents` at `2bb7876` |
| Commit | `5424ea5b` `perf(agent): load actor programs in place and reuse their preparation` |
| Size | 6 files, +815 −107 (`pvm/program/src/lib.rs`, `pvm/runtime/src/inner.rs`, `refine.rs`, `refine_host.rs`, `vos/src/agent/machine.rs`, `standard.rs`); about 400 lines are tests |
| Independent of | 0001, 0002 and 0003; applies alone to `saga/agents`. It shares `vos/src/agent/standard.rs` with 0001, in different functions; all four branches merge together without conflict |
| aledgent task | `T-025-03` (issue #66), `DEC-16` |

**Read this first.** The task was to stop paying for the actor program on every invocation. It
does part of that. The runtime charged about **630 gas per byte of actor program** on every Invoke.
This branch removes about **168 gas per byte** of it, 27 %, plus the host's native predecode of
the actor. Gas and results are identical cold and warm, and the transitions are byte-identical.
The remaining **462 gas per byte is three BLAKE2b passes over the program**. The runtime cannot
avoid them without either trusting the host with bytes it has not hashed, or leaving the
standard machine calls. Both are design decisions for the maintainer; they are set out at the
end. For aledgent's 1.97 MB core that is 1.49 G → 1.14 G gas per reservation, 2.68 s → 2.36 s at
p99. It is a step, not the fix.

Everything below the line is the pull request text.

---

## Problem

Every Invoke carries the actor's program as availability, and the runtime works through all of it
before the actor runs. A Local Agent pays about 630 runtime gas per byte of program on every
invocation: 42 M gas for a 66 KB actor, 1.3 G for a 2 MB one. The host adds native work
outside the gas schedule: a fresh inner machine predecodes the whole program every time, 3.5 ms
for the 66 KB actor, 18 ms for a 291 KB one and 96 ms for a 2 MB one on the reference host
(`prep-bench`, p50).

Where the gas goes, from a gas-per-counter profile of one Invoke of the standard runtime built from
`2bb7876` (`RefineContext::run_observed`, counters resolved to functions with
`resolve_observed_standard_program_pcs` and the runtime ELF's symbols), taken as the difference
between a 66 469 B and a 291 219 B actor:

| Where | Gas per program byte |
|---|---:|
| `blake2b_simd::portable::compress1_loop`: three passes over the program | 447 |
| `ActorMachine::load_at` (inlined `compact_code`: re-packing the bitmask) | 76 |
| `vos_pvm_program::parse_compact_code_blob` (expanding the bitmask to a byte per code byte) | 71 |
| `memcpy` (decode, the resolver's clone, the parsed sections, the re-encoded blob) | 35 |
| **Total** | **≈ 630** |

By phase: 488 gas per byte before the runtime's `machine` call (decode, the two content hashes,
parse, re-encode), and 142 per byte after the actor returns (the public-IO hash of the whole work,
`runtime_transition_public_io`).

The three BLAKE2b passes cost about 149 gas per byte each: about 19 000 gas per 128-byte block.
The runtime is built for `riscv64em` (16 registers), BLAKE2b's 32 working words spill on
every round, and the gas model charges 25 cycles per memory access.

Two of the four rows are pure waste. `parse_standard_program` expands the packed bitmask to one byte
per code byte, and `compact_code` packs it back into exactly the bytes it came from. The runtime
then hands that blob to `machine`, which parses it again natively.

## Design

Three changes. None of them alters what any execution computes.

**1. Validate in place and pass the program's own code blob to `machine`** (`pvm/program`,
`vos/src/agent/machine.rs`). `StandardProgramRef::parse` validates a standard program without
copying any section or expanding the bitmask, and borrows the sections from the program bytes.
Its `code_blob()` is the program's embedded compact code blob. `ActorMachine::load_at` hands
that to `machine` and initialises the regions from the borrowed sections. A restored snapshot's
counter is checked against the packed bitmask.

The owned and borrowed parsers now share one acceptance rule, `validate_compact_code_blob`, and
differ only in materialising. Two facts make the swap exact:

- `StandardProgramRef::parse(b)` succeeds exactly when `parse_standard_program(b)` does.
- Its `code_blob()` equals `parse_standard_program(b)?.compact_code()` byte for byte. Canonical
  naturals, minimal entry width, exact length and zero unused bitmask bits make the encoding
  injective, so re-encoding a valid blob returns the same bytes.

So `machine` receives the same bytes, creates the same machine and refuses the same programs.

**2. Borrow the program on the execution path** (`vos/src/agent/standard.rs`).
`resolve_clean_invocation_for_execution` keeps a borrow of the program in the work instead of
cloning it. `resolve_clean_invocation` still returns an owned copy to its other callers.

**3. Keep each inner program's preparation on the runtime's preparation** (`pvm/runtime`).
`PreparedInnerProgram` holds one inner program, validated (`deblob` at counter 0) and predecoded
for the interpreter. `InnerProgramCache` keeps four of them, most recently used first, selected
by exact bytes. `PreparedProgram` now carries one, so every `RefineContext::load_prepared` from
the same runtime preparation reuses it. The Local journal driver's `RuntimePreparationCache`,
aledgent's harness and F-005's host all load through it. `InnerMachines::create` admits the
requested entry against a cached preparation exactly as `deblob(blob, entry)` would.
`validate_code_blob(program, pc)` is a scan from counter zero, which already requires entry
zero, plus the entry's own check. A machine built from the preparation is the machine built
cold, because the cold constructor, `with_memory_and_mode`, is that same predecode followed by
the same `from_predecoded`. Invalid blobs are never cached; they take the cold path and its
refusal. The cache is host-local and holds no results, memory or per-invocation state.

## Guarantees preserved

- **Same inputs, same results, same gas, on every node, cold or warm.** The runtime's gas is a
  function of the work alone, and now smaller. The host cache changes native time only. Tests
  run each inner program cold, then as a first prepared use (a miss), then as later uses
  (hits). Machines, exits, gas, registers, memory, counters and refusals are equal on every
  path. **The runtime's transitions are byte-identical to the base runtime's**: aledgent's
  harness ran one Agent through both runtimes in lockstep and compared every
  `RuntimeTransition` (`vos-host compare`, below). The actor's own gas is equal to the unit in
  every measurement. The outer gas is not part of the transition.
- **Metering.** The removed guest work re-derived bytes the runtime already had. Validating and
  decoding the program for the machine still happens under the standard `machine` charge
  (1 862 + 112 per KiB) the runtime always paid, natively, as before. The predecode the cache
  saves was never metered; it was native on every invocation and is now native once per runtime
  preparation.
- **Replay.** A new runtime program has a new ProgramId, and an Agent replays under the runtime
  its descriptor pins, so gas figures from the old runtime are never compared against the new
  one. Journals themselves are unaffected: the transitions are identical.
- **Transition proofs.** The runtime's only outer calls are still the standard machine calls
  9–14 (`validate_refine_host_calls`), `machine`'s semantics are unchanged, and the public IO
  commits to the same input and output bytes. The proof constrains the same machine with less
  guest code around it.
- **Memory.** The runtime heap no longer holds the program's clone, the parsed sections, the
  expanded bitmask or the re-encoded blob: about 3.5 times the program less at the load's peak.
  0003's larger programs depend on that headroom less, not more. The host keeps at most four
  prepared programs per runtime preparation, about 30 bytes of predecoded tables per code byte
  (about 40 MB for a 1.4 MB code segment).

## Tests

In VOS's own suite:

- `vos-pvm-program`: `borrowed_program_is_the_owned_program_without_copies`,
  `borrowed_program_refuses_what_the_owned_parse_refuses`, and three property tests
  (`borrowed_and_owned_parses_agree_on_{valid,mutated,arbitrary}…`). On valid, byte-edited and
  truncated programs and on arbitrary bytes they check four things: the same acceptance, the
  code blob equal to `compact_code`, the same layout for two argument sizes, and the same
  instruction starts.
- `vos/src/agent/machine.rs`: `load_hands_machine_the_programs_own_code_blob`. The machine the
  loader creates holds exactly the re-encoded bytes. A trailing byte is still refused. A
  snapshot at a non-instruction counter is still refused.
- `vos-pvm` `inner`: `prepared_programs_are_indistinguishable_from_cold_loads` (four programs, five
  entries each, a miss round and a hit round, every observable operation compared; invalid blobs
  never cached; the 63-machine limit unchanged) and
  `inner_program_cache_is_bounded_and_keeps_the_most_recent`.
- `vos-pvm` `refine_host`: `prepared_runtime_reuses_inner_programs_without_changing_execution`.
  A runtime program that creates and invokes an inner machine runs cold and prepared, three
  rounds over two valid inner programs and one invalid one. Exit, counter, registers, gas and
  memory are equal every time, and exactly the two valid programs are prepared once each.

Suites run (Linux `aarch64` container, `nightly-2025-05-09`):

| Command | Result |
|---|---|
| `cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'` | 1 779 passed, 17 ignored, **7 failed — the same 7 fail on unmodified `2bb7876`** (two need the `x86_64` recompiler, five need prebuilt artifacts: `package_admission.rs:754`, `clean_bootstrap.rs:10723`/`:10760`, `driver.rs:5934`) |
| the same, `--ignored compiled_runtime compiled_shared_genesis`, with `AGENT_RUNTIME_CANDIDATE_ELF` built from this branch | 4 passed |
| `cargo test -p vos --lib --features '… experimental-state-blocks'` on `agent::wire`, `agent::replay`, `agent::state_block*`, `agent::execution`, `agent::actor_storage`, `agent::machine` | 228 passed, 26 ignored |
| `cargo test -p vos-pvm --lib` | 216 passed, 2 ignored, 4 failed — the same 4 fail on `2bb7876` (`ForceRecompiler`, `x86_64` only) |
| `cargo test -p vos-pvm-program --lib` | 14 passed |

aledgent's harness also ran one Agent through both runtimes in lockstep (`vos-host compare`, with
`F025_COMPARE_RUNTIME`): Create, Install, a directory inspection and 30 Invoke and Acknowledge
pairs for each of `state-blob` and `kernel-probe`, 63 executions each. **Every transition was
byte-identical.**

## Before and after

aledgent's harness (`spikes/vos-host`, the runtime ABI as `local_journal_driver.rs` drives it).
"Before" is the standard runtime built from `2bb7876` with the same pipeline as "after", and the
harness built against `2bb7876`. "After" is both built from this branch, so the harness has the
host cache. Two interleaved rounds of 200 invocations each, one run at a time.

**Runtime gas per Invoke** (p50; the same in both rounds to the unit; the actor's own gas is
identical before and after):

| Actor | Program | Before | After | Change |
|---|---:|---:|---:|---:|
| `state-blob`, S4 at 64 KiB | 66 469 B | 99.54 M | 87.22 M | −12.32 M |
| `kernel-probe`, no-op | 291 219 B | 222.69 M | 171.05 M | −51.64 M |
| `kernel-probe`, reserve with a new holding position | 291 219 B | 268.60 M | 217.00 M | −51.60 M |
| aledgent's core, one reservation (F-005) | 1 968 379 B | 1 487.06 M | 1 135.63 M | −351.42 M (−23.6 %) |

The slope across the three actors goes from 630 to 462 gas per program byte. What is left is
the three BLAKE2b passes (447) and one decode copy (16).

**Time** (ms, p50 / p99, 400 samples; the core 52):

| Measurement | Before | After |
|---|---:|---:|
| `state-blob` Invoke + Acknowledge at the caller | 186.4 / 208.0 | 169.8 / 185.0 |
| `kernel-probe` no-op, Invoke + Acknowledge at the caller | 399.1 / 431.8 | 347.8 / 365.1 |
| `kernel-probe` reserve, Invoke + Acknowledge at the caller | 473.5 / 501.8 | 422.0 / 446.6 |
| The core's reservation, the driver's whole round | 2 544.8 / 2 677.0 | 2 262.5 / 2 360.3 |

Of the no-op's 51 ms at p50, about 24 ms is the host cache alone: the same "after" runtime under
the "before" harness, with no host cache, takes 346.0 ms against 322.8 ms. The rest is the
guest work removed. For the 2 MB core the predecode the cache saves is about 96 ms
(`prep-bench`). Less time is saved than gas: the removed loops ran faster than their gas
suggests.

## For the maintainer to decide: the three hashing passes

What is left per byte of program is hashing:

1. **The public-IO work hash** (`runtime_transition_public_io`): about 149 gas per byte.
2. **The availability reference check**: `RuntimeBlob::validate` hashes each blob against its
   `BlobRef` (`vos/blob` domain). About 149 gas per byte.
3. **The ProgramId**: `resolve_clean_invocation` computes `ProgramId::of_pvm` of every blob that
   could be the program (`vos/program/service` domain). About 149 gas per byte.

The domains differ, so no two passes can share compressions. Five ways to go further, each
beyond this branch:

- **A. Commit the public IO to availability by reference.** The runtime verifies each blob's
  bytes against its `BlobRef` anyway. Hashing the work with each blob replaced by its reference
  binds the same bytes and saves pass 1. It changes the proof's public-IO definition.
- **B. Identify the program by its verified reference.** If the actor directory recorded the
  program's `BlobRef` at install (it records only the ProgramId today, and `InstallActor` does
  not carry the program), invocation could find the program by reference equality and skip
  pass 3. This is a state and wire change.
- **C. Take the program out of the work.** Give the runtime the program by hash, verified by the
  executor natively, as JAM's `historical_lookup` supplies preimages to refine. This removes all
  three passes and about 2 MB of input per invocation. It needs an outer call beyond the
  standard machine calls, which the runtime's design rules out today
  (`vos/src/crypto/blake2b.rs:44-46`). A transition proof would also have to constrain the
  lookup.
- **D. A BLAKE2b tuned to this gas model.** The portable implementation spills heavily on 16
  registers at 25 cycles per access. A register-aware version could be several times cheaper;
  not measured.
- **E. Compiled Agent execution** (aledgent `T-025-06`): everything interpreted gets cheaper,
  this included.

For the core's reservation, A and B together would remove about 590 M gas of the remaining
1.14 G; C about 880 M.

Smaller points:

- **Cache bounds.** Four programs per runtime preparation, by count. A byte budget would bound
  memory more tightly for Agents that run several large actors.
- **The interpreter tables are cloned per machine.** `from_predecoded` takes the tables by value,
  for the outer runtime already and now for cached actors: a few milliseconds per invocation
  for a 2 MB actor. Sharing them through an `Arc` would remove that.
- **The experimental state-block path** loads the runtime cold on every execution
  (`state_block_pvm.rs`, `RefineContext::load`) and gets neither cache. That is aledgent's
  `T-025-04`.
- **Repinning the bundled runtime** after review.
