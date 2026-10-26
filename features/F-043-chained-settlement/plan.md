<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-043 — Chained settlement entries, no stored list

| | |
|---|---|
| Milestone | M-3 — Production V0 (settlement cost and completeness; a test book halted on the path this removes) |
| Track | B |
| Home | `contracts/settlement` (v3, then v4), `services/settle` (sync verification), `crates/ledger-core` (the re-scan path removed; incidents and gaps to the audit trail), `services/ingress` (the re-scan route withdrawn), `crates/verify`, `ops` |
| Depends on | `F-026` `T-026-10` (contract v2, `set_anchor_head`), `F-037` `T-037-19` (a stateful in-place upgrade of a running core) |
| Realises | `REQ-IN-5`, `REQ-SL-9`, `REQ-SL-10`, `REQ-SA-7`, `REQ-AT-1`, `AC-D2.1` as amendment 0040 restates them; `REQ-SL-5`, `REQ-PR-9`, `NFR-6`, `NFR-22` unchanged |
| Status | **Proposed** (amendment 0040, `[PENDING AMENDMENT 0040]`; the owner ruled the design on 2026-10-26). Docs only in this pull request |

## Goal

The settlement contract stops storing an entry per movement. It keeps one head cell, `(position, digest)`,
where each entry's digest covers the one before it. The adapter checks completeness by reading that head at
each sync, and folds only the entries that are new since the head it last accepted. The core trusts that
check. The re-scan goes. Settlement incidents and gaps leave the book's state for the audit trail.

This is not a blockchain: it is one running digest in the contract's state, over entries Kreivo already orders
and finalises.

## Design summary (amendment 0040)

- `e_n = blake2b-256(SCALE(InboxEntry { at, event }))`, as today. `d_n = blake2b-256(d_(n-1) ‖ n ‖ e_n)`, with `n`
  as 8 bytes big-endian. A chain starts at `(0, 32 zero bytes)`.
- A reset (`set_anchor_head`) starts a new chain at position 0. Resets come only with a book re-founding.
- `Deposited` gains `from` and `Refused` gains `call`. With those, every entry can be rebuilt from its event
  and the event's block number. No new event, and no digest in events.
- v3 keeps `prune_inbox` for the entries v2 stored. v4 removes it and the storage v3 no longer uses.
- The core keeps one `u64`, the end of the highest gap, so `proof` (`ledger/settlement.rs:1689`) keeps its
  answer without the gaps list.

## Work, by component

### 1. Contract v3 (`contracts/settlement`)

- A head cell at `0x0000_000C` and a read-only message for it. An absent cell reads as `(0, zeros)`.
- `append` (`lib.rs:834-846`) folds the entry into the cell and stops writing `inbox`. The cell is read and
  written in storage on every append, so a frame that is rolled back (`run_batch`, `isolate`) drops the write.
- `set_anchor_head` (`lib.rs:582-641`) writes `(0, zeros)`, and `AnchorEpoch.from_seq` = 0.
- Events: `from` on `Deposited` (`lib.rs:149-160`), `call` on `Refused` (`lib.rs:211-222`).
- Positions restart, so:
  - `Head.next_seq` stays where v2 left it, as the end of what v2 stored;
  - `Outcome`, `LatestAnchor.seq` and `bound[asset]` take chain positions;
  - the removals of a superseded anchor's stored entry (`lib.rs:618`, `:1001`) go;
  - the overflow checks on `Head.next_seq` (`lib.rs:398`, `:431`, `:870`) move to the chain position.
