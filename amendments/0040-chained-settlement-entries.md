<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0040 — Chained settlement entries: the contract keeps one head, not a list

**Status:** **draft**, for the owner's review. Nothing here is applied to `SPEC.md`; the plan and the code cite it
as `[PENDING AMENDMENT 0040]`. The design is the owner's (2026-10-26); the open questions at the end are his to
rule.
**Target:** `SPEC.md` v0.4.29 as on `develop` (at `15ba1c5`); `PLAN.md` `DEC-17`; `F-016`, `F-026`, `F-027`;
amendments 0029 and 0039 (draft).
**Date:** 2026-10-26
**Number:** 0039 is the last on `develop`; no open PR or remote branch claims 0040.
**Amends:** `REQ-IN-5` (restated), `REQ-SL-9` (restated), `REQ-SL-10` (one phrase), `REQ-SA-7` (one sentence),
`AC-D2.1` and `AC-D2.2` (restated). No command, query, error condition or invariant changes. `NFR-22` and
`REQ-PR-1` to `REQ-PR-11` are unchanged.
**Replaces:** the core's list of settlement incidents for completeness (`Missing`, `Unknown`, `Gap`), the
re-scan input and the adapter's re-scan timer. The interim de-duplication of #73 is a stopgap that this design
replaces. #74 is answered item by item below.
**Rationale in one line:** an entry stored for every movement costs a deposit forever, and re-checking stored
entries on a timer grew a list that halted a test book. If each entry commits to the one before, one stored
head proves that every entry before it is there and unchanged, so the contract can drop the list. A sync then
checks completeness by reading that head, with no re-scan.

---

## What this is, and what it is not

The design is small. Each settlement entry's digest includes the previous entry's digest. The contract keeps
only the latest digest, with its sequence number and the book it belongs to. Whoever reads the entries can
recompute the digests and compare the result with that one stored value.

**This is not a blockchain.** There is no consensus, there are no blocks of our own, and nothing is mined or
voted on. The settlement layer already orders and finalizes the entries (`REQ-SL-3`). This design adds one
32-byte commitment in the contract's state, which lets a reader tell that what it read is complete. It is
the same idea as the running digest an append-only log keeps.

## The problem

1. **Cost.** The contract stores every entry as an item of its own state: `inbox: Mapping<InboxSeq, InboxEntry>`
   (`contracts/settlement/src/lib.rs:94`). Each one is written in `append` (`lib.rs:834-846`), the only place
   entries are written, and each item carries a storage deposit. Amendment 0029 removed the per-anchor record
   for this reason. The inbox is the same cost again, once per movement, refusal and anchor. `prune_inbox`
   (`lib.rs:501-544`) returns the deposit only when the admin signs a call for at most 128 positions at a time.
   The owner warned that this list was a bad idea for cost.
