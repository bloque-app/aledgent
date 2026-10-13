<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-014 — Export, import and opening-load measurements

**Status:** measured 2026-09-23 on a developer laptop, not the production host class. Informative.
Nothing here changes `SPEC.md` or `PLAN.md`.

**The exit criterion's 1 M-entry run was not run; 800 000 entries were.** The host's disk was shared
with other builds and this run was held to 3 GB of generated data. A book on `F-006`'s leaf store
takes about 1.86 KB per entry, and an import writes a second book of about 1.64 KB per entry, so a
1 M-entry export and re-import needs about 3.5 GB at once: over the limit. 800 000 entries need
2.8 GB. The opening load of 500 000 rows (1 M entries) was run on its own, after the first two books
were deleted, in 2.65 GB. Every dataset was deleted once measured. The 1 M-entry figures below are
extrapolated linearly and marked so; nothing in the run grew faster than linearly.

## Host and method

| | |
|---|---|
| Machine | MacBook Pro, Apple M2 Pro, 16 GiB, APFS SSD, shared with other builds and nearly out of swap |
| Build | Rust 1.98.0 stable, `--release` |
| Command | `cargo run --release -p conformance --bin f014-scale -- --entries 800000 --accounts 10000 --openings 500000 --dir DIR` |
| Leaf store | `F-006`'s `BookStore` on redb, on disk; archive **without fsync**, store commits `Visible` |
| Openings store | `F-006`'s `OpeningsStore`, in memory |

The book is built through the command surface only (`conformance::tools::book`): 2 000 random
commands of every kind (so every section of the export has records), then 10 000 funded accounts
and multi-leg transfers of ten legs among them until 800 001 kernel entries (399 978 postings). The
archive's fsync is off: `F-006` measured it at about 10 ms per commit on this disk, and it is not
what this feature adds. Stated plainly, a durable fill and a durable import are slower by roughly
that per commit (9 422 commits for this import).

## Results: 800 000 entries

| Phase | Time | Notes |
|---|---:|---|
| Fill | 385.8 s | 2 073 entries/s through the commands; the kernel's proofs and signatures dominate |
| Source store on disk | | 1 486 MB (1 857 B per entry, archive included) |
| Checkpoint (book root) | 0.5 s | encodes the whole service store for its root |
| Export | 17.9 s | the privileged read, the fold of the delta log, the leaves' root recomputed, every section |
| Export size | | 352 MB (440 B per entry); to bytes and back 0.3 s |
| Import | 91.8 s | every check, then 9 422 commits of ≤ 24 KiB; target 1 315 MB on disk |
| Compare (`AC-K5.2`) | 50.6 s | 471 746 queries, 1 599 972 movements paged, **0 differences** |
| Re-derive (`AC-K5.1`) | 22.2 s | entries against balances, openings against commitments, every acknowledged operation present |
| Second export of the import | 16.6 s | every section and both roots byte-identical |
| Peak memory | | 4.4 GB footprint, 1.66 GB resident (two books' service stores and the export in memory) |

Largest sections: kernel transfers 205 MB (399 978 records), service journal 44 MB, operations
38 MB, attribution records 30 MB, external ids 14 MB.

## Results: opening load of 500 000 rows

| Phase | Time | Notes |
|---|---:|---|
| 250 000 accounts opened | 3.0 s | delegated custody (see below) |
| Set and reconciliation built | 0.5 s | two sources for USD, one for EUR |
| Load | 549.2 s | 910 rows/s: 50 000 batches of 10, one linked chain and one commit each; gate, completion and `INV-5` checked |
| Book on disk | | 2 651 MB |

## 1 M entries, extrapolated (not measured)

| Phase | Extrapolated |
|---|---:|
| Fill | ~480 s |
| Export | ~22 s |
| Import | ~115 s |
| Compare | ~63 s |
| Re-derive | ~28 s |
| Disk, both books | ~3.5 GB |

## Reading

- **`AC-K5.1` and `AC-K5.2` hold at 800 000 entries**, in a plain process, with the export crossing
  as bytes. `REQ-RC-3` "at production volume" (`REQ-RT-8`) is not shown by this: the exit criterion
  asks for 1 M, a real book may be far larger, and an upgrade must not rely on this run.
- **Import costs about five times export**, almost all of it the leaf store's commits: one commit per
  24 KiB of leaves, each re-hashing its paths. A bulk-load path in the store (build the trees from
  sorted leaves, as `composite_state_root` does) would cut it; that is `F-006`'s lever, not this
  feature's.
- **The service store is in memory** in `M-1` (`F-005`, `F-006` give it its durable home), so the
  export and the import hold it, and the export, in memory: 1.66 GB resident here. At production
  volume the export must stream section by section; the format allows it (each section is a count
  and a stream of length-prefixed records), the in-process API does not yet.
- **The opening load is bound by the kernel**: 910 rows/s is one posting per row through the kernel's
  proofs, in chains of ten. A 500 000-row opening set loads in under ten minutes on this laptop.
- **Owned-custody accounts were not used for the 250 000 opening accounts**: opening an owned
  account lists the owner's recovery credentials by scanning every credential of the book
  (`F-012`'s `State::credentials_of`), which is quadratic over a quarter of a million openings. A
  first debug run, a third of its accounts owned, spent 21 minutes opening them. A follow-up to
  index credentials by identity is proposed.
- **A first version of the load was quadratic** (it re-scanned the set for pending rows before each
  batch); found by this run and fixed before these numbers.
