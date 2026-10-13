<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 — lever 2: the book in the Agent's committed maps

Notes of `T-031-06` to `T-031-08` (`DEC-22`, amendment 0012). To be folded into this feature's
`NOTES.md` when the design PR (#124) lands.

## What was built

- **`core_binding::committed`** (`T-031-06`): a sparse Merkle tree kept as the Agent's own rows and
  maintained incrementally: only branching nodes stored (`n − 1` branch rows for `n` leaves), the
  structure of `F-006`'s `leaf_store::trie` and of VOS's `committed.rs`, over `transition::Rows`, so
  it runs natively and in the guest. Its roots equal CipherClerk's `SparseMerkleTree` under random
  inserts, updates and removals (a removal keeps the key with the empty sentinel, as `F-006` does).
  A subtree walk lists a kind's leaves without a range scan.
- **`core_binding::rowstore`** (`T-031-07`): the kernel's `LeafStore` over the six trees as
  committed maps: a load reads one row per leaf; a commit runs `F-001`'s write rules and writes each
  changed leaf with its path. The writes go to an overlay shared with the transition, applied only
  when the invocation commits. It reaches `MemStore`'s roots on the kernel's store suite.
- **`core_binding::mapped`** (`T-031-07`): layout 3. Genesis runs inside the Agent over the maps.
  An input carries the calls, the deliveries, the **service keys** it touches (keys only, no values,
  no proof) and the private blob; the Agent loads those service leaves from its map, runs the
  library, writes back only the leaves whose value changed, and reports the same roots layout 2
  reports. The host's driver drives either layout (`Driver::found_mapped`, recovery reads the layout
  from the hot row).
- **Conformance through the Agent** (`T-031-08`): a target `through-agent/mapped`, on by default.
  It passes exactly what `through-agent/native` passes: 193 cases pass, 62 are not runnable there
  (they need the raw kernel and store ports), 7 pending, on both.

## Measured, natively (an observation)

`crates/core-binding/tests/mapped.rs`, release, 500 transfers after a funded setup, the Agent's
transition through `NativePort` (the same code the guest runs), on a 12-core laptop under a load
average of about 50:

| Layout | Transition p50 | Mean | Bytes an invocation carries, p50 | Agent rows after |
|---|---:|---:|---:|---:|
| 2 (witnesses) | 1 841 µs | 1 858 µs | 6 960 B | 3 |
| 3 (committed maps) | 1 085 µs | 1 132 µs | 859 B | 10 095 |

The transition takes **41 % less time** and an invocation carries **88 % fewer bytes**: no
multiproof to decode and fold twice, no witness to verify, only the paths of changed leaves hashed.
The remaining time is mostly the kernel's own work (signatures and commitments), which no layout
changes.

## Measured on VOS (x86-64 recompiler, an observation)

Run 37098709186 of the manual
bench, mode `mapped-timing`: AMD EPYC, the core Agent on VOS `3140c3d` with `de0f12f` picked, 100
reservations after a warm-up of 3, both layouts with the same raised bounds (the stress suite's, plus
the program limit at 4 096 KiB and the fetch and compression quotas: a storage read is a fetch, and
stock VOS allows 16 plus one per blob in an invocation).

| Layout | Round p50 | p99 | Invoke gas p50 | Actor gas p50 | Blobs | Agent state |
|---|---:|---:|---:|---:|---:|---:|
| 2 (witnesses) | 329 ms | 447 ms | 2 064 M | 294.9 M | 6 569 B | 11.9 KB |
| 3 (committed maps) | 361 ms | 414 ms | 2 215 M (2 477 M max) | **219.8 M (−25 %)** | 149 B | 359 → 671 KB |

- **The actor's own work falls by a quarter**, as natively: no witness to decode and verify.
- **The invocation as a whole costs 7 % more at p50, and more as the book grows**: a Local Agent's
  rows are its image, moved whole on every invocation (359 KB of state after the warm-up, 671 KB
  after 100 reservations). This is the term external state blocks on a Shared Agent remove, and why
  layout 3 is not for Local.
- **Two VOS bounds a committed map meets at once** (for `DEP-11` and @olanod (VOS's maintainer)): the fetch quota (16
  storage reads plus one per blob per invocation) and the execution-state bound (48 KiB).
- **The program**: with layout 3 linked, the core's program is 2 630 772 B (the summary's), past the
  2 560 KiB the other modes raise to. Layout 3 is therefore a guest feature (`core-binding/mapped`,
  `aledgent-core/mapped`), off by default: the production guest does not carry it.

## Decisions

1. **Our map, CipherClerk's roots.** The tree is core-binding's (Apache-2.0 OR MIT), byte-compatible
   with VOS's `committed.rs`; converging on VOS's type is a question for @olanod (owner question `c-12`).
2. **Keys, not values, from the host.** The service store is still a partial store that refuses
   what it was not given (`ledger_core::leaves`), so the host names the service leaves an input
   touches; the Agent reads their values from its own rows. A lazy loader inside the library would
   remove even the key list; it changes `ledger-core`'s leaf maps, and is left for later.
3. **Layout 3 on Local is a test, not a deployment.** VOS's production Local Agent is image-based:
   the rows are moved whole on every invocation, and VOS refuses more than 48 KiB of execution state
   and 16 384 rows. After 500 transfers the Agent holds 10 095 rows. Layout 3 is for a Shared Agent
   over external state blocks (`DEP-1`, `DEP-11`); layout 2 stays the default.
4. **Private inputs unchanged** (`REQ-RT-6`): the openings and keys stay a caller blob; a test scans
   the rows for the key-ring root.

## Found in passing

`crates/core-binding/tests/agent_core.rs` (feature `ingress`) fails on `main`: it expects `200` for a
reservation that ingress now acknowledges `202 Received` (amendment 0007). CI does not run it
(`cargo test --workspace` does not enable `core-binding/ingress`).