2. **The re-scan grew without bound and halted a book.** Settle re-reads `[cursor − rescan_window, cursor)` every
   `rescan_every` steps (`services/settle/src/adapter.rs:1161-1166`; `config.rs:103-106`; `deploy/templates/settle.json:29-30`,
   100 and 1024). The core records each difference as a new incident (`crates/ledger-core/src/ledger/settlement.rs:1856-1930`).
   All of them go into one leaf, `incidents: Slot<Vec<Incident>>` (`crates/ledger-core/src/settlement/store.rs:243`,
   pushed at `:570-573`), and that leaf is decoded and re-encoded whole on every write. After a contract reset,
   the stored inbox still held the previous book's entries below the new book's start. Each re-scan reported
   them again. On 2026-10-24, the 45th re-scan pushed that leaf past the guest's heap, and a test book halted
   (`REQ-AT-3`). #73 records each difference once. That bounds re-scans of the same range, but every
   *distinct* incident still lands in the one leaf (about 500 of them halt a book; #74 item 1).
3. **Old entries are reported against a new book.** Nothing in a stored entry says which book it belongs to.
   A reader that starts at the wrong position, or a re-scan window that reaches below a reset, reads another
   book's entries as this book's (#74 item 2).

## Verified facts

1. **The entry digest exists already, and the core holds it.** An entry's digest is blake2b-256 of its stored
   SCALE bytes, which include the block that wrote it (`services/settle/src/chain/mod.rs:202-206`). The core
   keeps that digest as `Coordinate.digest` (`crates/ledger-core/src/settlement/mod.rs:121-128`) and
   de-duplicates on the coordinate (`REQ-IN-1`). The core holds at most one final coordinate per position
   (`store.rs:778`, `final_at`).
2. **Today's events cannot rebuild an entry.** `Deposited` carries no `from` (`lib.rs:151-160`), and `Refused`
   carries no `call` (`lib.rs:213-222`). The stored `InboxEntry` has both (`contracts/settlement/src/types.rs:482-567`).
   If the stored list goes away, a new event must carry the whole entry.
3. **`Head` cannot change encoding.** It is stored at `0x0000_0002` and passed between frames
   (`types.rs:143-156`; amendment 0039, fact 5).
4. **Entries are written in three places outside a batch's own frame.** `deposit` and `credit` append directly
   (`lib.rs:385-439`). Inside a batch, a withdrawal to another deployment runs in a frame of its own that is
   rolled back whole if it fails (`run_batch`, `isolate`, `lib.rs:849-952`).
5. **An ordinary node keeps events for about 256 blocks.** Events are state at their block, and a full node
   keeps that state for 256 blocks by default (`services/settle/src/chain/rpc.rs:991-995`). The light client
   cannot look a block up by number, so it walks back by parent hash (`rpc.rs:980-981`).
6. **The adapter reads the inbox only by position at the finalized head** (`rpc.rs:601-628`, `adapter.rs:2463-2532`),
   and checks a provisional record by re-reading its position (`adapter.rs:2430-2442`).
7. **The contract's own logic never reads an inbox entry back**, except `prune_inbox` and the debug invariant
   check (`lib.rs:519`, `:1578-1720`). Anchors use `latest_anchor`, and binds use `bound[asset]`.
8. **No code runs at upgrade.** `set_code` swaps the code hash, so any migration is lazy (amendment 0039,
   fact 2).

## The design

### What each entry commits to

For the entry at position `n`, with `e_n` the entry digest of fact 1:

```
e_n = blake2b-256( SCALE(InboxEntry { at, event }) )             -- unchanged: the coordinate's digest
d_n = blake2b-256( "aledgent/settlement-entry/v1" ‖ d_(n-1) ‖ n ‖ e_n )
```

- `n` is a u64, big-endian. That is the convention of the core's other references
  (`crates/ledger-core/src/settlement/envelope.rs:65`).
- The hash is blake2b with a 32-byte output: Kreivo's `blake2_256`. The contract computes it with ink!'s
  `hash_bytes::<Blake2x256>`, the same host function.
- `d_n` is the **chain digest** at `n`. Building it over `e_n` (rather than over the raw bytes) lets the core
  recompute it from coordinates it already holds. The coordinate type and `REQ-IN-1` stay as they are.

### Where a chain starts: genesis and reset

```
d_(from_seq − 1) = blake2b-256( "aledgent/settlement-genesis/v1" ‖ contract account ‖ epoch ‖ book ‖ from_seq )
```

- `epoch` (u32), `book` (32 bytes) and `from_seq` (u64) are amendment 0039's `AnchorEpoch` fields
  (`types.rs:201-225`). On an instance that was never reset, `epoch` is 0 and `book` is 32 zero bytes.
- **A reset starts a new chain.** `set_anchor_head` (`lib.rs:581-643`) starts a new chain at its own `from_seq`
  (the inbox's `next_seq` at the reset), with the new book and epoch in the genesis.
- So a digest of the old book's chain can never extend the new book's chain. A reader that starts below
  `from_seq`, or carries a digest from before the reset, does not match the new head. **Old entries cannot be
  reported against a new book.**
- Inbox positions keep running across a reset (0039's S2). A coordinate `(instance, seq)` still names one entry
  for good.

### What the contract stores

The contract has one new cell. Every existing cell keeps its key and its encoding.

```
entry_head: Lazy<EntryHead, ManualKey<0x0000_000C>>
EntryHead { epoch: u32, book: [u8; 32], from_seq: u64, next_seq: u64, digest: [u8; 32] }
```

That is 84 bytes, overwritten in place. It is read with `entry_head()` (a new read-only message).

- **`append` stops writing to `inbox`.** It computes `e_n` and `d_n`, writes the new `EntryHead`, and emits one
  new event that carries the whole entry: `Entry { seq (topic), entry: InboxEntry }`. That is everything a
  reader needs to recompute `e_n`. The per-kind events (`Deposited`, `Withdrawn`, `Anchored`, …) are kept,
  subject to Q5.
- **Frames.** `append` reads `entry_head` from storage and writes it back each time. It never carries the
  digest in memory across a frame. A frame that is rolled back takes its `entry_head` write with it, together
  with everything else it did. `Head` and `apply`'s signature do not change. `deposit` and `credit` go through
  `append` like the rest.
- **Lazy start.** If `entry_head` is absent, the first `append` after the upgrade writes the genesis at
  `from_seq` = `head.next_seq`, from `anchor_epoch` if one exists, and emits `EntryChainStarted { epoch, book,
  from_seq }`. `set_anchor_head` does the same for a new chain. A deposit can arrive at any time, from anyone,
  so v3 must be able to start a chain on its own.
- **What stays:** `latest_anchor`, `anchor_epoch`, `governance`, `positions`, `bounds`, and `bound[asset]` (the
  position of a bind's entry, which is never pruned). Each is bounded by the number of assets, or is a single
  item.
- **Entries written before the upgrade** stay where they are and can still be read with `inbox_entry(seq)`.
  `prune_inbox` keeps working, but only below the chain's `from_seq`, and releases their deposits (Q7). Nothing
  above `from_seq` is ever stored.
- `REQ-SL-5` gets stronger: an entry adds no item to the layer's current state.

### How the adapter verifies when it syncs

The adapter keeps, per instance, the **last head it accepted**: `(seq, digest)` and the block it read it at. It
keeps these beside its cursor (`settle_cursors`, `services/settle/migrations/0001_settle.sql:67-70`). Each step:

1. **Read the head:** one read of `entry_head` at the finalized block.
2. **Equal to the accepted head: done.** The check is that single read. Nothing else is read for completeness.
3. **The head moved:** read the `Entry` events from the blocks after the one last read, up to the finalized
   block. Recompute `d` for only those entries, starting from the accepted digest. Never recompute the whole
   chain. If the result equals the head, report each entry to the core in order, exactly as the cursor read does
   today. Accept the new head only where the cursor advances today: after the core has taken the entry in
   (`adapter.rs:2524`). Then "equal" means the core ingested everything, not only that the adapter saw it.
4. **Not equal: broken at `n`,** where `n` is the first position after the accepted head. The adapter reads
   once more (from the history node, if one is configured). If the mismatch remains, it reports the break to
   the core, raises an alert, and **does not advance past `n`** (the rule of `REQ-SA-7`). It keeps anchoring:
   anchoring is not on any holder's critical path (`REQ-PR-9`), and it reads `latest_anchor`, not entries.

A **missing** entry, an **extra** one or an **altered** one changes every digest after it. Wherever it is, the
next sync finds it, so no re-scan is needed and **the re-scan timer (`rescan_every`, `rescan_window`) is
removed**.

**Provisional records** (`REQ-IN-3`, `REQ-IN-4`). A record seen above finality is read from the `Entry` event
of a recent block. When the finalized head passes it, the record is checked against the entry at its position
in the verified range, instead of by re-reading the stored position (fact 6).

### Where the entries are read from, once storage drops them

- **Routine settlement needs an ordinary node or the light client.** The adapter reads the new entries of each
  step's few blocks, which an ordinary node keeps for about 256 blocks (fact 5). **Routine settlement never
  needs an archive node.**
- **An archive node is needed only to catch up or to restore.** If the adapter is further behind than an
  ordinary node keeps (it was down, or its store is being restored from scratch), it reads the missing events
  from a **history node**: an RPC endpoint the configuration names (`history_endpoint`), separate from the light
  client, which cannot serve old state.
- **Unreadable, never missing** (amendment 0029) applies to that catch-up only. If no reader can serve a block's
  events, the adapter says the position is unreadable, alerts, and stops there. It never calls the entry
  missing, and it never skips without the explicit gap of `REQ-SA-7`.
- **Readers other than the adapter.** The verifier reads anchors from `latest_anchor` as before. The one read it
  made from the stored inbox, an anchor superseded in the same block as a reset (0039, "The verifier"), moves
  to that block's `Entry` events.

### What the core keeps instead of the incident list

- **One leaf per instance:** `SettlementChain { epoch, book, from_seq, seq, digest, break: Option<Break> }`.
  - `seq` and `digest` are the head the core has checked.
  - `Break { at: n, layer: (seq, digest), found: Time }` is "broken at `n`". At most one break is open at a time,
    because nothing advances past it.
  - The leaf has a fixed size and is never a list.
- **The head report carries the head.** `HeadBody` (`services/ingress/src/settlement.rs:558-567`) already
  carries the finalized height and `next_seq`. It gains `digest`.
- **The core checks too (Q1).** On a head report, the core recomputes `d` over **only** the positions between
  its own `seq` and the reported one. It uses the final coordinates it ingested (`final_at`), reads each one once
  in the book's life, and caps how many it reads per input.
  - A position it does not yet hold as final is a wait, not a break.
  - A different digest is a break.
  - This is the comparison that `REQ-IN-5` names: what the layer recorded against what aledgent ingested.
- **A first head names the book.** The core accepts a chain only if its genesis names this book, under 0039's
  q1 (which digest names a book). It refuses an observation below the chain's `from_seq`. A test book could not
  have halted this way.
- **Clearing a break** is either a re-read that verifies (a reader fault), or an operator intervention with its
  own authority and a justification (`REQ-AT-3`). Every break, and how it was cleared, goes to the audit trail
  (`NFR-6`), which is append-only and outside the book's state. The open break appears on `/v1/ops/signals`
  (#74 item 3).
- **Removed:** the `Rescan` input's path (`ledger/settlement.rs:1856-1930`) and the incidents `Missing`,
  `Unknown` and `Gap`. The `Rescan` call stays decodable (`crates/ledger-core/src/call.rs:260-266`), so that old
  input logs replay under the code that took them (`NFR-22`, `F-037`). The new code refuses it. If #73 has
  merged, its index `RESCAN_RECORDED` is removed too.
- **What this does not remove.** The other settlement incidents still exist: `AmountMismatch`, `Unassociated`,
  `Unusable`, `LateEvidence`, `FinalFailure`, `SettlementRefused`, `BindRefused` and `IncludedWithoutRecord`
  (`settlement/mod.rs:663-723`). They must not go into one growing leaf again either. The default proposed here
  keys them: each incident gets a leaf of its own, keyed by its operation or its coordinate, and a write touches
  only that leaf (#74 item 1, `T-032-26`). See Q2.

## `SPEC.md` text (proposed)

**`REQ-IN-5`, restated:**

> aledgent MUST monitor observation **completeness**, not only observation. Each entry of the settlement
> layer's sequenced record commits to the entry before it, and the layer's current state holds the latest
> commitment, with its position and the book it belongs to. At every sync, aledgent MUST check that what it
> read since the last sync extends the commitment it last accepted to the one the layer now holds, and that what
> it ingested does the same. A difference MUST raise an incident that names the first position not shown
> complete, and aledgent MUST NOT treat any later position as complete until it is resolved. No periodic
> re-scan is required. Entries from before the book's first position MUST NOT be compared with the book.
> *Amendment 0040.*

**`REQ-SL-9`, restated:**

> The settlement layer MUST expose the movements and anchors concerning a deployment as a **sequenced record**,
> in which each entry commits to the one before it, and MUST hold the latest commitment and its position in its
> current state. Entries MAY be readable only from the layer's finalized history. Observation completeness
> (`REQ-SA-7`) is then a read of the entries since the last sync, checked against one value of current state, and
> it does not depend on a subscription. Reading entries older than an ordinary reader keeps MAY require a reader
> that keeps the layer's history (amendment 0029's assumption). *Amendment 0040.*

**`REQ-SL-10`:** "and the sequenced record of `REQ-SL-9`" becomes "and the latest commitment of the sequenced
record of `REQ-SL-9`".

**`REQ-SA-7`, one sentence added:**

> A range is read successfully only if it extends the commitment the adapter last accepted to the one the layer
> holds. *Amendment 0040.*

**`AC-D2.1`:**

> Given any sync, Then aledgent states the position up to which the settlement layer's record and its ingested
> observations are shown to agree, or the first position at which they do not. A difference is an incident
> (`REQ-IN-5`, `NFR-9`).

**`AC-D2.2`:**

> Given a range that could not be read (no reader keeps it), Then it is recorded as unreadable, or as an explicit
> gap an operator chose, and never as missing and never silently skipped (`REQ-SA-7`).

`REQ-SA-7`'s trade-off paragraph stands. A gap is still reconciled from an archive afterwards.

## Properties

- **C1, one read when idle:** an instance with no new entries is checked with one read of current state.
- **C2, work proportional to new entries:** a sync recomputes only the entries since the last accepted head, and
  the core reads each final coordinate once.
- **C3, nothing grows per entry:** not the contract's state, and not the core's completeness record.
- **C4, any change below the head is found at the next sync:** a missing, extra or altered entry changes `d` at
  every later position (barring a hash collision).
- **C5, one book per chain:** a chain's genesis binds the contract, the epoch, the book and the first position.
- **C6, no value moves and no coordinate changes:** the upgrade and the chain change no position, bound, binding,
  key or inbox numbering (0039's S1 and S2 still hold).

## Failure modes

| Case | What happens |
|---|---|
| The adapter is down longer than an ordinary node keeps | It catches up from the history node. Without one, the first unread position is unreadable: an alert, no advance, never "missing" |
| A reader returns an incomplete or altered block of events | The recomputed head differs. The adapter re-reads, from another reader if it has one, before it reports a break |
| The core lacks a final coordinate the layer holds | A wait while the coordinate is not yet final; a break naming the position if a different coordinate is final there |
| An entry of the previous book is read after a reset | Its digest cannot extend the new genesis; the core refuses positions below `from_seq` |
| A frame of a batch fails | Its `entry_head` write is rolled back with it, and the refusal is appended as an entry in the outer frame |
| Rollback to v2 after v3 wrote entries | v2 would append to `inbox` from `head.next_seq` and ignore `entry_head`. The v3 positions have no stored entries, so v2's reads of them answer "absent". Roll back only before v3's first entry, or accept a chain that has to be restarted by a reset |
| A settle still running the re-scan meets a v3 contract | It reads no stored entries above `from_seq` and reports them as absent. v3 and the new settle go together, and the old settle runs with `rescan_every: 0` meanwhile |

## Migration from contract v2

- v2 is the code with `set_anchor_head` (amendment 0039). v3 is this code. The upgrade is `ContractsStore.
  publish_upgrade(app, code)`, then `ContractsStore.upgrade(app, licence)`. Both require the **licence owner**:
  `publish_upgrade` from the publisher, and `upgrade` from the licence owner, who runs `Contracts::set_code`.
  `set_anchor_head` requires **governance** (`NotGovernance` otherwise), and `set_governance` requires the
  admin once and governance after that. No code runs at upgrade (fact 8).
- **Storage:** one new cell (`0x0000_000C`); nothing rewritten. The test of the upgrade writes the v2 layout by
  raw key, applies v3, and checks every existing cell byte for byte, as 0039's test does.
- **The first entry after the upgrade starts the chain** at that moment's `next_seq` (lazy start, above). Stored
  entries below it keep being read by position, as today, until they are pruned.

## What happens to existing books

- **A book whose chain starts with v3** (a new book, or a book reset under v3) never sees a stored entry. Its
  adapter's cursor starts at the chain's `from_seq`, read from `entry_head`. #74 item 2 and `T-036-15` are
  answered without a manual cursor seed.
- **A running book across the upgrade.** Positions below the chain's start were checked, or not, by the old
  path. After the change, nothing re-checks them. A one-off comparison of those positions, made while their
  stored entries still exist, closes them (Q11).
- **The core change is a state-layout change.** It removes the incidents leaf (kind 51, and `RESCAN_RECORDED`
  if #73 merged) and adds `SettlementChain`, and the export format changes with it. A running book takes it as
  an `F-037` upgrade with a stateful migration, which needs `T-037-19`, or at a re-founding. Q11 asks which.
- **The book on test** is being re-founded after the halt anyway. It is founded on code that has #73 and runs
  with re-scans off until v3 and the new settle are in.
- **Prod** has not run settle on its current book. The settle stays off, or runs with `rescan_every: 0`, until
  #73 or this design is in.

## Rollout (described, not executed)

1. **Test.** The test instance's admin is a key, so the admin's calls are each one signed call, with no
   referendum. The code upgrade is made by the test instance's licence owner on its fast track. Then the new
   settle (sync verification, no re-scan) and the new core.
2. **Prod, after the Root referendum enacts.** That referendum upgrades prod to v2 and gives the licence-owning
   community the chain's default track. Then **the community's referendum**: `Utility.batch_all([ContractsStore.
   publish_upgrade(app, v3), ContractsStore.upgrade(app, licence)])`, on the code hash CI built and reviewers
   compared. If prod's book has not been reset yet, reset it **under v3**
   (`Communities.dispatch_as_account(Contracts.call(set_anchor_head(…)))` as governance), so that its chain starts
   at the book's base with no stored range in between (Q8).
3. Settle on prod comes back with the new code. Its first sync reads `entry_head` and starts from `from_seq`.

**Rollback:** see the failure modes. Before v3's first entry, the old code can be put back by the same two
calls.

## Rejected alternatives

- **Keep the list and prune it on a timer.** That still pays a deposit per entry until someone prunes, and
  pruning shortens what a re-scan can read (`features/F-019-settlement-pallet/plan.md:158`). It fixes neither cost nor growth.
- **A Merkle tree of entries.** It would prove one entry without the rest, which nobody here needs: the adapter
  reads every entry in order anyway. A running digest is one hash per entry and one value of state.
- **Re-scan less often, or only the new range.** The halt came from re-scanning ranges that were already
  scanned. Reading the new range only is exactly what the chain makes sufficient.
- **Keep the last N entries stored.** That is still a list, with a cost per entry and a pruning rule.

## Open questions for the owner

- **Q1. Does the core also recompute the chain over what it ingested, or does it trust the adapter's check?**
  The draft has the core recompute it over only the new positions, one leaf read each, once. `REQ-IN-5`
  compares the layer with what aledgent *ingested*, and the adapter and the core disagreed in the halt. The
  alternative is cheaper but leaves that comparison to the adapter.
- **Q2. Where do the incident kinds that remain go** (`AmountMismatch`, `Unassociated`, `Unusable`,
  `LateEvidence`, `FinalFailure`, `SettlementRefused`, `BindRefused`, `IncludedWithoutRecord`)? The draft keys
  each one in a leaf of its own. The alternatives are the audit trail only, or counters on the signals. "Broken
  at `n`" replaces only `Missing`, `Unknown` and `Gap`.
- **Q3. How is a break cleared?** The draft: automatically, when a re-read verifies; otherwise by an operator
  intervention with a justification, which either records an explicit gap and accepts the layer's head, or
  waits for a reader that can serve the range.
- **Q4. Should each `Entry` event carry its chain digest `d_n`?** Then "broken at `n`" names the exact entry
  instead of the first position after the accepted head, at 32 bytes more per event. The draft leaves it out to
  stay minimal.
- **Q5. Keep the per-kind events (`Deposited`, `Withdrawn`, `Anchored`, `Bound`, `Refused`) beside `Entry`?**
  Their topics let a reader find an operation's entry by its reference. Dropping them halves the event bytes.
  The draft keeps them.
- **Q6. Should `EntryHead` also hold the block of the latest entry?** Then catch-up knows where to stop walking
  back. That is 4 more bytes. The draft does not include it.
- **Q7. Is `prune_inbox` kept for the entries v2 stored,** to release their deposits, with `Bound` entries still
  kept? The draft keeps it, below the chain's start only.
- **Q8. Is prod's book reset once, under v3,** with 0039's second referendum moved after the v3 upgrade? Or is
  it reset under v2 first? Then its chain starts at the upgrade, and the range between is read the old way once.
- **Q9. On an instance never reset, is a chain whose book is 32 zero bytes acceptable,** or must governance
  start the chain explicitly (a reset to the current base) before v3 writes anything? A deposit can arrive at
  any time, so the draft starts it lazily.
- **Q10. Which digest names a book?** This is 0039's q1, carried over and not decided here. The genesis and the
  core's acceptance of a chain both depend on it.
- **Q11. For a running book: a stateful migration (`T-037-19`) or a re-founding?** And should a one-off
  comparison of the positions below the chain's start run before the re-scan path is removed?
- **Q12. Is the `gaps` list bounded the same way?** That is the operator-chosen gap kept as the latest one, in
  `SettlementChain`, with the history in the audit trail. Or is it keyed by range? (`gaps` is
  `Slot<Vec<Gap>>`, `store.rs:242`, the same shape as the incidents.)

## Plan and code

`F-043` (`features/F-043-chained-settlement/`): contract v3 (`contracts/settlement`), sync verification in the
adapter (`services/settle`), the core's chain record and the removal of the re-scan path (`crates/ledger-core`,
`services/ingress`), tests, and the rollout. #73 stays the stopgap until `F-043` reaches each environment.
