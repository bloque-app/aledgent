<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Load actor programs in place and reuse their preparation

Every Invoke carries the actor's program, and the AgentRuntime spends about 630 gas per program byte
on it before the actor runs. The host also predecodes the whole program again on every invocation. This
PR removes the guest work that only re-derives bytes the runtime already has, and keeps each inner
program's predecode across invocations. Runtime gas drops by about 168 gas per program byte (27 %).
Results, actor gas and transitions are byte-identical.

## Problem

A gas-per-counter profile of one Invoke of the runtime built from `2bb78760`, taken as the difference
between a 66 469-byte and a 291 219-byte actor:

| Where | Gas per program byte |
|---|---:|
| BLAKE2b (`compress1_loop`): three passes over the program | 447 |
| `ActorMachine::load_at` re-packing the bitmask (`compact_code`) | 76 |
| `parse_compact_code_blob` expanding the bitmask to a byte per code byte | 71 |
| `memcpy`: decode, the resolver's clone, parsed sections, re-encoded blob | 35 |
| **Total** | **≈ 630** |

Two rows are pure waste. `ActorMachine::load_at` (`vos/src/agent/machine.rs:67`) parses the program in
full (`:73`), which copies every section and expands the packed bitmask one byte per code byte
(`pvm/program/src/lib.rs:257`, `:266-271`); it then packs the bitmask back into exactly the bytes it came
from (`compact_code`, `machine.rs:82`, `lib.rs:139-141`) and hands the result to `machine`, which parses
it again natively. Before that, `resolve_clean_invocation` clones the program out of the work
(`vos/src/agent/standard.rs:2964`).

Outside the gas schedule, the host builds a fresh `InnerMachines` for every context
(`pvm/runtime/src/refine_host.rs:100`), so every `machine` call predecodes the whole program again
(`inner.rs:177-196`, `Interpreter::with_memory_and_mode`): 3.5 ms for a 66 KB actor, 96 ms for a 2 MB
one.

For our ledger's core actor (1 968 379 bytes), this is most of a reservation's 1 487 M gas.

## Change

One commit, 6 files, +815 −107; about 370 lines are tests.

- **`pvm/program/src/lib.rs`: validate in place.** `StandardProgramRef<'a>` (`:171`) with `parse`
  (`:182`) validates a standard program without copying sections or expanding the bitmask; its
  `code_blob()` (`:197`) is the program's own embedded compact code blob, and `is_instruction_start`
  answers from the packed bitmask. Owned and borrowed parsers share one acceptance rule,
  `validate_compact_code_blob` (`:319`, with `CompactCodeLayout`); `parse_compact_code_blob` now
  validates first and then materialises. `standard_layout` is factored out of `StandardProgram::layout`.
- **`vos/src/agent/machine.rs`: hand that blob to `machine`.** `load_at` parses with
  `StandardProgramRef`, passes `code_blob()` to `machine`, checks a restored snapshot's counter against
  the packed bitmask, and initialises regions from the borrowed sections with the layout computed once.
- **`vos/src/agent/standard.rs`: borrow on the execution path.** A private
  `resolve_clean_invocation_borrowed` returns the program as a slice of the work;
  `resolve_clean_invocation_for_execution` uses it. `resolve_clean_invocation` still returns an owned
  copy to its other callers.
- **`pvm/runtime/src/inner.rs`, `refine.rs`, `refine_host.rs`: cache inner program preparation.**
  `PreparedInnerProgram` (`inner.rs:178`) holds one inner program, validated (`deblob` at counter 0)
  and predecoded. `InnerProgramCache` (`:223`, `std` only) keeps four, most recently used first, keyed
  by exact bytes. `PreparedProgram` carries one (`refine.rs:116`, accessor `inner_programs()`), and
  `RefineContext::load_prepared` hands it to `InnerMachines::with_program_cache`
  (`refine_host.rs:102-106`). `InnerMachines::create` admits the requested entry against a cached
  preparation exactly as `deblob(blob, entry)` would. Invalid blobs are never cached; they take the cold
  path and its refusal. The cache holds no results, memory or per-invocation state.

**Public API.** Additive only: `StandardProgramRef`, `CompactCodeLayout`, `validate_compact_code_blob`
in `vos-pvm-program`; `PreparedInnerProgram`, `InnerProgramCache`, `InnerMachines::with_program_cache`,
`PreparedProgram::inner_programs` in `vos-pvm`. Nothing public changes in `vos`. No wire, ABI or gas
schedule change.

## Design choices and alternatives

