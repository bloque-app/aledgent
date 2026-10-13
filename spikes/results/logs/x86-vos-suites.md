<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

## x86-64 bench: vos-suites

### Host

```
Linux 7.0.0-30-generic x86_64
cpu: AMD EPYC-Genoa Processor
cpus: 16
avx2 avx512f 
MemTotal:       32078284 kB
load: 1.61 0.74 0.28
```

### VOS

- `https://codeberg.org/pandres95/VOS.git` `agent/recompiled-agent-execution`

```
3140c3d feat(pvm): opt-in recompiled Refine execution for Agents
```

Toolchain: `rustc 1.88.0-nightly (50aa04180 2025-05-08)`.

`GREY_PVM` picks the kernel path's engine (the recompiler by default on x86-64). The Refine
path (Agents) is the interpreter unless a test selects the recompiler: `refine_backends.rs`
and `agent_runtime_pvm.rs` run both engines in lockstep themselves.

#### vos-pvm, kernel path on the recompiler

` cargo test --locked --no-fail-fast -p vos-pvm` — exit 0, 39 s (build included)

| Test binary | Passed | Failed | Ignored |
|---|---:|---:|---:|
| `unittests src/lib.rs` | 264 | 0 | 2 |
| `tests/pvm_vectors.rs (child run of one test)` | 1 | 0 | 0 |
| `tests/pvm_vectors.rs` | 20 | 0 | 0 |
| `tests/refine_backends.rs` | 7 | 0 | 0 |
| `tests/spi.rs` | 4 | 0 | 0 |
| `doc-tests vos_pvm` | 0 | 0 | 0 |

#### vos-pvm, kernel path on the interpreter

`GREY_PVM=interpreter cargo test --locked --no-fail-fast -p vos-pvm` — exit 0, 19 s (build included)

| Test binary | Passed | Failed | Ignored |
|---|---:|---:|---:|
| `unittests src/lib.rs` | 264 | 0 | 2 |
| `tests/pvm_vectors.rs (child run of one test)` | 1 | 0 | 0 |
| `tests/pvm_vectors.rs` | 20 | 0 | 0 |
| `tests/refine_backends.rs` | 7 | 0 | 0 |
| `tests/spi.rs` | 4 | 0 | 0 |
| `doc-tests vos_pvm` | 0 | 0 | 0 |

#### Gray Paper v0.8.0 vectors (`pvm/runtime/tests/VECTORS.md`)

Kernel path on the recompiler:

```
every_gas_budget_exits_identically_across_backends ... ok
graypaper_v080_block_gas_corpus_matches_the_oracle_table ... ok
graypaper_v080_block_gas_matches_checked_in_oracle ... ok
graypaper_v080_corpus_matches_the_generator_table ... ok
graypaper_v080_executable_opcode_coverage_is_exact ... ok
graypaper_v080_interpreter_satisfies_every_vector ... ok
graypaper_v080_opcode_manifest_is_exact ... ok
graypaper_v080_recompiler_satisfies_every_vector ... ok
interpreter::tests::standard_unary_opcodes_use_the_v080_table ... ok
interpreter_satisfies_every_vector ... ok
nested_refine_programs_agree_across_backends_at_every_budget ... ok
recompiler_satisfies_every_vector ... ok
v080_cyclic_jit_memory_families_stay_inside_native_window ... ok
v080_cyclic_load_width_boundaries_are_exact ... ok
v080_cyclic_store_is_atomic_and_retry_does_not_recharge ... ok
v080_cyclic_wide_access_orders_high_tail_before_low_zone ... ok
v080_external_exit_counters_and_host_resume_are_exact ... ok
v080_low_zone_panics_before_a_mapped_memory_access ... ok
v080_page_fault_retry_preserves_state_and_does_not_recharge ... ok
v080_vectors_as_inner_machines_agree_across_backends ... ok
```

Kernel path on the interpreter:

```
every_gas_budget_exits_identically_across_backends ... ok
graypaper_v080_block_gas_corpus_matches_the_oracle_table ... ok
graypaper_v080_block_gas_matches_checked_in_oracle ... ok
graypaper_v080_corpus_matches_the_generator_table ... ok
graypaper_v080_executable_opcode_coverage_is_exact ... ok
graypaper_v080_interpreter_satisfies_every_vector ... ok
graypaper_v080_opcode_manifest_is_exact ... ok
graypaper_v080_recompiler_satisfies_every_vector ... ok
interpreter::tests::standard_unary_opcodes_use_the_v080_table ... ok
interpreter_satisfies_every_vector ... ok
nested_refine_programs_agree_across_backends_at_every_budget ... ok
recompiler_satisfies_every_vector ... ok
v080_cyclic_jit_memory_families_stay_inside_native_window ... ok
v080_cyclic_load_width_boundaries_are_exact ... ok
v080_cyclic_store_is_atomic_and_retry_does_not_recharge ... ok
v080_cyclic_wide_access_orders_high_tail_before_low_zone ... ok
v080_external_exit_counters_and_host_resume_are_exact ... ok
v080_low_zone_panics_before_a_mapped_memory_access ... ok
v080_page_fault_retry_preserves_state_and_does_not_recharge ... ok
v080_vectors_as_inner_machines_agree_across_backends ... ok
```

Fixture built first, as `just build-agent-recovery-fixture` does: the scripted recovery guest.

Fixture built first, as `just build-extensions` does: `echo-extension`.

#### vos --lib, kernel path on the recompiler

` cargo test --locked --no-fail-fast -p vos --lib` — exit 0, 140 s (build included)

| Test binary | Passed | Failed | Ignored |
|---|---:|---:|---:|
| `unittests src/lib.rs (child run of one test)` | 1 | 0 | 0 |
| `unittests src/lib.rs (child run of one test)` | 1 | 0 | 0 |
| `unittests src/lib.rs` | 1511 | 0 | 2 |

#### vos --lib, kernel path on the interpreter

`GREY_PVM=interpreter cargo test --locked --no-fail-fast -p vos --lib` — exit 0, 53 s (build included)

| Test binary | Passed | Failed | Ignored |
|---|---:|---:|---:|
| `unittests src/lib.rs (child run of one test)` | 1 | 0 | 0 |
| `unittests src/lib.rs (child run of one test)` | 1 | 0 | 0 |
| `unittests src/lib.rs` | 1511 | 0 | 2 |

#### vos agent_runtime_pvm (every execution on both engines)

` cargo test --locked --no-fail-fast -p vos --test agent_runtime_pvm` — exit 0, 8 s (build included)

| Test binary | Passed | Failed | Ignored |
|---|---:|---:|---:|
| `tests/agent_runtime_pvm.rs` | 6 | 0 | 1 |

Result: exit 0.
