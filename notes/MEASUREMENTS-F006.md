<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-006 — Leaf store measurements

**Status:** measured 2026-09-23 on a developer laptop, not the production host class. Informative.
Nothing here changes `SPEC.md` or `PLAN.md`.

**The `NFR-3` exit criterion at 150 million entries was not run.** The store takes about 1.46 KB
per entry on disk (leaf store and archive together, below), so a 150 M-entry book needs about
220 GB. The host had 15 GB free when the work began and under 5 GB by the end, shared with other
work. The runs below are at 100 000 and 1 000 000 entries, with the same 70% skew; the 150 M run is
left for the target machine, with the command below. What the smaller runs show, and what they do
not, is stated after the tables.

## Host and method

| | |
|---|---|
| Machine | MacBook Pro, Apple M2 Pro, 16 GiB, APFS SSD (nearly full: 3–15 GiB free during the runs) |
| Build | Rust 1.98.0 stable, `--release` |
| Engine | redb 4.3 (the Proposal engine), one file for the leaf store, one for the openings store |
| Command | `cargo run --release -p leaf-store --bin leaf-bench -- --entries N --accounts A --dir DIR` |

`leaf-bench` fills a file-backed `BookStore` through the native commit path (`LeafStore::commit`),
50 two-entry transfers per record, each record archived and fsynced before the store commits.
70% of the transfers move the hot account against itself, so 70% of all entries are on one
account. The book is synthetic: the store does not run the kernel, so entries carry placeholder
commitments; every account's balance opens with its row in the openings store, so balance reads
do the full check. The kernel's own cost is `F-002`'s (0.3 ms per posting natively).

Balance reads are the full read of `leaf_store::balance::read_balance`: a view of each store, the
account leaf (re-hashed and checked), its opening (checked against the leaf's two commitments —
two scalar multiplications, most of the time), and the reserved total. Warm page cache; the host
cannot drop it without privileges.

## Results

### 100 000 entries, 10 000 accounts

| | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|
| Balance read, hot account (70 152 entries) | 89.5 µs | 98.6 µs | 107.0 µs | 134.5 µs |
| Balance read, uniform account | 90.8 µs | 95.0 µs | 101.0 µs | 193.5 µs |
| Witness build (transfer read set: loads + six frontiers, 57–65 nodes) | 61.7 µs | | 75.3 µs | |
| Commit per record of 50 transfers, archive fsynced | 11.8 ms | | 15.4 ms | 18.9 ms |

Fill: 7 669 entries/s. Rebuild from genesis: 1 101 records in 7 s (12 944 entries/s), roots
equal. On disk: 1 728 bytes per entry (leaf store 86 MB in use, archive 36 MB).

### 1 000 000 entries, 100 000 accounts

| | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|
| Balance read, hot account (701 002 entries) | 89.7 µs | 97.5 µs | 104.5 µs | 387.5 µs |
| Balance read, uniform account | 98.8 µs | 303.1 µs | 354.4 µs | 4 900.2 µs |
| Witness build (71–77 frontier nodes) | 275.5 µs | | 797.3 µs | |
| Commit per record of 50 transfers, archive fsynced | 15.7 ms | | 57.1 ms | 840.5 ms |

Fill: 4 438 entries/s (225 s). Rebuild from genesis: 11 001 records in 175 s (5 712 entries/s),
roots equal. On disk: 1 459 bytes per entry.

## Reading

- **`NFR-3` at these sizes: met with a wide margin.** p99 is 0.10 ms for the account holding 70%
  of the book and 0.35 ms for a uniform account, against 5 ms. The hot account reads exactly as
  fast at 700 000 entries as at 70 000: a balance read never touches history. The uniform p99
  grows from 0.10 ms to 0.35 ms between the two sizes, and one read in 5 000 took 4.9 ms: that is
  the page cache (100 000 distinct accounts, a nearly full SSD shared with other work), not the
  read path, which is three point reads at any size.
- **What 150 M entries would change is B-tree depth and cache misses**, not the number of reads.
  At 150 M entries a cold point read in redb is a handful of page reads; whether three of them
  plus the opening check stay under 5 ms at p99 on the target machine is **not measured here**
  and should not be assumed. It is the run to do first on the production host class.
- **Apply cost is flat in the book's size, in engine work.** The store's work per record grows
  with tree depth only (`log₂` of the leaves); the unit test
  `nfr_1_apply_cost_does_not_grow_with_the_book` pins reads and writes per apply at 10 000
  postings within 2× of 100, and equal for a hot and a cold account. Wall time per record here is
  dominated by the archive's fsync (about 10 ms on this SSD) and by the disk being nearly full:
  the 1 M run's p99 (57 ms) and max (840 ms) are the SSD's.
- **Rebuild throughput** (`NFR-10`'s drill): 5 700–13 000 entries per second single-threaded
  here, so a 150 M-entry book would take roughly 3–7 hours on this laptop. Verified snapshots at
  checkpoints (plan Proposal) would bound restore time; the exit test still rebuilds from
  genesis.
- **Size.** About 1.5 KB per entry on disk, of which the archive is about 30%. A two-entry
  posting adds two leaves (the transfer and its external id), so that is about 1.5 KB per leaf
  with history, indexes and the archive included — about 12× less than `FileStore`'s 18 KB per
  leaf (`F-002` §6), because only branching nodes are stored. It is still not small: 150 M entries is about 220 GB. The rkyv transfer archive (~300 B for two
  entries) and redb's page overhead dominate. Compaction and a leaner transfer encoding are
  `F-018`'s levers, and the engine choice before `M-3` (plan: redb vs RocksDB) should weigh it.
- **Witness** build time grows with the book (62 µs → 276 µs at p50) through page-cache misses on
  frontier reads; the node count grows as `log₂` (57 → 71). `F-002` measured the witness's size
  (5.4–5.9 KB at 1 M accounts) and its verification (0.83–1.33 ms natively); the store's build
  adds well under 1 ms.
