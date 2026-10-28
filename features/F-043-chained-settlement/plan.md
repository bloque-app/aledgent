<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-043 — Chained settlement entries, no stored list

| | |
|---|---|
| Milestone | M-3 — Production V0 (settlement cost and completeness; a test book halted on the path this removes) |
| Track | B |
| Home | `contracts/settlement` (one release), `services/settle` (sync verification), `crates/ledger-core` (the re-scan path removed; incidents and gaps to the audit trail), `services/ingress` (the re-scan route withdrawn), `crates/verify`, `ops` |
| Depends on | `F-026` `T-026-10` (contract v2, `set_anchor_head`), `F-037` `T-037-19` (a stateful in-place upgrade of a running core) |
| Realises | `REQ-IN-5`, `REQ-SL-9`, `REQ-SL-10`, `REQ-SA-7`, `REQ-AT-1`, `AC-D2.1` as amendment 0040 restates them; `REQ-SL-5`, `REQ-PR-9`, `NFR-6`, `NFR-22` unchanged |
| Status | **In progress** (amendment 0040, `[PENDING AMENDMENT 0040]`; the owner ruled the design on 2026-10-26). The contract is built (`T-043-02`); nothing is deployed |

## Goal

The settlement contract stops storing an entry per movement. It keeps one head cell, `(position, digest, start)`,
where each entry's digest covers the one before it. The adapter checks completeness by reading that head at
each sync, and folds only the entries that are new since the head it last accepted. The core trusts that
check. The re-scan goes. Settlement incidents and gaps leave the book's state for the audit trail.

This is not a blockchain: it is one running digest in the contract's state, over entries Kreivo already orders
and finalises.

## Design summary (amendment 0040)

- `e_n = blake2b-256(SCALE(InboxEntry { at, event }))`, as today.
  `d_n = blake2b-256("aledgent/settlement-entry/v1" ‖ d_(n-1) ‖ n ‖ e_n)`, with `n` as 8 bytes big-endian. A chain
  starts at `(0, 32 zero bytes)`.
- A reset (`set_anchor_head`) is a full reset: it removes what v2 stored, sets `next_seq` and `pruned_to` to 0, and
  starts a new chain at position 0. A reset may come with a book re-founding, or under a running book, which then
  follows the new chain (amendment 0041, `[PENDING AMENDMENT 0041]`, section 7).
- `Deposited` gains `from` and `Refused` gains `call`. With those, every entry can be rebuilt from its event
  and the event's block number. No new event, and no digest in events.
- One release: `prune_inbox` and the stored entries go now, with no intermediate version.
- The core keeps one `u64`, the end of the highest gap, so `proof` (`ledger/settlement.rs:1689`) keeps its
  answer without the gaps list.

## Work, by component

### 1. The contract (`contracts/settlement`), built in `T-043-02`

- The head cell at `0x0000_000C`, `ChainHead { position, digest, start }`, and its read `chain_head()`
  (`0x0000_030A`). An absent cell reads as `(next_seq, zeros, next_seq)`.
- `append` folds the entry into the cell and stores nothing else; it traps if the cell is not at
  `Head.next_seq`. The cell is written in the frame that appends (`run_batch`, `isolate`), so a failed frame
  drops the write.
- `set_anchor_head` removes `[0, start)` (at most `MAX_RESET_CLEAR` = 64, `TooManyStoredEntries` otherwise),
  writes `Head { base, 0, 0 }`, clears the cell and the latest anchor, and records `AnchorEpoch.from_seq` = 0.
- Events: `from` on `Deposited`, `call` on `Refused`.
- Removed: `prune_inbox`, `inbox_entry`, the superseded-anchor removals, `InboxPruned`. The `inbox` and legacy
  `anchors` keys stay declared, and the legacy anchor migration stays as it was.
- `bound[asset]` is kept across a reset (`REQ-AS-8`); its value is a position in the chain it was bound in.
- `REFUSAL_RESERVE` from 0.6 G and 8 KiB to 0.75 G and 20 KiB: a dry-run refusal costs 0.13 G and 10.4 KB more
  than under v2 (it reads and writes the cell and hashes twice). To be re-measured on zombienet (`T-043-03`).
- `check_invariants` checks the cell against the head, that nothing is stored from `start` on, and that the
  events the contract emitted since its last reset fold to the cell (the mock records each event's emitter and
  block, as Kreivo does).

#### The reset's weight and refund (measured)

