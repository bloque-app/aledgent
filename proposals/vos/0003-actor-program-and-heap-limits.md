<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0003 — Let an actor declare its program, heap and stack, within the owner's ceilings

**Draft pull request description, 2026-09-24.** Not yet opened upstream. Found by `F-005` (`notes/MEASUREMENTS-F005.md` §4); this is the final form of the
draft that `F-005`'s branch carries.

| | |
|---|---|
| Branch | `agent/actor-declared-limits` on aledgent's VOS fork, one commit on `saga/agents` at `2bb7876` |
| Commit | `491f4306` `feat(agent): let actors declare program, heap and stack within owner ceilings` |
| Size | 14 files, +677 −49 (`vos-agent-sdk`: `package.rs`, `runtime.rs`; `vos`: `package_admission.rs`, `execution.rs`, `abi/pvm/alloc.rs`, `Cargo.toml`, `agent/wire.rs` tests and four fixture literals; `pvm/compiler/src/linker.rs`; `vosx`: `build.rs`, `space/local_install.rs`); about 330 lines are tests |
| Independent of | 0001, 0002 and 0004; applies alone to `saga/agents`. It shares `vos-agent-sdk/src/runtime.rs` and `vos/src/agent/wire.rs` with 0001, and `vos/Cargo.toml` and `vos/src/agent/execution.rs` with 0002, in different places; all four branches merge together without conflict |
| aledgent task | `T-025-09` (issue #66), `DEC-16`; unblocks `F-005` |

**Read this first.** The draft asked for a declaration that "enters the program identity and the
gas budget" and a heap arena sized from it. The code shows that most of this already exists.
The package signs the program's length (`manifest.program.len`). The program's own header
carries its heap pages and stack, so both are already part of the ProgramId. What was missing:

- a ceiling above the fixed 1 280 KiB;
- a way for an actor to choose its heap, which was a compile-time constant in `vos` (256 KiB);
- a stack size that does not cost zero padding in the program.

This branch adds those. The owner's ceiling is applied where a new install is admitted, not
enforced by the runtime from the Agent's state. That would need a runtime ABI change: a new
field in the resource policy or the descriptor, neither of which has a versioning seam. The
alternative is written up at the end as a question.

**One measured finding set the hard maximum.** The draft used 2 560 KiB. On the released runtime
a program that size **exhausts the runtime's 32 MiB arena**: the runtime guest panics once Agent
state nears the 4 MiB ceiling. A 2 048 KiB program fails the same way, at 4.05 MB of state. The
hard maximum here is therefore **1 792 KiB**, the largest size measured to run at the state
ceiling. The runtime materialises the program several times while loading it, and `0004` removes
those copies: with `0004`, 2 560 KiB runs at the state ceiling. Raising the constant is a
one-line follow-up once `0004` is in. aledgent's core fits 1 792 KiB with its stack declared
(1 771 777 B).

Everything below the line is the pull request text.

---

## Problem

Three fixed sizes stop a real application actor on the Agent path.

1. **Program size.** The runtime resolves an actor program only up to `MAX_EXECUTION_PROGRAM_BYTES`
   (1 280 KiB; `standard.rs`, `resolve_clean_invocation`). The SDK bounds a work item's
   availability by `MAX_RUNTIME_PROGRAM_BYTES` (1 280 KiB). A larger actor installs, because
   clean package admission never checks the program's size, but every invocation is then
   refused as `InvalidAvailability`. aledgent's core is a double-entry library with a
   confidential kernel and a sparse-Merkle verifier. It links to 1 968 379 B with
   `opt-level = "z"`, LTO and every size flag, and its reservation path alone to 1 453 878 B.
2. **Heap.** An application actor's allocator arena is `HEAP_SIZE` in `vos/src/abi/pvm/alloc.rs`,
   256 KiB, fixed at compile time. Past it the actor panics. Infrastructure guests get 8 or 32 MiB
   behind features; application actors have no choice.
3. **Stack.** `link_elf_spi` takes the stack size from the lowest read-only address. The scaffold's
   linker script puts read-only data at `0x10000`, a 64 KiB stack. An actor that needs more moves
   its read-only data up, `0x40000` for 256 KiB. `link_elf_spi` then pads the read-only blob with
   192 KiB of zeros, which travel in every invocation.

## Design

**The declaration.** An actor package may carry `ActorResources { program_bytes, heap_bytes,
stack_bytes }` (`vos-agent-sdk/src/package.rs`) under a new actor manifest tag, 3. Tag 0, the
released format, is byte-identical when the declaration is absent. This follows the
`external_state_limits` precedent in the runtime manifest (tag 1 against tag 2). Every value
restates what the signed program already fixes: its length (`manifest.program.len`, checked in
the manifest), its zero-initialised read-write pages (`heap_pages × 4 KiB`, which contain the
allocator arena) and its stack (`s`). Admission checks the declaration against the program header
(`actor_program_resources`) and refuses a mismatch as `ResourceDeclarationMismatch` on every path.
A declaration adds no execution rule. It makes a large actor a signed, visible choice of its
producer, and it is part of the package identity and deployment. `vosx actor build` declares
automatically when the program exceeds the standard size.

**The ceilings.**

- **The runtime's hard maximum** is `MAX_RUNTIME_PROGRAM_BYTES` = `MAX_EXECUTION_PROGRAM_BYTES` =
  **1 792 KiB**. It is a constant of the runtime program, so it is covered by the runtime's
  ProgramId. The runtime never resolves a larger program. Why this value is safe is in
  "Guarantees".
- **The standard actor profile** is `STANDARD_ACTOR_PROGRAM_BYTES`, 1 280 KiB, today's limit.
- **The owner's ceilings**, `ActorResourceCeilings { max_program_bytes, max_heap_bytes,
  max_stack_bytes }` (`vos/src/agent/package_admission.rs`), apply through
  `admit_actor_package_within` when an owner submits a new install. Above the standard program
  size the package must declare; any value over a ceiling is refused as `ResourceCeiling`. The
  standard ceilings are exactly today's behaviour: 1 280 KiB of program, heap and stack as any
  header can encode. `vosx space install-local-actor --max-program-bytes <N>` is where a Local
  Agent's owner raises the program ceiling, up to the hard maximum.
- **Re-admission never applies a ceiling.** `admit_actor_package` is how hosts re-admit a package
  they already retained: replay, recovery, the journal drivers and the intent stores. It still
  decides on the package bytes alone. Its only new refusal is a tag-3 declaration that differs
  from its program, and no existing package has that tag. Replay therefore never depends on a
  host's current configuration.

**The heap.** `vos` gains `actor-heap-1m`, `actor-heap-4m` and `actor-heap-16m`; the largest
selected wins, and none selected keeps 256 KiB. The arena is `.bss`, so the choice lands in the
program's heap pages, and so in the ProgramId and the declaration.

**The stack.** A guest may define `__vos_stack_size` in its linker script, in whole pages below
the `E3` width. `link_elf_spi` then uses it for the program's `s`, and read-only data can sit at
the standard base with no padding (`spi_stack_size`). Without the symbol nothing changes, and the
`JAR` manifest container is unaffected.

## Guarantees preserved

- **Determinism and replay.** The runtime's only new behaviour is a larger constant: the same
  work gives the same result on every node. Admission for install is policy before the Install
  exists. Re-admission is a pure function of the package bytes and, for every package that
  exists today, decides exactly as before.
- **Identity.** Declared sizes are in the signed manifest, so they are in the package reference,
  the deployment identity and the producer's signature (tested). The heap and stack they restate
  are in the program header, so they are in the ProgramId.
- **The runtime's memory.** Every Invoke decodes the whole work, program and state, into the
  runtime's 32 MiB arena. The runtime then clones the program, parses it into sections with a
  byte-per-code-byte bitmask, and re-encodes it, beside the state image and its runtime
  representation. The runtime guest panics on an exhausted arena: a deterministic failure, but not
  a clean refusal. So the hard maximum must fit at the state ceiling. It was measured with a real
  actor (`state-blob`, its program padded with unreachable `trap`s) filled to 4 121 295 B of
  state with 31 unacknowledged results, the harness's worst case:

  | Program | This branch alone | With `0004` |
  |---:|---|---|
  | 1 536 KiB | runs (Invoke 2.35 G gas) | runs |
  | 1 792 KiB | runs (Invoke 2.53 G gas) | runs |
  | 1 920 KiB | runs (Invoke 2.62 G gas) | runs |
  | 2 048 KiB | runtime panics at 4 054 287 B of state | runs |
  | 2 560 KiB | runtime panics at 3 853 263 B of state | runs (Invoke 2.44 G gas) |

  "With `0004`" was measured at 2 560 KiB; smaller programs need less of the arena.
- **Gas.** The declared size enters every invocation's gas through the runtime's per-byte load
  cost: about 630 gas per byte today, 462 with `0004`. At the state ceiling a 1 792 KiB actor's
  Invoke costs 2.53 G, inside the 5 G management budget. Heap pages are paid for when the
  runtime maps them (`pages`, 121 gas per page: 124 k gas for 4 MiB).
- **Yielding actors.** A continuation snapshot still captures at most 4 MiB of writable memory
  (`MAX_PORTABLE_MACHINE_MEMORY_BYTES`). An actor that declares a larger heap cannot yield. That is
  unchanged, and now visible in its declaration.

## Tests

In VOS's own suite:

- `vos-agent-sdk` `package.rs`:
  - `undeclared_actor_manifest_keeps_the_released_tag_and_bytes`: an undeclared manifest encodes
    byte-for-byte as the released tag 0.
  - `declared_actor_resources_are_signed_under_their_own_tag`: tag 3 round-trips and verifies,
    and dropping the declaration changes the deployment identity and breaks the signature.
  - `declared_actor_resources_must_restate_a_representable_program`: a wrong program length, zero
    length, partial or oversized heap pages, an oversized stack and a program above the hard
    maximum are each refused.
- `vos` `package_admission.rs`: `actor_over_the_standard_program_installs_only_when_declared_and_admitted`.
  It uses a real standard program between the standard size and the hard maximum:
  - re-admission admits it declared or not, as today;
  - the standard ceilings refuse both;
  - an owner who raises the program ceiling installs the declared one and still refuses the
    undeclared one;
  - each of the three ceilings refuses one byte or page over;
  - a ceiling above the hard maximum is invalid;
  - a declaration that differs from the header is refused on both paths;
  - a standard-size actor is unaffected.
- `vos` `agent/wire.rs`: `clean_resolver_executes_declared_programs_up_to_the_hard_maximum`. The
  runtime resolves a program at the standard size, one byte over it and at the hard maximum. It
  refuses one byte over the hard maximum.
- `pvm/compiler` `linker.rs`: `declared_stack_size_replaces_the_layout_derived_stack`. Without the
  symbol the stack is unchanged; with it, it is independent of where read-only data is linked;
  zero, a partial page and a size beyond `E3` are refused.

Suites run (Linux `aarch64` container, `nightly-2025-05-09`):

| Command | Result |
|---|---|
| `cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'` | 1 780 passed, 17 ignored, **7 failed — the same 7 fail on unmodified `2bb7876`** (two need the `x86_64` recompiler, five need prebuilt artifacts) |
| the same, `--ignored compiled_runtime compiled_shared_genesis`, with `AGENT_RUNTIME_CANDIDATE_ELF` built from this branch | 4 passed |
| `cargo test -p vos --lib --features '… experimental-state-blocks'` on `agent::wire`, `agent::replay`, `agent::state_block*`, `agent::execution`, `agent::actor_storage`, `agent::package_admission` | 228 passed, 26 ignored |
| `cargo test -p vos-agent-sdk`, with and without `experimental-state-blocks` | 251 and 250 passed |
| `cargo test -p vos-pvm-compiler --lib` | 67 passed |
| `cargo test -p vosx --bin vosx -- local_install build` | 16 passed |

