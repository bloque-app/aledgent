<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-033 — aledgent's runtime and the book's Agents

| | |
|---|---|
| Milestone | M-4 — Replicated (the runtime and the core's composite batches land before it, on the Local core) |
| Track | B |
| Home | `agents/aledgent-runtime` (new: the runtime guest), VOS fork branch `aledgent/composite-runtime` (the engine: `vos::agent::composite`, `wire::composite_engine`, feature `runtime-composite`), `agents/core-host` (composite batches, the relay), `.github/x86-bench` (the runtime as a bench variant, the conformance mode) |
| Depends on | `F-005`, `F-015`, `F-025` (the `perf/agent-stack` the engine sits on), `F-031` (shards, the principals Agent). `DEP-11` for the Shared deployment, `DEP-12` for upstream |
| Unblocks | `DEC-23`'s Agents; `DEP-2` (ticks); the actor split `DEC-22` deferred |
| Realises | `DEC-23`, amendment 0014 |

## Goal

A runtime of our own, pinned by every Agent of the book, that does three things VOS's standard runtime does
not: commit several of an Agent's actors in one Linear transition; keep each payer's commands in order and
isolate each command inside one invocation; and apply every message between Agents exactly once, in its
sequence, as part of the receiving transition. Then the Agents around it: settlement and anchor beside the
shards and the principals Agent, the relay that moves messages, and online splits.

The owner's ruling (2026-10-03) makes this the plan whatever upstream decides about two runtime variants.

## Spec traces

- `REQ-RT-1`: one commit boundary per writer, enclosing every message the commit owes (amendment 0014 R1).
- `REQ-RT-10`: a payer's debits in order: order keys.
- `REQ-RT-11`, `REQ-RT-13`: messages between writers, exactly once and in sequence: streams.
- `REQ-AT-2`: one audit trail fed by every writer, gaps visible.
- `INV-18`: exactly once, at the invocation, the stream and the payer.
- `INV-19`: an anchor over every partition, the registry and the trail.
- `NFR-18`–`NFR-20`: throughput measured; the credit and revocation lags.

## Design

`DEC-23`. In short:

- **Composite invocations.** A Linear invocation (the primary) may carry one caller blob that starts with
  `VCMP`: a mode (atomic or isolated) and up to 128 steps, each a Linear method of an actor installed in the
  same Agent, with its gas and, optionally, an order key and a stream sequence. The runtime runs the primary,
  then each step against the state before it, and retains one result for the whole: exact retry,
  acknowledgement and recovery are the standard ones.
- **Atomic**: every step and the primary, or nothing; the failure is retained. **Isolated**: each step alone;
  a step the remaining gas cannot cover is deferred.
- **Order keys**: once a step with a key is not applied, every later step with the key is deferred.
- **Streams**: the runtime keeps each stream's applied head in its control image (section `SST1`, absent while
  empty) and applies a message only at head plus one, in the step's commit; a repeat is a duplicate, a gap
  waits.
- **The program**: VOS's standard runtime entry with the rules set first, and an optional lean public I/O
  (`direct-public-io`). The engine lives in VOS because the standard runtime's internals are crate-private.

## Measurement

On one GitHub-hosted runner per comparison (never the self-hosted fleet), the x86 bench's `compare` mode
measures the core's reservations under VOS's runtime and under ours at the same VOS commit and features:
fixed gas per invocation, actor gas, inputs per second at batches 1, 8 and 32, plain and as composites. Its
`conformance` mode runs the suite through the core Agent in VOS under each runtime. Results:
`NOTES.md`.

## Out of scope here

- Cross-Agent evidence a receiving runtime can check: needs a format from VOS (`DEP-12`).
- Upstreaming the engine: to be proposed to VOS.
