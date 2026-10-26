<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-043 — Chained settlement entries, no stored list

| | |
|---|---|
| Milestone | M-3 — Production V0 (settlement cost and completeness; a test book halted on the path this removes) |
| Track | B |
| Home | `contracts/settlement` (v3), `services/settle` (sync verification), `crates/ledger-core` (the chain record; the re-scan path and the completeness incidents removed), `services/ingress` (the head report; the re-scan route withdrawn), `crates/verify`, `ops` (runbooks, alerts), `deploy/templates/settle.json` |
| Depends on | `F-026` `T-026-10` (contract v2, `set_anchor_head`), `F-037` (an upgrade of a running core; `T-037-19` for a stateful migration) |
| Unblocks | #74 items 1 to 3; `T-036-15` (the adapter reads its first position from the contract) |
| Realises | `REQ-IN-5`, `REQ-SL-9`, `REQ-SL-10`, `REQ-SA-7`, `AC-D2.1`, `AC-D2.2` as amendment 0040 restates them; `REQ-SL-5`, `REQ-PR-9`, `REQ-AT-3`, `NFR-22` unchanged |
| Status | **Proposed** (amendment 0040, `[PENDING AMENDMENT 0040]`, draft). Docs only in this pull request. Nothing is built until the owner rules the open questions |

## Goal

The settlement contract stops storing an entry per movement. Each entry's digest includes the digest of the
entry before it, and the contract keeps only the latest digest, its position and the book it belongs to. The
adapter checks completeness at each sync by reading that one head, and it recomputes digests only for entries
that are new since the head it last accepted. The core keeps one fixed-size record of where the chain stands,
or where it broke, in place of its growing list of re-scan incidents. The re-scan timer, the re-scan input and
the completeness incidents go away.

This is not a blockchain. It is one running digest in the contract's state. The settlement layer already
orders and finalizes entries.

## Design summary

The rule set is amendment 0040. In short:

- `e_n = blake2b-256(SCALE(InboxEntry))`, unchanged and already the coordinate's digest.
- `d_n = blake2b-256("aledgent/settlement-entry/v1" ‖ d_(n-1) ‖ n_be ‖ e_n)`.
- The genesis is `blake2b-256("aledgent/settlement-genesis/v1" ‖ contract account ‖ epoch ‖ book ‖ from_seq)`.
  A reset starts a new chain.
- The contract stores `EntryHead { epoch, book, from_seq, next_seq, digest }` at `0x0000_000C` and emits
  `Entry { seq, entry }` per entry. It stores no entry.
- The adapter, at each step:
  - reads the head;
  - is done if the head equals the one it accepted;
  - otherwise verifies only the new entries from the `Entry` events of the blocks since it last read;
  - accepts the new head only after the core has taken each entry in;
  - on a mismatch, stops at `n` and reports a break.
- The core keeps `SettlementChain { epoch, book, from_seq, seq, digest, break }`, one leaf per instance. It
  recomputes over only the newly final positions on each head report (amendment 0040, Q1).
- An archive node is needed only to catch up past what an ordinary node keeps (about 256 blocks), or to restore.
  Routine settlement never needs one.

## Work, by component

### 1. Contract v3 (`contracts/settlement`)

- `append` (`lib.rs:834-846`) computes `e_n` and `d_n`, writes `entry_head`, emits `Entry`, and stops writing
  `inbox`.
  - It reads and writes `entry_head` in storage every time, so a frame that is rolled back takes the write with
    it (`run_batch`, `isolate`, `lib.rs:849-952`).
  - `Head` and `apply` do not change.
- Lazy genesis on the first `append` with no `entry_head`, and a new genesis in `set_anchor_head`
  (`lib.rs:581-643`). Event `EntryChainStarted`.
- A new read-only message `entry_head()`. `inbox_entry(seq)` stays for entries written before v3.
- `prune_inbox` limited to positions below the chain's `from_seq` (amendment 0040, Q7).
- `check_invariants` (`lib.rs:1578`):
  - `entry_head.next_seq == head.next_seq`;
  - no stored entry at or above `from_seq`;
  - the digest recomputed from the test's recorded events equals the head.
- The per-kind events stay unless Q5 rules otherwise.

### 2. Settle (`services/settle`)

- `Chain` gains `entry_head(at)` and `entries(from_block, to_block)`, which reads the `Entry` events and walks
  back by parent hash, because the light client has no lookup by number (`chain/rpc.rs:980-981`).
  - It is implemented for `rpc`, the light client and `fake`.
  - It is configured with an optional `history_endpoint` (an RPC of a node that keeps history), which is used
    only to catch up.
- The store keeps the accepted head beside the cursor: `settle_cursors` gains `digest` and `block`, in a new
  migration.
- `observe` (`adapter.rs:2403-2560`):
  - the cursor read by position becomes: read the head, compare it, verify the new entries, then report them in
    order, as now;
  - the provisional check (`adapter.rs:2430-2442`) compares against the verified range;
  - the read by position is kept only for the range below the chain's `from_seq` that v2 stored.
- **The re-scan is removed:**
  - `Adapter::rescan` (`adapter.rs:3388-3478`);
  - the step's timer (`adapter.rs:1161-1166`);
  - `rescan_every` and `rescan_window` (`config.rs:103-106`). Kept as accepted and ignored for one release,
    with a warning, then refused;
  - `settle.json:29-30`.
- On a break: re-read once (from the history node, if configured), then report the break, raise
  `chain:{instance}:{n}`, and stop the read at `n`. Anchoring goes on (`REQ-PR-9`).
- On first start against v3: the cursor is the chain's `from_seq`, read from `entry_head` (`T-036-15`).

### 3. The core (`crates/ledger-core`) and ingress (`services/ingress`)

