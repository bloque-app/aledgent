<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Let actors declare program, heap and stack within owner ceilings

Three fixed sizes stop a large application actor on the Agent path: a 1 280 KiB program limit, a 256 KiB
heap fixed at compile time, and a stack that can only grow by padding the program with zeros. This PR
lets an actor declare all three in its signed package, raises the runtime's hard program maximum to
1 792 KiB, and lets the Agent's owner set ceilings when it installs an actor. Packages without a
declaration are unchanged, byte for byte.

## Problem

At `2bb78760`:

1. **Program size.** The runtime resolves an actor program only up to `MAX_EXECUTION_PROGRAM_BYTES`,
   1 280 KiB (`vos/src/agent/execution.rs:38`; `MAX_RUNTIME_PROGRAM_BYTES` in
   `vos-agent-sdk/src/runtime.rs:24` has the same value). A larger program is never matched as the
   program (`vos/src/agent/standard.rs:2846-2847`), so every invocation is refused with
   `InvalidAvailability` (`:2900`). Package admission never checks the program's size
   (`vos/src/agent/package_admission.rs:231-240` only parses it), so such an actor installs and then
   never runs. Our ledger's core actor is 1 968 379 bytes with `opt-level = "z"`, LTO and every size
   flag.
2. **Heap.** An application actor's allocator arena is `HEAP_SIZE`, 256 KiB
   (`vos/src/abi/pvm/alloc.rs:14`). Past it the actor panics. Infrastructure guests get 8 or 32 MiB
   behind features (`:10`, `:12`); application actors have no choice.
3. **Stack.** `link_elf_spi` takes the stack size from the lowest read-only address
   (`pvm/compiler/src/linker.rs:567-580`). The scaffold links read-only data at `0x10000`, a 64 KiB
   stack. A 256 KiB stack means linking it at `0x40000`, which pads the program with 192 KiB of zeros
   that travel in every invocation and are paid for per byte.

Much of the identity side already exists: the package signs the program's length
(`manifest.program.len`), and the program header carries its heap pages and stack, so both are part of
the ProgramId. What was missing is a ceiling above 1 280 KiB, a heap an actor can choose, and a stack
that costs no padding.

## Change

One commit, 14 files, +677 −49; about 330 lines are tests.

- **`vos-agent-sdk/src/package.rs`: the declaration.** `ActorResources { program_bytes, heap_bytes,
  stack_bytes }` (all `u32`; heap in whole 4 KiB pages up to 65 535, stack below 2²⁴), carried as
  `ActorPackageManifest::resources: Option<ActorResources>` under a **new actor manifest tag, 3**. An
  undeclared manifest keeps tag 0 and its exact bytes and signature. This follows the
  `external_state_limits` precedent for runtime manifests (tag 1 against tag 2).
