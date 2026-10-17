<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-037 — Code history and upgrades

| | |
|---|---|
| Milestone | Not assigned by `PLAN.md`; ordered after `F-036` and before `F-021` |
| Home | `crates/ledger-core` (`code`, the manifest; `anchor`, checkpoint `v3`), `crates/core-binding` (the hot row, the ring entry, the fence, `Continuation`, per-range roll-forward), `crates/core-archive` (checkpoints as rows and leaf chunks, the founding record, the code map), `agents/core` (the self-identity, the upgrade handler), `agents/core-host` (`switch`, `CodeSource`, restore), a new `aledgent-code-fetch` and `aledgent-code-source-plugin`, `crates/conformance` (`f037.rs`), `.github/workflows` (`code-history`) |
| Depends on | `F-036` (the chainspec names *H₀* and the initial mode; `StateGenesis`), `F-005` (the core Agent and its archive), `F-035` (the host's keys) |
| Unblocks | `F-021` (warp sync admits a fresh replica); a later move of the book to another runtime as an upgrade |
| Realises | `REQ-RT-3`, `REQ-RT-15` to `REQ-RT-21`, `REQ-PR-11`, `NFR-22`, `US-K7` (amendments 0023 and 0033; `DEC-26`) |

## Goal

The book says which code ran each of its inputs, keeps that code for as long as the book exists, and changes
it only by an input of its own (amendment 0023). It does so without tying any of that to the runtime it runs
on today, so that a later move to another runtime is one more upgrade, with no new genesis (amendment 0033).

## Spec traces

- `REQ-RT-15`: each writer holds its code version, construction and mode; checkpoints carry them and anchors
  bind them.
- `REQ-RT-16`, `REQ-RT-18`: an upgrade, a change of mode and a migration are inputs of the writer, authorised
  under the mode in force, applied forward only. A change of platform is an upgrade.
- `REQ-RT-17`, `NFR-22`: every code version is kept by digest, fetched from any source and checked.
- `REQ-RT-19`: the code version names its platform; nothing of the platform is hashed; checkpoints are
  aledgent's own format; replay needs no platform.
- `REQ-RT-20`: one switch step between input *n* − 1 and *n*, fenced by the code it replaces; the mode is the
  only authority over code.
- `REQ-RT-21`: a writer continued by another at input *n*, with its log head.
- `REQ-PR-11`: a migration of construction, with a bridge anchor.

## Design

`DEC-26` holds the design: (a) to (h) from amendment 0023, amended by amendment 0033, and (i), (j) from 0033.
In build order:

1. **The manifest** (`aledgent/code/v1`): platform, artefacts with digest, length, identities and build
   profile, the optional runtime, the self-identity of each package, the `replayer` role. *H* is its digest.
2. **The writer's state**: `code`, `construction`, `upgrade`, `authority`, `authority_next` in the hot row;
   `code` in each ring entry, under the log hash. `driver` and `observed_slot` are bound to the platform and
   enter no hash.
3. **Checkpoint `v3`**: the commitment amendment 0023 fixed, built from aledgent's fields alone.
4. **The fence**: while an upgrade to *H* from *n* is pending, code that is not *H* refuses input *n* and
   later, with nothing written.
5. **`switch(H, n)`**: one host operation; the registry's compare-and-swap on `saga`, an install into the
   same instance on `vos-core`, the founding of a continuation across platforms. The host checks the switched
   program's self-identity before input *n*.
6. **The inputs**: `Upgrade`, the modes and their inputs.
7. **Retrieval**: `CodeSource`, plugins, Kreivo preimages.
8. **Checkpoints as rows and leaf chunks**: the archive's checkpoint is the writer's rows and the leaves,
   never `agent.state`.
9. **Per-range replay**: native first for the book; the Agent side under each range's `replayer`.
10. **The continuation**: `StateGenesis` generalised; the founding archived; the code map's boundary.
11. **Migration of construction**, **fast and full resync**, the per-shard archive.
12. **The `code-history` gate** and **the legal hold**.

`tasks.md` lists the tasks in this order.

## Out of scope

- The move to `vos-core` itself. This feature makes it an upgrade; doing it is its own work, once
  `vos-core`'s formats settle.
- Changes to VOS. A compare-and-swap on install, and a way for an actor to read its own program's hash, would
  each strengthen the switch on `vos-core`; the fence does not depend on them.
- Changes to the settlement contract (ruling `a0023-q4`).