- `SettlementChain` leaf per instance in `SettlementStore` (`settlement/store.rs`). `HeadBody`
  (`ingress/src/settlement.rs:558-567`) gains `digest`.
- On a head report:
  - recompute over `[seq, reported)` from `final_at`, with a cap per input in the manner of `T-032-19`;
  - a position that is not yet final means wait;
  - a different digest means `Break`;
  - the first head is accepted only if its genesis names this book (0039's q1);
  - observations below `from_seq` are refused.
- **Clearing a break:** a re-read that verifies, or an operator intervention with its own authority and a
  justification (`REQ-AT-3`). An audit record either way.
- **Remove:**
  - `Ledger::rescan` (`ledger/settlement.rs:1856-1930`);
  - `Incident::Missing`, `Incident::Unknown` and `Incident::Gap`, for new writes;
  - the ingress route `POST /v1/settlement/rescans` (`ingress/src/surface.rs:625`, `settlement.rs:870`,
    `:1474`).
  - `Call::Rescan` stays decodable (`call.rs:260-266`), and the new code refuses it.
- **The incidents that remain** move to keyed leaves (Q2, `T-032-26`).
- The open break and the counts of the remaining incidents appear on `/v1/ops/signals` (#74 item 3).
- The state layout changes:
  - incidents (kind 51) and gaps (Q12) move, and `RESCAN_RECORDED` goes if #73 merged;
  - export records (`export/records.rs`) and the state image (`state/image.rs:297-306`) change with them;
  - a running book takes the change through `F-037`, with `T-037-19`, or at a re-founding (Q11).

### 4. Readers and operations

- `crates/verify`: the superseded anchor in a reset's own block is read from that block's `Entry` events, no
  longer from the stored inbox (amendment 0039, "The verifier").
- Runbooks that name the re-scan:
  - `ops/runbooks/release/divergence.md` (`AledgentRescanDifference`);
  - `gap-filling.md`, `crank-outage.md`, `restore.md`, `release/settlement-adapter-outage.md` and
    `release/total-loss-recovery.md`.
  - The alert becomes `AledgentSettlementChainBroken`.
- `features/F-018-operations/plan.md`: an adapter restore no longer "re-scans the inbox". It resumes from the
  accepted head, and catches up from the history node if it is behind.

## Test plan

**Contract (unit, then a local Kreivo).**

- Unit tests (`contracts/settlement/src/tests.rs`):
  - the digest after every message that appends, against an independent implementation in the test;
  - a frame that fails (a withdrawal to another deployment that traps) leaves `entry_head` as it was before
    the frame, and the refusal's entry extends it;
  - `deposit` and `credit` extend the chain;
  - lazy genesis on the v2 layout written by raw key, with every existing cell byte-identical afterwards;
  - `set_anchor_head` starts a new genesis, and an old digest does not extend it;
  - `prune_inbox` refuses any position at or above `from_seq`;
  - no stored entry is written.
- **zombienet** (the `F-026` suite) or **Chopsticks** (a fork of a live network, for the upgrade path):
  - instantiate v2;
  - write entries;
  - `publish_upgrade` and `upgrade` to v3;
  - write more entries, including a batch with a failing frame;
  - read the head and the events, and recompute;
  - reset, and check the new chain.
  - On Chopsticks, also dry-run the exact upgrade calls with the origin each one requires (licence owner,
    governance) against a fork of each live instance.

**The adapter against a forked chain.**

- **Chopsticks forks of the test and prod networks with v3 upgraded in.**
  - The adapter syncs: one read when idle.
  - A burst of deposits is verified once.
  - An entry hidden by a reader stub, an extra one, and an altered one each give "broken at `n`" at the next
    sync, with no advance past `n`.
  - The adapter is stopped for more than 256 blocks with no history endpoint: unreadable, an alert, never
    "missing".
  - The same with a history endpoint: it catches up.
  - A reset: the new book's adapter starts at `from_seq`, and no old entry is reported.
- `services/settle/tests`: `flow.rs`, `contract.rs` and `anchoring.rs` move to the head read, and the re-scan
  cases become chain cases.

**The core.**

- `ledger-core` tests:
  - a head that extends the record;
  - a position not yet final (a wait);
  - a different coordinate (a break at `n`);
  - a first head of another book (refused);
  - an observation below `from_seq` (refused);
  - a break cleared by a re-read, and by an operator with an audit record;
  - `Call::Rescan` refused by the new code and replayed by the old.
- `core-binding` heap test:
  - 10,000 entries over many head reports keep the guest's arena flat. This replaces `rescan_incidents.rs`,
    which #73 added.
- Conformance:
  - `f010`, `f013`, `f016` and `f017` re-scan cases become chain cases;
  - `AC-D2.1` and `AC-D2.2` as restated.
- `ops/tests/restore.rs`: a restored adapter resumes from the accepted head.

## Rollout

1. **Test** (fast track).
   - The contract's admin is a key, so the admin's calls are each one signed call.
   - The code upgrade is made by the test instance's licence owner on its fast track.
   - Then settle and the core with `F-043`, on the re-founded test book.
   - Watch two syncs that are idle (one read each) and one that is not.
2. **Prod, after the Root referendum enacts.** That referendum moves prod to v2 and gives the licence-owning
   community the default track.
   - The community's referendum: `publish_upgrade` and `upgrade` to v3, on the code hash CI built.
   - If prod's book is not yet reset, its reset is made under v3 (amendment 0040, Q8).
   - Then settle and the core with `F-043`.
   - Until then, prod's settle stays off, or runs with `rescan_every: 0`, and #73 is the stopgap.

## Not in this feature

A Merkle proof of one entry; a stored entry window; moving the anchors (they stay in `latest_anchor`); the
Spaces venue (it inherits the rule through `REQ-SL-9`).
