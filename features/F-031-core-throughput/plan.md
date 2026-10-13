<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 — Core throughput and the Shared book

| | |
|---|---|
| Milestone | M-4 — Replicated (levers 1 and 2 and the key layout land before it, on the Local core) |
| Track | B |
| Home | `crates/core-binding` (`envelope`, `transition`, `host`, `core`; `committed`, new), `agents/core`, `agents/core-host`, `services/ingress` (forwarder, `Core::apply_batch`, remote protocol), `crates/ledger-kernel` and `crates/ledger-core` (partitioned keys), `crates/shards` (new: the shard prototype), `.github/x86-bench` |
| Depends on | `F-005`, `F-006`, `F-009`, `F-015`. `DEP-1` and `DEP-11` for the Shared deployment, not for the levers |
| Unblocks | `M-4`; `NFR-18` |
| Realises | `DEC-22`, amendment 0012 |

## Goal

The owner's target is millions of concurrently active agents. The core as an Agent decides about 3.5
reservations a second: 284 ms p50 per invocation on the x86-64 recompiler, one invocation at a time
(`notes/MEASUREMENTS-F005.md` §9), against 1.3 ms for the same decision natively. About five sixths of an
invocation's gas (1 747 M) is fixed cost outside the actor (295 M): the runtime reloading the 2.17 MB
program and its own state on every invocation. The actor's share is mostly proof hashing and decoding of
the witnessed leaves.

Three levers, each measured before and after:

1. **Batching.** Many inputs per invocation: the fixed cost is paid once per batch.
2. **Cost proportional to touched state.** The kernel trees and service leaves in committed maps in the
   Agent's rows: an invocation reads only the leaves it touches, verifies no witness, and hashes only the
   paths it writes.
3. **Split by account.** Partition-prefixed keys now; a prototype of several shards with the credit path;
   more only if the measurements justify it.

## Spec traces

- `REQ-RT-1` (per partition), `REQ-RT-9`–`REQ-RT-12` (amendment 0012): partitions, ordering per payer,
  transfers final at debit with the credit guaranteed, pools.
- `INV-5`: the trial balance, in-transit positions included, is zero at every commit of every shard.
- `INV-9`: a credit is never refused.
- `INV-18`, `REQ-RT-2`: exactly once, for inputs in a batch and for credits between shards.
- `INV-19`: an anchor over every shard's roots at one cut.
- `INV-25`: one book.
- `NFR-18` (throughput: measured and reported by the stress suite, no committed figure; aim 10 000
  decisions a second to start, a million better), `NFR-19` (credit lag, 2 s p99 proposed).
- `REQ-RT-3`: a batch replays to the same state as its inputs one by one.

## Design

### Lever 1: batching (`T-031-02` to `T-031-05`)

- **Envelope.** `Envelope::Batch(Vec<InputEnvelope>)` and `Reply::Batch(Vec<Reply>)`. Each input keeps its own
  witness and its own blobs, built against the state after the input before it, exactly as consecutive
  single inputs are today. The Agent runs them in order over an overlay of its rows.
- **Isolation by prefix commit.** The batch commits its longest prefix of accepted inputs. The first input the
  Agent refuses answers its refusal; every later one answers `Skipped`. Library refusals (`ERR-*`) are
  recorded per command and commit, so they never stop a batch. After a failed batch, the host rebuilds its
  replica and drives the rest one at a time, so a faulty input fails alone: the hybrid isolation the owner
  ruled for the contract's batches (#102).
- **Bounds.** A configurable count (default 32), and VOS's limits: 16 KiB of message, 16 KiB of reply, 192 KiB
  of caller blobs. The driver stages inputs and flushes before a bound would be crossed.
- **Ingress.** `Core::apply_batch`, with a default that applies one by one; the forwarder takes up to
  `core_batch` undecided rows in log order and decides the committed prefix in order.

### Lever 2: committed maps (`T-031-06` to `T-031-08`)

- **The map.** `core_binding::committed`: a sparse Merkle tree over the Agent's rows, maintained
  incrementally, byte-compatible with CipherClerk's roots (`F-006`'s trie and VOS's `committed.rs` keep the
  same tree). Rows exist only where keys branch; a mutation touches the branching points along its key.
- **The state.** The six kernel trees and the service tree are committed maps under their own prefixes, with
  leaves in the encodings they have today. `RowStore` implements the kernel's `LeafStore` over them; the
  service store loads leaves from them. The roots the Agent reports are the roots it reports today, so the
  driver's comparison and the conformance suite need no change.
- **What the host stops doing.** No kernel or service witness: the Agent reads its own leaves. The private
  blob (openings, key-ring root, clerk key) stays a caller blob (`REQ-RT-6`).
- **Where it pays.** On a Local Agent the rows are part of the image (VOS's production Local stays
  image-based), so the image grows with the book and the 4 MiB and 16 384-row ceilings return. The lever pays
  on a Shared Agent over external state blocks (`DEP-1`, `DEP-11`). The measurements say which costs it
  removes on Local, for a book that fits.

### Lever 3: split by account (`T-031-09` to `T-031-11`)

- **Partition.** `partition(account) = blake2b-256("aledgent/partition/v1" ‖ account id)[0..2]`, 2¹⁶
  partitions.
- **Keys.** The kernel account id of every kernel account of a ledger account, the transfer id of every
  posting it pays, and the service key of every account-scoped kind carry the partition in their top sixteen
  bits. Other kinds (registry, roles, settlement configuration) are global and live with the principals.
- **The prototype** (`crates/shards`). Two or more shards in one process, each a full book over its range of
  partitions, with: an outbound credit queue per peer, numbered per pair; a relay; the destination's applied
  sequence per source; in-transit positions per asset and peer; a directory copy for the payer's checks.
  Tests: exactly-once credit under duplicated, reordered and lost deliveries; per-payer ordering under
  concurrent debits; zero divergence of every balance against one book fed the same commands; the trial
  balance zero at every commit.

## Out of scope here

- The principals Agent and the revocation feed (`DEC-22`): designed; built when the shards are, after the
  prototype's measurements.
- A Shared deployment: waits for `DEP-11`.
- Hot-account pools (`REQ-RT-12`): designed; built with the shards.

## Exit criterion

Each lever's ops/s is measured on the x86-64 recompiler, before and after, and recorded. The conformance
suite passes through the Agent over committed maps. The shard prototype passes exactly-once credit,
per-payer ordering and zero divergence against one book.