- `prune_inbox` works only below the frozen `Head.next_seq`. It no longer spares the latest anchor's position or
  bind entries (amendment 0040's addition, not a ruling), so the stored map can be emptied for v4.
- `check_invariants` (`lib.rs:1578`) is rewritten for the chain: nothing is stored at chain positions, and the
  head equals a fold of the test's recorded events.

### 2. Settle (`services/settle`)

- `Chain` gains a head read and an events read for a block range. The events read walks back by parent hash,
  because the light client has no lookup by number (`chain/rpc.rs:980-981`). It is implemented for rpc, the
  light client and `fake`. An optional history endpoint is used only to catch up.
- `settle_cursors` gains the accepted digest and block (a new migration).
- **Every reader of the next position uses the chain's.** The instance view takes `next_seq` from the head cell
  rather than `head()` (`chain/rpc.rs:583-599`). That covers the cursor loop (`adapter.rs:2468`), the check at
  `:1845`, the positions cut (`:1866-1897`), the provisional range (`:2544-2545`) and the head report
  (`:2588`), whose `next_seq` becomes the core's `FinalisedHead` (`HeadBody`,
  `services/ingress/src/settlement.rs:558-567`), from which `proof` builds its range. In `ops`, the reconcile
  cut (`ops/src/reconcile.rs:154`) and the cursor-lag signal (`ops/src/signals.rs:772`) change with it.
- `observe` (`adapter.rs:2403`):
  - the read by position becomes: read the head; done if unchanged; otherwise fold the new entries from the
    accepted digest and compare;
  - on a match, report in order and accept the head after the core has taken in every entry;
  - on a mismatch, read once more, then alert and stop at the first new position;
  - the provisional check compares against events above finality.
- The first start for a book is `(0, zeros)`: just after the reset's `AnchorHeadSet` event, or at
  instantiation for an instance never reset (`anchor_epoch()` is `None`).
- **The operator's gap** (a40-q3): when no reader can serve a range, the operator has the adapter record it
  through its own authoritative source (`record_gap`, `ledger/settlement.rs:1830`, refuses any other), and
  the adapter then accepts the layer's head as it stands, unverified for that range. The adapter's own
  `Pruned` gap (`adapter.rs:2531`) goes, since nothing is pruned any more.
- **Removed:**
  - `Adapter::rescan` (`adapter.rs:3388`) and its timer (`:1161-1166`);
  - `rescan_every` and `rescan_window` (`config.rs:103-106`), accepted with a warning for one release, then refused;
  - `settle.json:29-30`.

### 3. The core (`crates/ledger-core`) and ingress

- **Removed:**
  - `Ledger::rescan` (`ledger/settlement.rs:1857-1938`) and the route `POST /v1/settlement/rescans`
    (`services/ingress/src/surface.rs:625`, `settlement.rs:870`, `:1474`);
  - `Incident::Missing` and `Incident::Unknown`;
  - the indexes `RESCAN_RECORDED` and `INGESTED_AT` (`settlement/store.rs:258`, `:273`).
  - `Call::Rescan` stays decodable (`call.rs:260-266`), and the new code refuses it.
- **Incidents and gaps go to the audit trail.** Each is written as an audit record in the batch that would have
  written the leaf, under a new audit class (`audit/mod.rs:58-68` has none for them). The leaves `gaps` and
  `incidents` (`store.rs:245-246`) go. `RecordGap` keeps its input and also sets the gap end.
- **`proof`** tests `from_seq < gap end` in place of `gap_within` (`store.rs:957`).
- The export records (`export/records.rs`, `settlement/store/canon.rs`) change with the leaves.
- The sim driver (`settlement/driver.rs:310-313`) keeps its own list of gaps.

### 4. The core change for running books

- An `F-037` in-place upgrade with a stateful migration (`T-037-19`). It writes the existing incidents and gaps
  as audit records, sets the gap end from the gaps, and drops the leaves.
- The leaf kinds are keys in the actor's one map of rows (`agents/core/src/lib.rs:68-73`), so the runtime's
  state-layout check passes (`agents/core-host/src/switch.rs:373-375`).
- It needs the release gate that replays the previous generation's state under the new core, which
  `T-037-19` names as still to be ruled.

### 5. Readers and operations