## Before and after

aledgent's harness and `F-005`'s host (`notes/MEASUREMENTS-F002.md` §13).

**aledgent's core.** Its full guest is 1 968 379 B. The released runtime refuses it
(`MEASUREMENTS-F005.md` §4), so `F-005` measured it on a copy of VOS with both constants raised by
hand. On this branch:

| Build of the core | Program | Result on this branch's runtime |
|---|---:|---|
| As `F-005` builds it (read-only data at `0x40000` for a 256 KiB stack; 256 KiB heap) | 1 968 379 B | Refused: above the 1 792 KiB hard maximum, so its declaration is invalid and the package cannot be built |
| The same library with `__vos_stack_size = 0x40000`, read-only data at `0x10000`, and `actor-heap-4m` | 1 771 777 B | Declared (heap 4 MiB, stack 256 KiB) and admitted; **all 26 of `F-005`'s reservations run**. Invoke gas 1 394.9 M against 1 487.1 M for the padded build; the actor's own gas is unchanged |

The declared stack removes 196 602 B of zero padding. The larger heap needs no change to the
actor's code.

**The runtime's arena at the state ceiling**: the table under "Guarantees".

## For the maintainer to decide

1. **Where the owner's ceiling lives.** This branch applies it at install admission. For a Local
   Agent that is the owner's own `vosx` and node. For a replicated Agent each node's operator
   admits, which is policy, not consensus. A ceiling the runtime enforces from the Agent's state
   would be a field in `RuntimeResourcePolicy` (owner-mutable, bounded by a new
   `RuntimeResourceLimits` field in the runtime package's contract), checked at invoke against
   the program's length. The runtime never sees the program at Install: `InstallActor` carries
   the package reference, not the program's. A `SetResourcePolicy` path for Local Agents would
   also be needed; it exists only as a Private control today. Every one of those is an inline
   wire field with no versioning seam, so it is a runtime ABI change, and yours to schedule.
2. **The hard maximum.** 1 792 KiB is measured, not derived. The runtime's peak memory also
   depends on caller availability (up to 192 KiB) and message size, which the probe kept small.
   A compiled-runtime gate that loads the largest program at the largest state would pin it.
   With `0004` merged, 2 560 KiB runs at the state ceiling; beyond that the bounds are the 16 MiB
   argument window and the 5 G management gas. A deterministic refusal, instead of a guest panic,
   when program and state together would exhaust the arena would make the ceiling a rule rather
   than a margin.
3. **Heap sizes as features.** Three steps keep the choice reviewable in `Cargo.toml`. A build-time
   value would be finer grained.
4. **A runtime bound on actor memory.** Nothing limits an actor's declared heap except the header's
   width (256 MiB) and the gas for mapping it. Host memory is only touched as the actor writes.
   A runtime ceiling on mapped actor memory would make that explicit.
5. **Repinning the bundled runtime** after review.
