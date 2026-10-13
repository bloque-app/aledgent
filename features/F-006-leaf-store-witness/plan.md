<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-006 — Leaf store and witness path

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Track | B |
| Home | `crates/leaf-store` |
| Depends on | `F-001` (storage trait), `F-002` (measurements), `DEP-1` (shapes this feature) |
| Unblocks | `F-005`, `F-011` at scale, `F-014` at scale, `F-018` |
| Realises | `DEC-4` |

## Goal
The book's leaves, history and indexes live in a host-side store, outside the Agent image. The Agent
commits only roots and a delta record per transition. The store can build a small, verifiable witness
for any command. The store is a projection: it can be rebuilt from the Agent's committed delta log,
and the rebuild matches the live store bit for bit. A balance read stays within `NFR-3` at 150 million
entries, on an extremely skewed book.

## Spec traces
- `REQ-RT-1` — the store is outside the commit boundary, so it must be re-derivable from what is inside.
- `INV-6` — every stored balance can be re-derived from entries alone.
- `NFR-3` — a balance read within 5 ms p99 at 150M entries, with extreme skew.
- `INV-2` — store half: no committed leaf is changed or deleted.
- `INV-5` — store half: the per-asset sum is checked continuously.
- `INV-12` — the store persists applied coordinates, through the kernel's `external_id` set.
- `REQ-PO-2`, `REQ-QR-5` — history is indexed by the global sequence, for cursor paging.
- `REQ-BA-3`, `REQ-QR-2` — reads come from one view pinned to one sequence.
- `NFR-4`, `AC-K5.1` — a restore re-derives every balance.

## Scope
**In:** the leaf store; SMT node storage and proof generation; the witness builder; the delta record
format and its archive; the applier; read views; history and indexes; the openings store; rebuild and
bit-for-bit comparison; the continuous commitment checks for `INV-5` and `INV-6`.
**Out:** the Agent side of the delta log (`F-005`); query semantics (`F-011`); backups, replication
of the archive and paging (`F-018`); export and import (`F-014`).

## Design

**Why this shape.** On `saga`, `MAX_RUNTIME_STATE_BYTES = 4 MiB` bounds the whole Agent state,
storage rows included, and every invocation ships that image in and out
(`vos-agent-sdk/src/lib.rs:57`, enforced `vos/src/agent/actor_storage.rs:370-373`). That fits about 12k
accounts with no history (notes/FIT §1). A book of record cannot live there. So leaves live here and
arrive in the Agent per call as a witnessed batch (`DEC-4`).

**Limits this feature designs to**: one storage row value is at most 64 KiB
(`vos/src/actors/storage.rs:64`); rows per lane are at most 16 384 (`vos/src/agent/standard.rs:42`); the
invocation message and reply are each at most 16 KiB (`vos-agent-sdk/src/runtime.rs:14-15`). The first
two bound one delta record and the delta ring. The third bounds the witness carrier.

**What is stored.**
- *Kernel leaves.* CipherClerk's six sub-trees: accounts, transfers, journals, external ids, voided
  transfers, pending statuses. Leaf bytes are the kernel's canonical form: a 1-byte domain tag and the
  rkyv archive. The composite root folds the six sub-roots.
- *Service leaves.* **Proposal:** the library's own growing records get a seventh tree, the service
  tree, with the same SMT construction (depth 128, 16-byte keys, own domain tag). These are identities,
  ledger accounts, authorities and credentials, operation records, client keys and the deadline index.
  The Agent commits its root beside the kernel's. *Alternative:* keep service records in Agent state.
  That caps the book at whatever `F-002` shows fits in the hot-state budget.
- *SMT nodes.* Only non-default nodes are stored. Proofs must be byte-compatible with CipherClerk's
  `SmtProof`: exactly 128 siblings, and `clerk-witness`'s `CC_SMT_PARAMS`.
- *History and indexes.* Entries by (account, sequence). Accounts by identity. Authority holders by
  account (the store side of `INV-16`). A per-account reserved total, kept from deltas so `reserved` is
  O(1) under `DEC-2`. Query semantics belong to `F-011`. This feature keeps the indexes.
- *Openings store.* A separate, sealed keyspace: the current opening (value, blinding) per account and
  layer. It serves plaintext balance reads and the private input of `F-005`. It is never part of the
  leaf store, the delta record or the archive (`REQ-RT-6`).

**Storage engine.** **Proposal:** an embedded ordered key-value store with ACID single-writer
transactions and snapshots, such as `redb`. *Alternative:* RocksDB, stronger at 150M entries, heavier to
operate. The choice sits behind one trait and is revisited with `F-002`'s numbers before `M-3`.

**The delta record.** The unit the Agent commits and the store applies:
`DeltaRecord { sequence, prev_log_hash, roots_before, roots_after, kernel_delta, service_delta, ops }`.
`kernel_delta` is CipherClerk's `StateDelta`, a set of by-id puts in a fixed drain order. `log_hash = H(prev_log_hash ‖ canonical(record))`. The Agent keeps the head in its state. The
type lives in `crates/ledger-core` so the Agent can build it. A record must fit one 64 KiB row.

