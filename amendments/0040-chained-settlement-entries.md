<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0040 — Chained settlement entries: the contract keeps one head, not a list

**Status:** **draft**. The owner ruled the design on 2026-10-26, in two rounds (below). Nothing here is applied
to `SPEC.md`; the plan and the code cite it as `[PENDING AMENDMENT 0040]`. The contract is built (`T-043-02`).
**Target:** `SPEC.md` v0.4.29 as on `develop` (at `c79d12d`); `F-016`, `F-026`, `F-027`; amendment 0039.
**Date:** 2026-10-26
**Number:** 0039 is the last on `develop`; no open PR or remote branch claims 0040.
**Amends:** `REQ-IN-5`, `REQ-SL-9` (restated); `REQ-SL-10` (one phrase); `REQ-SA-7` (one sentence); `REQ-AT-1`
(one item); `AC-D2.1`. No command, query or error condition of `SPEC.md` changes.
**Rationale in one line:** if each entry's digest covers the one before it, one stored value proves every
entry, so the contract need not store the entries and the core need not re-scan them.

---

## The problem

1. **Cost.** The contract (v2) stores every entry (`inbox`, written by `append`), each at a storage deposit
   until the admin prunes it (`prune_inbox`).
2. **The re-scan grows the book's state.** Settle re-reads recent positions on a timer
   (`services/settle/src/adapter.rs:1161-1166`). Each difference is an incident in one leaf that is rewritten
   whole on every write (`crates/ledger-core/src/settlement/store.rs:246`), and a test book halted on it. #73
   records each difference once (`store.rs:273`), but every incident and every gap (`store.rs:245`) still
   lands in a leaf that only grows.

## Verified facts (on `develop` at `c79d12d`)

1. **An entry's digest is `e_n = blake2b-256(SCALE(InboxEntry { at, event }))`**: the stored bytes, which
   include the block number `at` (`services/settle/src/chain/mod.rs:201-206`; `InboxEntry`,
   `contracts/settlement/src/types.rs:560-565`). `append` sets `at` to the current block (`lib.rs:839`).
2. **Entries are written in five places, each followed by an event with the same `seq`:** `Withdrawn`
   (`lib.rs:963`), `Anchored` (`:988`), `Bound` (`:1026`), `Refused` (`:1061`) and `Deposited` (`:1097`,
   for both `deposit` and `credit`). An anchor that repeats the latest writes and emits nothing (`:986`).
3. **Two events lack a field of their entry.** `Deposited` has no `from` (`lib.rs:149-160`; entry,
   `types.rs:485-494`), and `Refused` no `call` (`lib.rs:211-222`; entry, `types.rs:520-527`). `Withdrawn`,
   `Anchored` and `Bound` carry every field (`lib.rs:163-207`). Nothing decodes the contract's events today:
   no code matches `ContractEmitted`, and settle reads only `System` events (`chain/rpc.rs:1011-1020`).
4. **`Head` cannot change encoding** (`types.rs:143-156`): deployed instances store it and frames pass it.
   The next free storage key is `0x0000_000C` (`lib.rs:113`).
5. **The book's logic reads the gaps; it never reads the settlement incidents.** `proof` answers
   `Unprovable` when a gap overlaps `[from_seq, head.next_seq)` (`crates/ledger-core/src/ledger/settlement.rs:1689`).
   Outside export and the re-scan's index rebuild (`store.rs:793-802`), no code reads `incidents`.
6. **Removing a leaf keeps the core's state layout.** The actor keeps one map of rows
   (`agents/core/src/lib.rs:68-73`), and the leaf kinds are keys in it, not the layout the runtime checks on an
   in-place upgrade (`agents/core-host/src/switch.rs:373-375`). The founding records of the test book and of
   `p2` both name the patched runtime (`24d9f451…`, VOS `bd8d1bb7`) that `T-037-19` needs.