- **Why the swap is exact.** `StandardProgramRef::parse(b)` succeeds exactly when
  `parse_standard_program(b)` does, and `code_blob()` equals `compact_code()` byte for byte: canonical
  naturals, minimal entry width, exact length and zero unused bitmask bits make the encoding injective,
  so re-encoding a valid blob is the identity. `machine` receives the same bytes, builds the same
  machine and refuses the same programs.
- **Why cold equals warm.** The cold constructor is the same predecode followed by the same
  `from_predecoded` the cache uses. The cache only moves native work out of the invocation; the
  predecode it saves was never metered.
- **Cache per runtime preparation, bounded by count.** It lives where the preparation already lives,
  so it is dropped with it. A byte budget would bound memory more tightly for Agents that run several
  large actors (question 4).
- **The remaining 462 gas per byte is hashing, and this PR leaves it.** Three BLAKE2b passes, about
  149 gas per byte each: the public-IO hash of the work (`runtime_transition_public_io`,
  `vos-agent-sdk/src/runtime.rs:1110-1124`), the `BlobRef` check (`RuntimeBlob::validate`,
  `runtime.rs:86-87`, domain `vos/blob`) and the ProgramId (`ProgramId::of_pvm`, `standard.rs:2847`,
  domain `vos/program/service`). The domains differ, so they cannot share compressions. Removing any of
  them changes the proof's public IO, the stored state, or the runtime's outer interface, which
  `vos/src/crypto/blake2b.rs:44-46` restricts: "The infrastructure runtime deliberately has no private
  outer-host ABI. Hashing here is ordinary deterministic guest computation so its only outer host calls
  are standard PVM machine management (9..=14)." Options are in the questions.

## Compatibility

- **Execution:** identical. Runtime gas is a function of the work alone, and smaller; actor gas is
  unchanged to the unit. We ran one Agent through the base and the new runtime in lockstep (create,
  install, a directory inspection and 30 Invoke and Acknowledge pairs, for two actors): all 63
  `RuntimeTransition`s per actor byte-identical.
- **Proofs:** the runtime's outer calls are still the standard machine calls 9–14, `machine` is
  unchanged, and the public IO commits to the same bytes.
- **Replay and artifact:** the gas saving is in the runtime guest, so it only takes effect once
  `vosx/blobs/agent_runtime.pvm` is rebuilt from this source (not done here). The rebuilt runtime has a
  new ProgramId; an Agent replays under the runtime its descriptor pins, and journals are unaffected.
  The host cache takes effect at once for hosts that load through `load_prepared`.
- **Cold paths:** only `local_journal_driver.rs`'s `RuntimePreparationCache` loads through
  `load_prepared`. These still load cold and get no host cache: `runtime_pvm.rs:61` (used by
  `driver.rs` and `private_host.rs`), the experimental state-block path (`state_block_pvm.rs`), and proof
  generation (`pvm/proof/src/refine.rs:1059`), which is deliberate.
- **Memory:** the runtime heap no longer holds the clone, the parsed sections, the expanded bitmask or
  the re-encoded blob, about 3.5 times the program less at the load's peak. The host keeps at most four
  prepared inner programs per runtime preparation, each a copy of the blob plus predecoded tables
  (about 30 bytes per code byte; our estimate). `PreparedProgram` now has interior mutability, so its
  doc comment ("immutable interpreter preparation") needs a word.

## Tests

New:

- `vos-pvm-program` (`pvm/program/src/lib.rs`): `borrowed_program_is_the_owned_program_without_copies`,
  `borrowed_program_refuses_what_the_owned_parse_refuses`, and three property tests,
  `borrowed_and_owned_parses_agree_on_{valid_programs,mutated_programs,arbitrary_bytes}`: same
  acceptance, code blob equal to `compact_code`, same layout, same instruction starts.
- `vos-pvm` (`pvm/runtime/src/inner.rs`, `std`): `prepared_programs_are_indistinguishable_from_cold_loads`
  (four programs, five entries each, a miss round and a hit round, every operation compared; invalid
  blobs never cached; the 63-machine limit unchanged) and
  `inner_program_cache_is_bounded_and_keeps_the_most_recent`.
- `vos-pvm` (`refine_host.rs`): `prepared_runtime_reuses_inner_programs_without_changing_execution`:
  a runtime program that creates and invokes inner machines, cold and prepared, over two valid and one
  invalid inner program; exit, counter, registers, gas and memory equal; the cache ends holding two.
- `vos` (`vos/src/agent/machine.rs`): `load_hands_machine_the_programs_own_code_blob`.

```sh
cargo test -p vos-pvm-program --lib
cargo test -p vos-pvm --lib
cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'
```

Run on Linux aarch64, `nightly-2025-05-09`:

| Command | Result |
|---|---|
| `cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'` | 1 779 passed, 17 ignored, 7 failed; the same 7 fail on unmodified `2bb78760` |
| the same with `--ignored compiled_runtime compiled_shared_genesis` and `AGENT_RUNTIME_CANDIDATE_ELF` built from this branch | 4 passed |
| `vos` with `experimental-state-blocks`, filtered to `agent::wire`, `replay`, `state_block*`, `execution`, `actor_storage`, `machine` | 228 passed, 26 ignored |
| `cargo test -p vos-pvm --lib` | 216 passed, 2 ignored, 4 failed; the same 4 fail on `2bb78760` (they force the recompiler, x86-64 only) |
| `cargo test -p vos-pvm-program --lib` | 14 passed |

Re-checked on macOS aarch64 on 2026-09-29: `vos-pvm-program` 14 passed; the new `vos-pvm` tests pass;
`cargo check -p vos-pvm --no-default-features --lib` builds.

**Not run:** the recompiler tests (x86-64 only), the five tests that need prebuilt artifacts, and this
branch on x86-64 at all.

**Numbers.** Runtime gas per Invoke, p50 over 200 invocations, the same to the unit in two rounds;
"before" is the runtime built from `2bb78760` by the same pipeline:

| Actor | Program | Before | After |
|---|---:|---:|---:|
| `state-blob` (a small test actor) | 66 469 B | 99.54 M | 87.22 M |
| `kernel-probe`, no-op | 291 219 B | 222.69 M | 171.05 M |
| our core actor, one reservation | 1 968 379 B | 1 487.06 M | 1 135.63 M (−23.6 %) |

The core does not fit the base 1 280 KiB program limit, so both core rows ran on a tree with both program
limits raised to 2 560 KiB (see #7). Time, ms p50 / p99 on an Apple M2 Pro (aarch64 Linux VM): the no-op
Invoke plus Acknowledge 399.1 / 431.8 → 347.8 / 365.1, of which about 24 ms is the host cache alone; the
core's reservation round 2 544.8 / 2 677.0 → 2 262.5 / 2 360.3.

## Relation to the other pull requests

All five VOS pull requests (#6 to #10) are one commit each on `saga/agents` at `2bb78760`, still the tip
(checked 2026-09-29), and each applies alone. This one shares `vos/src/agent/standard.rs` with #6, in
different functions; #6 + #7 + #8 + #9 merge together cleanly under `git merge-tree`. **It conflicts
with #10** in `pvm/runtime/src/inner.rs` (five hunks), `refine.rs` and `refine_host.rs` (two each): both
add a per-preparation inner-program cache with the same shape (four entries, most recent first, exact
bytes), this one for predecoded interpreter programs and #10's for native ones. They should become one
cache with an engine-specific payload. We suggest merging this one first; we will rebase #10 onto it.
With this PR in, #7's hard program maximum can rise from 1 792 KiB to 2 560 KiB, because the runtime no
longer materialises the program several times.

## Questions for you

The remaining 462 gas per byte is the three hashing passes. For our core's reservation that is about
880 M of the 1 136 M gas left (77 %). Ways to go further, none in this PR:

1. **Commit the public IO to availability by reference** instead of bytes. The runtime verifies each
   blob against its `BlobRef` anyway. Saves one pass; changes the proof's public-IO definition.
2. **Record the program's `BlobRef` at install** and find the program by reference. Saves the ProgramId
   pass; a state and wire change (`InstallActor` carries the package reference, not the program's).
3. **Give the runtime the program by hash**, verified natively by the executor, as Refine obtains code
   by hash in the Gray Paper (§B.5–B.6, through the standard `historical_lookup` and `fetch` calls).
   Removes all three passes and about 2 MB of input per invocation. It widens the runtime's host-call
   allowlist beyond 9–14, though only with standard Refine calls, and the proof must constrain the
   lookup.
4. **Cache bound by bytes** rather than four programs, and sharing the predecoded tables through an
   `Arc` instead of cloning them per machine (a few ms per invocation for a 2 MB actor).
5. **The comment on `MAX_EXECUTION_PROGRAM_BYTES`** (`vos/src/agent/execution.rs:35-37`) justifies the
   limit by the materialisation this PR removes. Update it here, or with #7's follow-up?
6. **Other loaders:** should `runtime_pvm.rs` (and so `driver.rs` and `private_host.rs`) also load
   through preparations?
7. Repinning the bundled runtime after review.

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent, deciding each
balance-affecting command within a completion budget of tens of milliseconds. Its core actor is about
2 MB, because it carries a confidential kernel and a sparse-Merkle verifier. At that size the fixed cost
of loading the program on every call dominates everything the ledger itself does: about 1.24 G (630 gas per byte) of our 1.49 G
gas per reservation before this PR. This PR takes out the part that is pure waste; the questions above
are about the rest.