**The committed delta log.** The Agent's delta ring holds records until archived (`F-005`). The archive
is an append-only, segmented file set. A segment is fsynced before the Agent is told to trim. Every
record is self-checking: applying it to a store at `roots_before` must yield `roots_after`, and the hash
chain must reach the Agent's committed head. So the archive is the source of truth only because the
Agent's committed roots and head authenticate it (`DEC-4`). Losing an archive range means the store
cannot be rebuilt past it. That is why `F-018` replicates and backs it up.

**Witness builder.** The driver of `F-005` runs the library natively against a read view to find the
exact read set of a command. The builder turns that set into a witness: the touched leaves plus one
batch proof per sub-tree, which is CipherClerk's `SuccinctTransitionWitness`, cost O(touched · log N),
in `clerk-witness`'s `ClerkTransitionWitness` format. A read
missing from the set makes the Agent fail closed. A missing read is a bug, never a silent default.

**Applier and read views.**
1. Take the next record by sequence. Refuse a gap.
2. Check `roots_before` against the store's current roots.
3. Apply puts. Refuse any delta that overwrites an existing transfer leaf or deletes anything (`INV-2`).
4. Recompute the sub-roots. Compare them with `roots_after`. A mismatch stops the applier and raises an
   incident.
5. Update indexes and the openings store, then commit, recording `applied_sequence`.
Re-applying a record is a no-op, since `StateDelta::apply` is idempotent.
Every read runs on a snapshot pinned to one `applied_sequence` (`REQ-QR-2`). **Rule: read after
acknowledge.** The driver applies a record to the read view, at least in memory, before `F-009`
acknowledges the command. So the holder never sees a number older than the one the spend was checked
against (`REQ-BA-3`).

**Balance read at scale.** A balance read is two point reads: the account leaf and its opening. The
reserved total comes from its index. Cost does not depend on how many entries an account has. So an
account holding most of the book's entries reads as fast as any other.

**Commitment checks.** Pedersen commitments add up. So two checks need no openings:
- `INV-6`: for each account and layer, the sum of its entry commitments equals the stored `dr` and `cr`.
- `INV-5`: across the book, for each (currency, layer), the sum of debit points equals the sum of credit
  points.
Both run as store jobs. `F-018` schedules them and pages on failure.

**Rebuild.** From an empty store, apply every archived record from sequence 0, verifying the hash chain
and each `roots_after`. Then dump both stores canonically, with leaves and index rows in key order, and
compare digests. **Proposal:** verified store snapshots at checkpoint sequences, for faster restores.
The exit test always rebuilds from genesis.

**What `F-002` must show before this is finalised.**
1. Witness size and build time for a reservation and a transfer at 10k / 100k / 1M accounts. The witness
   plus the command must fit a 16 KiB message, or the carrier changes (`F-005`).
2. Delta record size per posting and per largest multi-leg batch. It must fit a 64 KiB row. This sets
   `ERR-BatchTooLarge` in `F-008`.
3. Apply and root-recompute time per record. The applier must keep pace with the Agent at peak.
4. Balance read p99 on the chosen engine at 1M accounts. This decides whether the 150M-entry test is
   worth running on the Proposal engine or on the alternative.
5. Rebuild throughput in records per second. This bounds restore time for `NFR-10`.

## Test plan
- **Exit, bit for bit.** Property test: random command streams from the library generators, through
  the in-process host and the delta path. Rebuild from the archive. Canonical digests must match.
  Repeat with crashes injected between archive, ack and apply.
- **Witness.** Random states: every witness verifies against the root. A tampered leaf, a wrong sibling,
  and a read outside the set are all rejected.
- **`INV-2`.** The applier refuses an overwrite of a transfer leaf and any delete.
- **`INV-6`, `INV-5`.** The commitment checks pass on honest stores. Each fails on a corrupted balance
  or a removed entry.
- **`INV-12`.** Applied coordinates survive a rebuild. A replay of the same coordinate is refused.
- **`REQ-BA-3`.** A read issued after an acknowledgement always reflects that command.
- **`NFR-3`.** A benchmark at 150M entries, most of them on one account, reports balance-read p99.
- **`AC-K5.1`** (with `F-014`, `F-018`). Restore, re-derive every balance, compare.

## Exit criterion
A store rebuilt from the committed delta log, from sequence 0, gives the same canonical digest as the
live store, for every leaf and every index row. A balance read at 150 million entries, with most of them
on one account, completes within 5 ms at p99 on the target machine (`NFR-3`).

## Open points
- **`DEP-1`.** If VOS plans out-of-image state, the witness path may be replaced. The history and index
  parts of this feature stay.
- **`RISK-2`.** If `F-002` shows the witness is too large or too slow, the fallback is a larger hot set in
  Agent state for the busiest accounts, plus the upstream ask.
- **Proposal:** the service tree. It touches the storage traits of `F-003`, `F-007` and `F-011`.
- **Proposal:** the storage engine (`redb` vs RocksDB).
- **Proposal:** verified snapshots at checkpoints.
- **The openings store is not re-derivable from the delta log** unless `F-005`'s deterministic-blinding
  Proposal is taken. Without it, the openings store is a second source of truth with its own backup.
  The owner decides.
- **Archive loss.** The store's re-derivability is only as good as the archive's durability. `F-018`
  owns this, and `RISK-5` should name the archive beside the Agent's state.