- **`vos-agent-sdk/src/runtime.rs`, `vos/src/agent/execution.rs`: the hard maximum.** New
  `STANDARD_ACTOR_PROGRAM_BYTES = 1 280 KiB` (today's limit). `MAX_RUNTIME_PROGRAM_BYTES` becomes
  1 792 KiB, and `MAX_EXECUTION_PROGRAM_BYTES` is defined as equal to it. It is a constant of the runtime
  program, so it is covered by the runtime's ProgramId.
- **`vos/src/agent/package_admission.rs`: owner ceilings.** `ActorResourceCeilings { max_program_bytes,
  max_heap_bytes, max_stack_bytes }` with `standard()` (exactly today's behaviour) and
  `runtime_maximum()`. `admit_actor_package_within(bytes, ceilings)` is used for **new installs only**:
  above the standard program size the package must declare, and anything over a ceiling is refused with
  `PackageAdmissionError::ResourceCeiling`. `actor_program_resources(program)` reads what a program
  encodes. On every path, a declaration that differs from the program header is refused with
  `ResourceDeclarationMismatch`.
- **Re-admission applies no ceiling.** `admit_actor_package` (replay, recovery, journal drivers, intent
  stores) still decides on the package bytes alone; its only new refusal is a tag-3 declaration that
  contradicts its program, and no existing package has tag 3.
- **`vos/Cargo.toml`, `vos/src/abi/pvm/alloc.rs`: the heap.** Features `actor-heap-1m`, `actor-heap-4m`,
  `actor-heap-16m`; the largest selected wins, none keeps 256 KiB. The arena is `.bss`, so the choice
  lands in the program's heap pages, and so in the ProgramId.
- **`pvm/compiler/src/linker.rs`: the stack.** A guest may define `__vos_stack_size` in its linker
  script (whole pages, below the `E3` field width). `link_elf_spi` then uses it for the program's stack
  and read-only data can sit at the standard base with no padding. Without the symbol nothing changes.
- **`vosx`.** `vosx actor build` (`vosx/src/commands/build.rs`) declares automatically when the program
  exceeds the standard size, and refuses one above the hard maximum. `vosx space install-local-actor
  --max-program-bytes <N>` (`vosx/src/commands/space/local_install.rs`) raises the owner's program
  ceiling for that install; the default is the standard size.
- Four fixture literals gain `resources: None` (`clean_bootstrap.rs`, `local_sdk_host.rs`,
  `state_block_pvm_lifecycle_tests.rs`, `vos/tests/agent_runtime_pvm.rs`).

**Public API:** additive (`ActorResources`, `STANDARD_ACTOR_PROGRAM_BYTES`, `ActorResourceCeilings`,
`admit_actor_package_within`, `actor_program_resources`, `SPI_STACK_SIZE_SYMBOL`), plus a new field on
`ActorPackageManifest` and two new `PackageAdmissionError` variants, which break struct literals and
exhaustive matches outside the workspace at compile time.

## Design choices and alternatives

- **Declare, rather than infer.** Every declared value restates what the signed program already fixes.
  The declaration adds no execution rule; it makes a large actor a signed, visible choice of its
  producer, part of the package reference, the deployment identity and the producer's signature.
- **The owner's ceiling at install admission, not in the runtime.** A runtime-enforced ceiling would be
  a field in `RuntimeResourcePolicy`, bounded by a new field in the runtime package's limits, checked at
  invoke against the program's length; the runtime never sees the program at Install (`InstallActor`
  carries the package reference). Each of those is an inline wire field with no versioning seam, so it
  is a runtime ABI change and yours to schedule (question 1).
- **1 792 KiB, set by measurement.** The runtime decodes the whole work into its 32 MiB arena and today
  materialises the program several times beside the state. It panics, deterministically but not as a
  clean refusal, when the arena runs out. Measured with a real actor padded to size, at 4 121 295 bytes
  of state with 31 unacknowledged results (the worst case our harness can build):

  | Program | This PR alone | With #8 |
  |---:|---|---|
  | 1 792 KiB | runs (Invoke 2.53 G gas) | runs |
  | 1 920 KiB | runs (Invoke 2.62 G gas) | runs |
  | 2 048 KiB | runtime panics at 4 054 287 B of state | runs |
  | 2 560 KiB | runtime panics at 3 853 263 B of state | runs (Invoke 2.44 G gas) |

- **Heap as three features**, so the choice is reviewable in `Cargo.toml`. A build-time value would be
  finer-grained (question 3).

## Compatibility

- **Existing packages and installs:** unchanged. Tag 0 encodes byte for byte as before, and
  re-admission of every existing package decides exactly as before.
- **Opt-in:** only actors above 1 280 KiB need a declaration, and only an owner who raises a ceiling
  admits one.
- **Older hosts:** a host built before this PR does not know tag 3 and refuses such a package.
- **Gas:** program bytes are paid through the runtime's per-byte load cost (about 630 gas per byte
  today, 462 with #8); heap pages when mapped (`pages`, 121 gas per page).
- **Yielding actors:** a continuation still captures at most 4 MiB of writable memory
  (`MAX_PORTABLE_MACHINE_MEMORY_BYTES`, `execution.rs:144`). An actor that declares more heap cannot
  yield. Unchanged, and now visible in its declaration.
- **Artifact:** `vosx/blobs/agent_runtime.pvm` is not rebuilt; the new hard maximum only applies once it
  is.
- **Migration for an actor author:** define `__vos_stack_size` in the linker script and move read-only
  data back to `0x10000`; enable an `actor-heap-*` feature if needed; build with `vosx actor build`,
  which declares when required. The installing owner passes `--max-program-bytes`.

## Tests

New:

- `vos-agent-sdk` (`package.rs`): `undeclared_actor_manifest_keeps_the_released_tag_and_bytes`,
  `declared_actor_resources_are_signed_under_their_own_tag` (dropping the declaration changes the
  deployment identity and breaks the signature),
  `declared_actor_resources_must_restate_a_representable_program`.
- `vos` (`package_admission.rs`): `actor_over_the_standard_program_installs_only_when_declared_and_admitted`
  (re-admission unchanged; standard ceilings refuse; a raised ceiling admits the declared package only;
  each ceiling refuses one byte or page over; a ceiling above the hard maximum is invalid; a mismatched
  declaration is refused on both paths).
- `vos` (`agent/wire.rs`): `clean_resolver_executes_declared_programs_up_to_the_hard_maximum` (the
  standard size, one byte over, the hard maximum; one byte over the hard maximum refused).
- `vos-pvm-compiler` (`linker.rs`): `declared_stack_size_replaces_the_layout_derived_stack`.

Run on Linux aarch64, `nightly-2025-05-09`:

| Command | Result |
|---|---|
| `cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'` | 1 780 passed, 17 ignored, 7 failed; the same 7 fail on unmodified `2bb78760` |
| the same with `--ignored compiled_runtime compiled_shared_genesis` and `AGENT_RUNTIME_CANDIDATE_ELF` built from this branch | 4 passed |
| `vos` with `experimental-state-blocks`, filtered to `agent::wire`, `replay`, `state_block*`, `execution`, `actor_storage`, `package_admission` | 228 passed, 26 ignored |
| `cargo test -p vos-agent-sdk`, with and without `experimental-state-blocks` | 251 and 250 passed |
| `cargo test -p vos-pvm-compiler --lib` | 67 passed |
| `cargo test -p vosx --bin vosx -- local_install build` | 16 passed |

**Not run:** the two recompiler tests (x86-64 only), the five that need prebuilt artifacts, and this
branch on x86-64. No compiled-runtime gate loads the largest program at the largest state; the arena
table above comes from our harness, not from VOS's suite.

**Our core actor on this branch** (2026-09-24):

| Build | Program | Result |
|---|---:|---|
| read-only data at `0x40000` (256 KiB stack), 256 KiB heap | 1 968 379 B | refused: above 1 792 KiB |
| `__vos_stack_size = 0x40000`, read-only data at `0x10000`, `actor-heap-4m` | 1 771 777 B | admitted; every reservation runs. Invoke 1 394.9 M gas against 1 487.1 M padded |

The declared stack removes 196 602 bytes of padding.

## Relation to the other pull requests

All five VOS pull requests (#6 to #10) are one commit each on `saga/agents` at `2bb78760`, still the tip
(checked 2026-09-29), and each applies alone. This one shares `vos-agent-sdk/src/runtime.rs` and
`vos/src/agent/wire.rs` with #6, `vos/Cargo.toml` and `vos/src/agent/execution.rs` with #9, and
`vos/tests/agent_runtime_pvm.rs` with #10, always in different regions. `git merge-tree` merges it
cleanly with each of the other four, and #6 + #7 + #8 + #9 and #6 + #7 + #9 + #10 merge cleanly together.
**It depends on #8 for headroom:** with #8 merged, 2 560 KiB runs at the state ceiling, and raising the
constant is a one-line follow-up we would send.

## Questions for you

1. **Where the owner's ceiling lives.** Install admission, as here, is each operator's policy for a
   replicated Agent, not consensus. Or a runtime-enforced ceiling, which is a runtime ABI change?
2. **The hard maximum.** 1 792 KiB is measured, not derived. Since this PR was opened our core has grown:
   the current build is 2 168 623 bytes with the padded stack, so about 1.97 MB with a declared stack by
   our estimate, above 1 792 KiB. We therefore need the 2 560 KiB follow-up once #8 is in. A clean
   refusal when program and state together would exhaust the arena, instead of a guest panic, would make
   the ceiling a rule rather than a margin.
3. **Heap sizes as three features**, or a build-time value?
4. **A runtime bound on actor memory.** Nothing limits a declared heap except the header's width (about
   256 MiB) and the gas to map it.
5. Repinning the bundled runtime after review.

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent, deciding each
balance-affecting command within a completion budget. Its core actor carries a confidential kernel and a
sparse-Merkle verifier, and even built for size it is about 2 MB. On `saga/agents` today it installs and
then can never run, and its 256 KiB heap is too small for a real book. This PR lets it run, as a signed
and visible choice that the Agent's owner admits.
