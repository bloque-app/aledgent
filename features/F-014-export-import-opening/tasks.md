<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-014 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-014-01 | Export format: manifest and key-sorted canonical sections, with no blinding, opening or key (Proposal) | `REQ-RC-3`, `REQ-RT-6`, `REQ-AU-8` | Two exports of the same state are byte-identical. A scan finds no secret bytes |
| T-014-02 | Export at one pinned sequence, taken right after an anchored checkpoint (Proposal) | `REQ-RC-3`, `REQ-PR-7`, `INV-19` | The manifest root equals the anchored root for that sequence |
| T-014-03 | Import into an empty deployment: verify digests, load, recompute both roots, set counters, audit | `REQ-RC-3`, `REQ-RT-8`, `REQ-AT-1` | Non-empty target, bad digest and root mismatch are each refused, leaving the target empty |
| T-014-04 | Observable-state comparison harness over every §8.2 query and both roots | `REQ-RC-3`, `US-K5` | `AC-K5.2` passes on a random book |
| T-014-05 | Re-derivation after restore: entry commitments vs stored balances, openings vs commitments, acknowledged operations present | `INV-6`, `NFR-4`, `US-K5` | `AC-K5.1` passes. Seeded corruptions of a balance, an entry and an opening are each caught |
| T-014-06 | Opening position per asset and the `Opening` posting with a kernel `external_id` per (account, asset) | `REQ-RC-4`, `REQ-KER-6`, `INV-5`, `INV-7` | A second opening for a pair is refused at the kernel. Trial balance stays zero |
| T-014-07 | `OpeningSet`, `Reconciliation` and the zero-divergence gate, recomputed by the library | `REQ-RC-4`, `NFR-9`, `REQ-AS-4` | Every seeded divergence (one minor unit, missing row, extra row, missing source, excess precision) blocks the set |
| T-014-08 | `load_opening_balances` through `F-013`'s frame: gate, batched atomic chains, resumable, completion check | `REQ-RC-4`, `REQ-KER-10`, `REQ-AT-1`, `REQ-OS-1` | An interrupted load resumes with no double posting. Completion requires totals to match |
| T-014-09 | Scale run: export, import and compare at 1M entries; opening load of 500k rows | `REQ-RC-3`, `REQ-RC-4`, `NFR-4` | Exit criterion met. Times recorded in `notes/` |