7. **The deployed instances hold few entries, all anchors** (read-only, finalized #40,151,145, 2026-10-26).
   Test: head `{ last_anchor 1, next_seq 13, pruned_to 0 }`, 13 stored `Anchored` entries, governance named
   (community 1200's account). Prod: `{ 17, 17, 0 }`, 17 stored `Anchored` entries, **no governance named**.
   Neither has a deposit, a withdrawal or a bind.

## The design

This is not a blockchain: no consensus, no blocks of our own. Kreivo already orders and finalises the entries
(`REQ-SL-3`). This adds one 40-byte value to the contract's state, so a reader can tell it read them all.

### The contract (one release)

There is no intermediate version: the contract that replaces v2 is the final one (26 Oct evening ruling).

- **One new cell** at `0x0000_000C`: `(position, digest, start)`. `position` is the next entry's position,
  always `Head.next_seq`; `digest` is the chain digest after the last entry; `start` is where the chain began
  from the zero digest. An absent cell reads as `(next_seq, zeros, next_seq)`, which is `(0, zeros, 0)` on any
  instance this code instantiated or reset.
- **Each entry `n`** sets the cell to `(n + 1, d_n, start)`, with
  `d_n = blake2b-256("aledgent/settlement-entry/v1" ‖ d_(n-1) ‖ n ‖ e_n)`: the label as ASCII bytes with no
  length prefix (a40c-domain-tag), `n` as 8 bytes big-endian, `d_(start-1)` the 32 zero bytes. Nothing else is
  stored. The cell is written in the frame that appends, so a frame that is rolled back takes its write with it.
- **The reset is a full reset** (`set_anchor_head`, governance only). It removes every entry v2 stored, the
  positions `[0, start)`; sets `Head` to `{ last_anchor: base, next_seq: 0, pruned_to: 0 }`; and clears the
  cell. `Head` keeps its encoding and `next_seq` runs from 0 in the new chain, so readers of `next_seq` read
  the chain's position with no change. The storage deposits it releases go to the origin of the call: the
  governance account (on Kreivo, the community account a referendum dispatches as). It removes at most 64
  entries and refuses an instance that may hold more (`TooManyStoredEntries`), changing nothing. Measured on a
  fork of Kreivo (spec 134): 1.71 G of ref time and 186 KB of proof with nothing to remove, plus 0.167 G and
  10.3 KB per entry; 4.55 G and 362 KB for prod's 17.
- **Why `start`.** Between the upgrade and the reset an instance can still append (an anchor), which advances
  `next_seq` and stores nothing. `next_seq` then no longer marks the end of what v2 stored, and an instance
  that was reset once has nothing stored at all. `start` is that end.
- **Removed:** `prune_inbox` and the read `inbox_entry`. Their selectors (`0x0000_0202`, `0x0000_0302`) are
  never reused, so an old reader fails to dispatch rather than reading "absent". The read `chain_head`
  (`0x0000_030A`) is new. `Error` keeps every variant's index; one is appended.
- **Events gain the two missing fields:** `from` on `Deposited`, `call` on `Refused`; no new event, no digest
  (a40-q4). A reader rebuilds `e_n` as `InboxEntry { at, event }` from the event's fields and its block's
  number, never from the event's bytes, and orders entries by position. This holds for all five append sites
  (the contract's unit tests fold the events of every kind, a failed frame and a credit, to the head).
- **`bound[asset]`** stays: it is the once-ever guard of `REQ-AS-8`, and a reset does not touch it, as it does
  not touch positions. Its value is the position of the `Bound` entry in the chain it was bound in, which after
  a reset is an earlier chain's; the `Bound` event, in its block, says which. No instance has a bind today.
- **Positions restart at 0 per chain** (a40b-positions). A coordinate is `(instance, sequence, position)`. This
  reverses 0039's rule that positions run on across a reset (`types.rs:195-196`).

### The adapter

It keeps the last head it accepted per instance, with the block it read it at, beside its cursor
(`settle_cursors`, `services/settle/migrations/0001_settle.sql:67-70`). Each step:

1. It reads the head at the finalised block. **Unchanged: done.**
2. **Moved:** it reads the events of the blocks since the last read and **folds** the new entries: it
   recomputes the digest entry by entry, starting from the accepted one. If the result equals the head, it
   reports the entries to the core in order, as today, and accepts the head once the core has taken in every
   entry up to it (where the cursor advances today, `adapter.rs:2524`).
3. **Different:** it reads once more. If the result still differs, it raises an alert and does not pass the
   first new position. It keeps submitting and anchoring (`REQ-PR-9`).

A full node keeps about 256 blocks of events (`rpc.rs:992-995`), so **routine settlement needs no archive
node**. An adapter further behind catches up from a node that keeps history. If none can serve a block, the
position is unreadable: an alert and no advance, until a reader serves it or an operator records a gap and the
adapter accepts the layer's head unverified for that range (a40-q3). Only observation past it waits. Nothing
is pruned any more, so the adapter's own `Pruned` gap (`adapter.rs:2531`) goes. The contract no longer answers
`inbox_entry`, so the adapter's read by position goes with it.

A book's adapter starts at `(0, zeros)`: just after the reset's `AnchorHeadSet` event, or at instantiation on
an instance never reset. A cursor kept from before the reset (test's sits at 13) is reset with the re-founding.
**The re-scan is removed:** `Adapter::rescan` (`adapter.rs:3388`), its timer, and
`rescan_every` and `rescan_window` (`services/settle/src/config.rs:103-106`).

### The core

- **It trusts the adapter's check** (a40-q1) and keeps no chain record.
- **Removed:** `Ledger::rescan` (`ledger/settlement.rs:1857-1938`), its route (`services/ingress/src/surface.rs:625`),
  the incidents `Missing` and `Unknown`, and the indexes only it reads (`store.rs:258`, `:273`).
  `Call::Rescan` stays decodable (`crates/ledger-core/src/call.rs:260-266`) for replay (`NFR-22`), and is refused.
- **Settlement incidents and gaps leave the book's state** (a40-q2, a40b-gaps). Each is written as an audit
  record (`NFR-6`, `crates/ledger-core/src/audit/`), in the batch that would have written it. No audit class
  covers them today (`audit/mod.rs:58-68`), so `F-043` adds one. The leaves at `store.rs:245-246` go.
- **One number stays for the gaps** (a40c-gaps-watermark): the end of the highest gap, a `u64`. `proof`
  (fact 5) answers `Unprovable` when `from_seq` is below it. Once the head has passed the gap, that is the
  answer the list gives. Before then it can be `Unprovable` where the list would say `NotIncluded`: never less
  cautious.

## `SPEC.md` text (proposed)

**`REQ-IN-5`, restated:**

> aledgent MUST monitor observation **completeness**, not only observation. At each sync, the adapter MUST check
> that the entries it read since the head it last accepted lead exactly to the head the settlement layer now
> holds (`REQ-SL-9`). It MUST accept the new head only after aledgent has ingested every entry up to it. A
> difference MUST raise an incident, and the adapter MUST NOT pass the first position it could not show
> complete. No periodic re-scan is required. Settlement incidents and gaps are recorded in the audit trail
> (`NFR-6`), not in the book's state. *Amendment 0040.*

**`REQ-SL-9`, restated:**

> The settlement layer MUST expose the movements and anchors concerning a deployment as a **sequenced record**,
> numbered from 0 for each book. Each entry's digest covers the digest of the entry before it. The layer MUST
> hold in its current state the position and digest of the latest entry, and nothing for each entry. Observation
> completeness (`REQ-SA-7`) is then a read of the entries since the last sync, checked against that one value.
> It does not depend on a subscription. *Amendment 0040.*

**`REQ-SL-10`:** "and the sequenced record of `REQ-SL-9`" becomes "and the position and digest of the latest
entry of the sequenced record of `REQ-SL-9`".

**`REQ-AT-1`, one item added:** "every settlement incident and every gap (`REQ-IN-5`, `REQ-SA-7`)."

**`REQ-SA-7`, one sentence added:** "A range is read successfully only if it leads from the head the adapter last
accepted to the head the layer holds."

**`AC-D2.1`:** "Given any sync, Then aledgent states the position up to which the settlement layer's record and
its ingested observations are shown to agree, or the first position at which they do not. A difference is an
incident (`REQ-IN-5`, `NFR-9`)."

## Migration and rollout

- **v2 to this code** is `ContractsStore.publish_upgrade` then `ContractsStore.upgrade`, by the licence owner. No
  code runs at upgrade; every existing cell keeps its key and its bytes (tested on both instances' live cells).
- **Then the reset**, by governance, in the same window: on test (governance named) before the next settlement
  there; on prod after the admin names governance (none is named today), then the reset (a40b-prod-order).
  Entries written between the upgrade and the reset belong to the book before; the reset removes what v2
  stored and starts the new book's chain at 0.
- **Settle stays off on both** until the adapter verifies the chain (`T-043-04`): v2's adapter reads stored
  entries, which this code does not write.
- **Running books take the core change in place** (a40b-existing-books, fact 6): the stateful migration
  (`T-037-19`) writes their incidents and gaps to the audit trail, sets the gap end, and drops the leaves.
- **Rollback:** before the reset, the same two calls put v2 back. After it, fixes go forward.

## Rejected alternatives

- **The core folds too** (a40-q1's alternative): the adapter accepts a head only after the core has taken in
  every entry up to it, so a second fold would check the same bytes again.
- **A new event carrying the whole entry:** two added fields are enough (facts 2 and 3).
- **Prune the list, keep the last N entries, or re-scan less often:** each still pays for, or re-reads, every entry.
- **An intermediate version that keeps v2's entries and `prune_inbox`, with `next_seq` frozen** (a40b-prune): a
  partial reset leaves stored entries and a second counter behind; the owner ruled a full reset instead.

## Rulings (2026-10-26)

- **a40-q1:** the core trusts the adapter's check.
- **a40-q2:** settlement incidents go to the audit trail only, outside the book's state.
- **a40-q3:** a re-read clears a mismatch, or an operator records a gap or waits; the rest of the book goes on.
- **a40-q4:** no digest in events.
- **a40b-positions:** positions restart at 0 for each chain.
- **a40b-gaps:** gaps go to the audit trail too.
- **a40b-existing-books:** running books upgrade in place (`T-037-19`), not by re-founding.
- **a40b-prod-order:** on prod, upgrade first, then one reset.
- **a40c-domain-tag:** a fixed label in front of the chain digest.
- **a40c-gaps-watermark:** the book keeps one number, the end of the highest gap; the history goes to the audit
  trail.
- **a40c-prune-all:** everything v2 stored goes, bind entries and the latest anchor's entry included.
- **Evening of 26 Oct** (supersedes a40b-prune, a40b-test-now and a40c-positions-cost): keeping the prior
  entries and not resetting `next_seq` "sounds like a bug". One release, no intermediate version, published
  before the next settlement on test; the reset is full.

## Open, for the owner

- **Prod has no governance named.** The admin names it (the community 1201 account) before the reset can run.
- **Re-binding after a re-founding.** A reset keeps every binding, so a re-founded book cannot bind an asset the
  earlier book bound. Today no instance has a bind.
- **The chain cell has a third field, `start`**, beyond the `(position, digest)` ruled. It is what lets the reset
  know where v2's stored entries end once the head has moved past them.

## Plan and code

`F-043` (`features/F-043-chained-settlement/`) carries the implementation detail and the tasks.