- `crates/verify`:
  - it reads an anchor superseded in the same block from that block's `Anchored` events, no longer from the
    stored entries (`in_block_inbox`, `crates/verify/src/rpc.rs:334-358`);
  - it assigns a carried anchor to a sequence by inbox position (`p.seq >= this.from_seq`, `rpc.rs:540`). With
    positions restarting and `from_seq` always 0, that compares blocks instead (`LatestAnchor.at` against
    `AnchorEpoch.at`).
- `ops`: the restore's re-scan step (`ops/src/restore.rs:373-376`, `ops/src/kit.rs:848`) becomes "resume from
  the accepted head, and catch up from the history endpoint if behind".
- The alert `AledgentRescanDifference` (`ops/prometheus/rules/aledgent.yml:38`) becomes
  `AledgentSettlementChainMismatch`. Every runbook that names the re-scan is rewritten:
  - `gap-filling.md`, `crank-outage.md` and `restore.md`;
  - under `release/`: `divergence.md`, `kreivo-finality-stall.md`, `settlement-adapter-outage.md` and
    `total-loss-recovery.md`;
  - the restore step in `features/F-018-operations/plan.md`.

### 6. Contract v4 (later)

- Removes `prune_inbox` and the storage v3 no longer uses: `inbox`, and the legacy `anchors` and
  `anchor_migration`.
- Ships once every instance has pruned its v2 entries.

## Test plan

- **Contract unit tests** (`contracts/settlement/src/tests.rs`):
  - the head after every kind of entry, against an independent fold in the test;
  - a frame that fails leaves the head as before the frame, and the refusal extends it;
  - `deposit` and `credit` extend the chain;
  - the v2 layout written by raw key is byte-identical after the first v3 message;
  - a reset writes `(0, zeros)`;
  - `prune_inbox` refuses at or above the frozen end, and empties the range including bind entries;
  - no stored entry is written.
- **Events rebuild entries:**
  - for each kind, an `InboxEntry` built from the event and its block hashes to the same `e_n` the v2 code
    stores;
  - on a local Kreivo (zombienet), the events of a failed frame are gone with it.
- **Upgrade, rehearsed:**
  - zombienet: v2, entries, v3, more entries (including a failing frame), a reset, and the fold equals the
    head at every step;
  - Chopsticks forks of test and prod: `publish_upgrade`, `upgrade` and `set_anchor_head` dry-run with the
    origin each requires.
- **Settle** (`services/settle/tests`: `flow.rs`, `contract.rs`, `anchoring.rs`):
  - an idle sync is one read;
  - a burst is folded once;
  - an entry hidden, added or altered by a reader stub stops the adapter at the first new position, with an
    alert;
  - behind more than 256 blocks: unreadable without a history endpoint, caught up with one;
  - after a reset, no entry of the old chain is reported.
- **Core:**
  - `Call::Rescan` refused by the new code and replayed by the old;
  - an incident and a gap each produce an audit record and no leaf;
  - `proof` gives the same answers with the gap end as with the list;
  - the migration of a book with incidents and gaps;
  - `rescan_incidents.rs` and the re-scan cases of conformance (`f010`, `f016`) are replaced by chain cases.

## Rollout

On test, then on prod. Each environment: the v3 upgrade, then the head reset for the current book, then settle
and the core with `F-043`.

- **Test:** the code upgrade is made by the test instance's licence owner. Settle stays off until v3
  (a40b-test-now). Done when two idle syncs read once each, and one sync with new entries folds them.
- **Prod (v2 since Root ref 63):**
  - the licence-owning community's referendum `publish_upgrade` + `upgrade` to v3, on the code hash CI built;
  - then the reset as governance;
  - then settle, which has never run on `p2`, and the core.
- **Running books** take the core change in place (section 4).

## Not in this feature

A proof of one entry without the rest; a window of stored entries; moving anchors out of `latest_anchor`; the
Spaces venue (it inherits the rule through `REQ-SL-9`).
