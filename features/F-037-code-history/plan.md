<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-037 — Code history and upgrades

| | |
|---|---|
| Milestone | Not assigned by `PLAN.md`; ordered after `F-036` and before `F-021` |
| Home | `crates/ledger-core` (`code`, the manifest; `anchor`, checkpoint `v3`), `crates/core-binding` (the hot row, the ring entry, the fence, `Continuation`, per-range roll-forward), `crates/core-archive` (checkpoints as rows and leaf chunks, the founding record, the code map), `agents/core` (the compiled `GENERATION`, the upgrade handler), `agents/core-host` (`switch`, `CodeSource`, restore), a new `aledgent-code-fetch` and `aledgent-code-source-plugin`, `crates/conformance` (`f037.rs`), `.github/workflows` (`code-history`) |
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
- `REQ-RT-20`: one switch step between input *n* − 1 and *n*, fenced by the code it replaces through the code
  generation, with a way out (move or cancel the upgrade) and an alert on a long fence; the mode is the only
  authority over code.
- `REQ-RT-21`: a writer continued by another at input *n*, carrying its whole writer state and log head,
  founded once per input number.
- `REQ-PR-11`: a migration of construction, with a bridge anchor.

## Design

`DEC-26` holds the design: (a) to (h) from amendment 0023, amended by amendment 0033, and (i), (j) from 0033.
Amendment 0033's §"The architect's decisions" (D1 to D26) records why the fence uses a code generation and is
always on, why it has a way out decided the same by either generation, why the execution node checks the
runtime and installs what the code map names, and why a continuation is founded once per input number, as
the claim of that number, with its record written last. In build order:

1. **The manifest** (`aledgent/code/v1`): platform, `generation`, artefacts with digest, length, identities
   and build profile, the optional runtime. *H* is its digest. Each package is built with a `GENERATION`
   constant equal to the manifest's, in the ELF section `.aledgent.generation`, read by one function at
   build time, at staging and in the host's checks. The replayer is not in it.
2. **The writer's state**: `code`, `generation`, `construction`, `upgrade`, `authority`, `authority_next` in
   the hot row; `code` in each ring entry, under the log hash. `driver` and `observed_slot` are the only
   host-local fields (`SPEC.md` §2 **Writer state**) and enter no hash.
3. **Checkpoint `v3`**: the commitment amendment 0023 fixed, built from aledgent's fields alone.
4. **`switch(H, n)`**: one host operation, installing only bytes staged and checked before the upgrade
   committed. It is the registry's compare-and-swap on `saga`, signed with the core's package key through
   the signer. In the in-process runtime
   the core host embeds (`agents/core-host`), the equivalent is the runtime's in-place upgrade of the
   installed actor, a compare-and-swap on the installed deployment: its management receipt is signed by the
   Agent's authority key, as an install's, and the staged package is signed by the package key (its
   producer), both through the signer, so no key is in the host (a note of `T-037-14`, not a change of the
   requirement). It is an install into the same instance on `vos-core`, and the founding of a continuation
   across platforms. While an upgrade is scheduled, the host ends its batches at *n* − 1. At start and at
   every switch, the host checks every artefact it runs, the runtime included, against the code version the
   code map names for the next input, by digest, and installs that version where any differs. The switch is
   symmetric: it installs a newer generation after an upgrade and the old one again after a way-out input.
5. **The inputs**: `Upgrade`, with the staging check (digests, the next generation, the compiled constant).
6. **The fence, always on, and the way out**: code whose `GENERATION` is neither the state's nor, from a
   scheduled `from`, the next one refuses the input, with nothing written. `RescheduleUpgrade` and
   `CancelUpgrade` are decided the same by either generation. The time spent fenced is a metric with an
   alert.
7. **The modes** and their inputs. A mode names each principal with one of its credentials
   `[PENDING AMENDMENT 0034]`.
8. **Retrieval**: `CodeSource`, plugins, Kreivo preimages.
9. **Checkpoints as rows and leaf chunks**: the archive's checkpoint, and a snapshot sent to another node,
   hold the writer state and the leaves, never `agent.state`.
10. **Per-range replay**: native first for the book, under each range's generation; the Agent side under
    each range's replayer, `replayer/<hex H>` in the code archive.
11. **The continuation**: `Continuation` beside `StateGenesis`; the writer state carried and compared field
    by field; no founding over a queued way-out input or for an upgrade no longer scheduled; the founding
    record written last, once, as the claim of *n* per (genesis, logical writer, *n*); the continuation's
    instance recorded per numbered build attempt, write-once, adopted by a retry only if it passes, and
    checked by the archive; fixed claims on both sides; each input number of a logical writer archived once;
    the upgrade final once founded, its way-out refused fence-style; the code map's boundary from the
    `Upgrade`.
12. **Migration of construction**, **fast and full resync**, the per-shard archive.
13. **The `code-history` gate** and **the legal hold**.

`tasks.md` lists the tasks in this order.

## Out of scope

- The move to `vos-core` itself. This feature makes it an upgrade; doing it is its own work, once
  `vos-core`'s formats settle.
- Changes to VOS. A compare-and-swap on install, and a way for an actor to read its own program's hash, would
  each strengthen the switch on `vos-core`; the fence does not depend on them. That an instance's rows
  survive a re-install on `vos-core` is an assumption, to be shown by the spike before `switch`'s `vos-core`
  form is used.
- Changes to the settlement contract (ruling `a0023-q4`).