On a Chopsticks fork of Kreivo (spec 134, finalized #40,151,145) with a local build of this code
(before `MAX_RESET_CLEAR` went from 128 to 64 and `REFUSAL_RESERVE` was raised; the reset's path is otherwise the
same) installed on both instances, `set_anchor_head`
dry-run as governance (`ContractsApi.call`, `gas_required`), on the test instance with its stored entries
rewritten in the fork's storage to each count:

| Stored entries | Ref time | Proof size |
|---|---|---|
| 0 | 1.713 G | 185,728 B |
| 13 (test) | 3.885 G | 320,148 B |
| 17 (prod's count) | 4.552 G | 361,508 B |
| 128 | 23.089 G | 1,509,248 B |

That is **0.167 G and 10,340 B per removed entry**, linear; at the bound of 64, about 12.4 G and 0.85 MB, a third
of the 2.45 MB a normal extrinsic may prove. The storage deposit released goes to the call's origin
(`Contracts.StorageDepositTransferredAndReleased`, contract to origin): through the referendum's route,
`Communities.dispatch_as_account`, that is the community's account. Test: community 1200's account
(`F3opxRaMqPWKwA5yvdbzVhug5ZTGkeU6id6SRBWB9c15Sy3`, already governance), 93,684,332,889 planck (0.0937 KSM). Prod:
community 1201's account (`F3opxRaMqPWKwA5yvdsjXFVnaEejJkLqPZbzMsLdyUjMCLk`) once the admin names it,
113,750,666,134 planck (0.1138 KSM), from a `DryRunApi` run on the prod instance. Only a referendum of the
community moves funds out of its account. The referendum's route ran only as a `DryRunApi` call with the
`Communities{1200}` origin; the fork's scheduler enacted nothing, so the enactment itself is `T-043-03`'s.

### 2. Settle (`services/settle`)

- `Chain` gains a head read and an events read for a block range. The events read walks back by parent hash,
  because the light client has no lookup by number (`chain/rpc.rs:980-981`). It is implemented for rpc, the
  light client and `fake`. An optional history endpoint is used only to catch up.
- `settle_cursors` gains the accepted digest and block (a new migration).
- **Readers of the next position need no change.** After the full reset `Head.next_seq` is the chain's position,
  so the instance view (`chain/rpc.rs:583-599`), the cursor loop (`adapter.rs:2468`), the check at `:1845`,
  the positions cut (`:1866-1897`), the provisional range (`:2544-2545`), the head report (`:2588`) and, in
  `ops`, the reconcile cut (`ops/src/reconcile.rs:154`) and the cursor-lag signal (`ops/src/signals.rs:772`)
  read it as they do. What changes is the read of entries: `inbox_entry` (`chain/rpc.rs:609-631`, `kreivo.rs`
  `INBOX_ENTRY`) no longer exists, and the entries come from the events.
- The cursor kept for an instance from before its reset (test's is at 13) is moved back to the chain's start at the
  adapter's first start; a reset under a running adapter is reported to the core (section 7).
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

- `crates/verify` (it does not depend on the contract crate, so it builds unchanged):
  - `in_block_inbox` (`crates/verify/src/rpc.rs:334-358`) reads an anchor superseded in the same block from the
    stored entries at that block. Blocks before the upgrade still hold them in past state; for a block after
    it, the anchor is read from that block's `Anchored` events instead (`ContractReader` gains a read of a
    block's contract events).
  - `rpc.rs:540` assigns the anchor a sequence carried forward to the sequence before by position
    (`p.seq >= this.from_seq`). With `from_seq` 0 after every new reset, that test passes everything. The
    smallest exact test is `Some(p) != this.previous`: a sequence carries forward the anchor it received only
    when it stored none of its own. It holds for 0039's sequences and for these, and needs no block
    comparison, which a reset and an anchor in the same block would confuse.
- `ops`: the restore's re-scan step (`ops/src/restore.rs:373-376`, `ops/src/kit.rs:848`) becomes "resume from
  the accepted head, and catch up from the history endpoint if behind".
- The alert `AledgentRescanDifference` (`ops/prometheus/rules/aledgent.yml:38`) becomes
  `AledgentSettlementChainMismatch`. Every runbook that names the re-scan is rewritten:
  - `gap-filling.md`, `crank-outage.md` and `restore.md`;
  - under `release/`: `divergence.md`, `kreivo-finality-stall.md`, `settlement-adapter-outage.md` and
    `total-loss-recovery.md`;
  - the restore step in `features/F-018-operations/plan.md`.

### 6. The release gate's scripts

`contracts/settlement/scripts/drive.mjs` (`read`) and `e2e.mjs` (`check`, the faults that read the inbox) read
stored entries. They fold the events and compare with `chain_head` instead (`T-043-03`); until then the
zombienet gate fails against this code.

### 7. A new sequence under a running book (amendment 0041, `[PENDING AMENDMENT 0041]`)

The core keeps the highest head it has seen, so after a reset it drops every head of the new chain until that chain
passes the old position, and a withdrawal created meanwhile is born expired (test, 28 Oct: head 13 from the chain
before the reset). Amendment 0041 has the adapter report the reset, and the core follow it.

- **The core** (`crates/ledger-core`), from `SETTLEMENT_EPOCHS` (decision 1: equal to `CHAINED_SETTLEMENT`, 1):
  - a new adapter input, `AdapterInput::Epoch(SettlementEpoch)`, appended last: the sequence's number, base, first
    position, block, book and previous anchor, as `anchor_epoch()` holds them. Before the generation it is
    refused with `ERR-MalformedCommand` and writes nothing;
  - the answer `AdapterReply::Epoch(EpochOutcome)`: `Started(head)`, `Recorded` (the same sequence again), or
    `Refused(EpochRefusal)`, which is also one audit record (`Incident::EpochRefused`);
  - refused for another book (the book's chainspec digest; zero never matches), an earlier or conflicting
    sequence, an instruction in flight (the `live` index not empty), or a final record at or above `from_seq`
    observed at or before the reset's block;
  - otherwise one store write, `SettlementWrite::Epoch { epoch, head }`: the sequence (leaf kind 36, absent until
    set), and the head `{ at, from_seq }` unless the core holds a head at a later block. The store refuses a
    sequence that is not strictly later, and only this write moves the head back;
  - with a head, which the store requires to be the sequence's start, the same write clears the gap end (ruling q28, decision 6), and its undo restores it; a start
    that keeps a later head keeps it;
  - the tally and the index by position are left as they are (amendment 0041, "left as they are").
  - The input reads the `live` index and the final positions whole: its witness holds the whole `finals` map,
    as `proof`'s does. It comes once per reset.
- **Ingress:** the adapter's route `POST /v1/settlement/epochs` (one route per adapter input, as today), its body
  and OpenAPI schema, and the reply's `epoch` result.
- **Settle:**
  - `ChainEpoch` gains `base`, `book` and `previous`;
  - the walk counts the entries a reset drops;
  - on a live reset with none dropped, the adapter reports the start and moves its cursor to the new chain only
    once the core has taken it; with entries dropped, or for another book, `reset:` as today;
  - once per sequence, for the sequence its verified head carries, until the core confirms it (the confirmation is
    the report's stored verdict, `settle/epoch/<instance>/<epoch>`);
  - a refusal raises `epoch:<instance>:<epoch>`, which pages, and the start is reported again every ten minutes;
    a core before the generation raises the same alert, non-paging, and the adapter goes on.
- **Ops:** the alert `AledgentSettlementEpochRefused`; `gap-filling.md` "After a reset" and
  `release/settlement-adapter-outage.md` rewritten; the settle README.
- **Follow-ups:**
  - `GET /v1/ops/settlement/head`, read-only for the operator's principals: per instance, the head, the sequence and
    the head reported and not taken (`T-043-16`);
  - the guard (`T-043-17`): a head dropped for a lower position at a later block is recorded as reported and not
    taken (leaf kind 35); a later head or a start clears it; while it is set, `withdraw` and `submit_outbound`
    are refused with `ERR-SettlementLayerUnavailable`.

Tests:
- test's case as it stands: core head `{40,142,088, 13}`, new-chain anchors at 0 and 1 already final, then the
  start of sequence 2 (base 0, at 40,195,735), then heads from 2 taken at once, and a withdrawal right after with
  its first position at the head and an expiry above the finalised height;
- refusals: an instruction in flight, a final record of the earlier chain at 0, another book, an earlier
  sequence;
- a second start after an idle sequence is taken; one after a sequence with finals is refused;
- generation 0 refuses the input and the generation-0 fixtures replay unchanged (`f043_generations.rs`, the
  `code-history` gate);
- the ingress body round-trips; the adapter reports a live reset and a first-start sequence, and stops on a
  refusal.

## Test plan

- **Contract unit tests** (`contracts/settlement/src/tests.rs`), done in `T-043-02`:
  - the digest pinned to values computed outside the crate;
  - every entry kind, a failed frame and a credit, rebuilt from the events alone, fold to the head;
  - a failed frame drops its chain write at the sender and the receiving deployment;
  - the test and prod instances' cells, read live, written by raw key: unchanged byte for byte by the upgrade and
    by a message that appends nothing; an entry before the reset changes the head and the cell only; the full
    reset removes every stored entry and starts at `(0, zeros, 0)`; the first anchor after it is `d_0`;
  - the reset's bound; a reset keeps bindings;
  - `check_invariants` folds the events on every call, in every existing test.
- **Upgrade, rehearsed:**
  - zombienet: v2, entries, the upgrade, more entries (including a failing frame), a reset, and the fold equals
    the head at every step; the per-refusal weight against `REFUSAL_RESERVE`;
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

On test, then on prod. Each environment: the upgrade, then at once the full reset for the current book, then settle
and the core with `F-043`.

- **Test:** the code upgrade by the test instance's licence owner (community 1200), then the reset by governance
  (already named), before the next settlement there. Settle stays off until `T-043-04`. Done when two idle syncs
  read once each, and one sync with new entries folds them.
- **Prod (v2 since Root ref 63):**
  - the licence-owning community's referendum (1201) `publish_upgrade` + `upgrade`, on the code hash CI built;
  - the admin names governance (none is named);
  - then the reset as governance;
  - then settle, which has never run on `p2`, and the core.
- **Running books** take the core change in place (section 4).

## Not in this feature

A proof of one entry without the rest; a window of stored entries; moving anchors out of `latest_anchor`; the
Spaces venue (it inherits the rule through `REQ-SL-9`).
