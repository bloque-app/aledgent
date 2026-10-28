<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0041 — The book follows a new settlement sequence without a re-founding

**Status:** **draft**, ruled by the owner on 2026-10-28 (form q28, and decision 2 the same day). Every
decision is ruled as built but decision 6, ruled the other way (the start clears the gap end), and the build
follows the ruling. Nothing here is applied to `SPEC.md` yet; the plan and the code cite it
as `[PENDING AMENDMENT 0041]`. The implementation is built on top (`T-043-14` to `T-043-17`).
**Target:** `SPEC.md` v0.4.29 as on `develop` (at `9c12b11`); amendments 0039 and 0040; `F-043`.
**Date:** 2026-10-28
**Number:** 0040 is the last on `develop`; no open PR or remote branch claims 0041.
**Amends:** `REQ-SL-1` (0039's paragraph, one phrase); `REQ-SL-9` (as 0040 restates it, one phrase);
`REQ-SL-4` (one paragraph); `REQ-SA-10` (new); `ERR-SettlementLayerUnavailable` (its meaning, one clause).
Amendment 0040's rule "a reset comes only with a book re-founding" is withdrawn. No command or query of
`SPEC.md` changes, and no error condition is added.
**Rationale in one line:** the settlement layer can start a new sequence of its record under a book that keeps
running, and today the book keeps the old sequence's head and silently drops every head of the new one, so
withdrawals created meanwhile are born expired.

---

## The problem

Amendment 0040 lets governance reset an instance: its positions restart at 0, and a new chain of entries begins
(`AnchorHeadSet`, `anchor_epoch()`). It assumed the reset comes only with a re-founding, so that the core of
the new book starts with no head at all. Neither deployment follows that assumption:

- **Test.** Book `r10` was founded on 26 Oct, before the instance's reset on 27 Oct. Its core took heads of the
  chain before the reset, up to `next_seq` 13.
- **Prod.** The planned reset of the prod instance (0040's rollout) has no re-founding: `p2` keeps running.

The core keeps the highest head it has seen and drops any lower one. So after the reset it drops every head of
the new chain until that chain passes position 13. A withdrawal created meanwhile takes its expiry and its first
position from the stale head, and is born expired.

A contract reset belongs to a re-founding and its maintenance window, never to routine maintenance. If one finds
anything in flight, the core refuses it and pages (`epoch:`), and that is the signal that a re-founding window is
needed (decision 2).

## Verified facts (on `develop` at `9c12b11`, and read-only on test, 28 Oct)

1. **The core drops a lower head.** `observe_head_chosen` returns without writing when
   `head.height < last.height || head.next_seq < last.next_seq` (`crates/ledger-core/src/ledger/settlement.rs:1687`).
   `head_work` has the same test (`:1573`). The store refuses a head that goes back (`Backwards`,
   `settlement/store.rs:630`).
2. **A withdrawal's instruction is built from the stored head.** Its expiry is `head.height + expiry_window`
   (`settlement.rs:723`) and its first position is `head.next_seq` (`:744`).
3. **Positions are keyed by `(instance, seq)`, with no sequence.** Final records live in `finals`
   (`settlement/store.rs:252`). `MarkFinal` refuses a second final at a position (`PositionTaken`, `:531`).
   `proof` counts the final records in `[from_seq, head.next_seq)` (`settlement.rs:1819`).
4. **The adapter already follows a reset on its own side.**
   - On a first start, a cursor kept from before the reset is moved back to the chain's start
     (`services/settle/src/adapter.rs:2761`). That is what happened on test on 28 Oct: the cursor went from
     13 to 0 and no alert was raised.
   - A reset while the adapter runs raises `reset:<instance>` (`adapter.rs:3133`), and the adapter reports
     nothing more until the operator runs `refound` (`:3238`).
   - The adapter never tells the core about a reset.
5. **The reset names its book.** `AnchorEpoch { epoch, base, from_seq, at, book, previous }`:
   - `base` is the anchor number the new sequence starts after, not a position;
   - `from_seq` is the new chain's first position, 0 for every reset by the current contract;
   - `at` is the reset's block;
   - `book` is the book's chainspec (seed) digest, as governance gave it (amendment 0039, q1 still open).

   The core reads the same digest as its own genesis, in `Ledger::code_binding` (`crates/ledger-core/src/ledger.rs:565`).
   Test's reset names `0x1b2c44a4…6979`, which is `r10`'s chainspec digest.
6. **Test, read-only, 28 Oct.**
   - **Chain:** `anchor_epoch()` is epoch 2, base 0, `from_seq` 0, at #40,195,735.
   - **Core head:** `{ height ≈ 40,142,088, next_seq 13 }`, taken from the chain before the reset on 26 Oct.
     The core holds no final record of that chain; its only trace is 13 `Missing` incidents from a re-scan.
   - **New-chain anchors:** anchors 1 and 2 went final in the core at positions 0 and 1 of the new chain, in
     blocks #40,205,617 and #40,205,633, after the reset.
   - **Instructions:** none can be in flight. Every test asset is `book_only`, so no withdrawal can be
     instructed at all.
7. **Prod's core has never seen settlement.** `p2` holds no head, no final record and no instruction.
8. **No real book has run generation 1.** The core Agent is still built as `GENERATION = 0`
   (`agents/core/src/lib.rs:36`), and both books run their genesis code. `CHAINED_SETTLEMENT` is 1
   (`crates/ledger-core/src/settlement/mod.rs:83`). The `code-history` fixture's generation-1 and 2 ranges hold
   no adapter input (`Adapter` is in `NOT_YET_RECORDED`).

## The design

### What the adapter reports

The adapter reports the new sequence itself, as a new input (`AdapterInput::Epoch`). The input carries the
sequence's number, its base, its first position, its block, the book it names, and the previous anchor
(number and commitment), all as `anchor_epoch()` holds them. It reports it in two cases:

- **When it finds the reset live.** The sync meets `AnchorHeadSet` past the verified head, or sees the
  sequence change on an empty chain. The adapter first checks that the chain before the reset ended at the head
  it verified, so that no entry of it was left unreported (fact 4). Then it reports the start, and only then
  moves its cursor to the new chain's start and goes on. It reports no entry of the new chain before the core
  has taken the start in.
- **Once per sequence, from state.** While the head it verified carries a sequence the core has not confirmed,
  it reads `anchor_epoch()` and reports it. That is test's case: the adapter already moved itself to the new
  chain on 28 Oct.

On the core's answer:

| The core | The adapter |
|---|---|
| started the sequence, or already holds it | goes on; the confirmation is kept, so the sequence is not reported again |
| refused it: the sequence names another book | raises `reset:<instance>`, as today, and reports nothing more until `refound` (the book was re-founded) |
| refused it for any other reason | raises `epoch:<instance>:<epoch>` (pages), reports nothing of the new chain, and reports the start again every ten minutes |
| cannot take the input (a core before the generation of this amendment) | in a live reset, raises `reset:` as today; otherwise a non-paging `epoch:` alert, and goes on |

`refound` stays, for a book that was re-founded.

### What the core does with it

From the generation `SETTLEMENT_EPOCHS` on (decision 1), the core takes the start as follows. Before that
generation it refuses the input with `ERR-MalformedCommand` and writes nothing, so earlier inputs replay as they
were decided.

1. **The same sequence again** (equal in every field) is `Recorded`, and nothing changes. **An earlier or equal
   sequence number with other fields** is refused.
2. **Another book.** The start is refused if its `book` is not the book's own genesis digest. A book founded
   without a chainspec (genesis digest zero) can never match, so its starts are always refused (decision 4).
3. **Instructions in flight.** The start is refused while any outbound instruction is not yet decided (the
   `live` index is not empty). Such an instruction's first position and expiry come from the earlier sequence,
   and the new sequence's records cannot prove anything about it (decision 2).
4. **Final records of an earlier sequence.** The start is refused if the book holds a final record at a
   position at or above `from_seq`, on this instance, observed at or before the reset's block. A record
   observed after the reset's block belongs to the new sequence and is accepted (fact 6: test's anchors at 0
   and 1). A record in the reset's own block cannot be placed by block alone, so it counts as earlier
   (decision 5).
5. **Otherwise the start is taken.** The sequence is recorded, and the head becomes
   `{ height: at, next_seq: from_seq }`, and the gap end is cleared (decision 6): every position below it was
   the earlier sequence's. If the core already holds a head at a later block than `at`, that head is already
   the new chain's and is kept, and so is the gap end, since a gap recorded since may be the new chain's. Later heads compare against it as before, so they are compared within
   the sequence. The head moves back only here, and only to a strictly later sequence; the store enforces both
   (`REQ-AR-4`).

A refusal changes no state but the audit trail. It is one audit record, a settlement incident
(`Incident::EpochRefused`, amendment 0040's class), and its reason is in the adapter's reply. A second start
(sequence + 1) goes through the same steps.

**Left as they are:**
- **The positions' tally.** It only passes final positions, and step 4 leaves none of an earlier sequence at or
  above `from_seq`.
- **The index by position.** Nothing from generation 1 on reads it.

### The guard (a follow-up, `T-043-17`)

From the same generation, when the core drops a head because its position is lower but its block is later, it
records that head as **reported and not taken**. Taking a later head, or a start, clears it. While one is
recorded, `withdraw` and `submit_outbound` are refused with `ERR-SettlementLayerUnavailable`. No instruction is
then built from a stale head, whatever the cause. This is the case of a reset the adapter has not reported yet,
or one the core refused.

### A read of the head (a follow-up, `T-043-16`)

`GET /v1/ops/settlement/head`, for the operator's principals, read-only. Per instance, it shows the core's
finalised head, the sequence it holds, and the head reported and not taken. A test or prod operator can then see
the state this amendment is about without reading logs.

## `SPEC.md` text (proposed)

**`REQ-SL-1`, amendment 0039's paragraph:** "start a new sequence for a re-founded book" becomes "start a new
sequence for a book, re-founded or not".

**`REQ-SL-9`, as amendment 0040 restates it:** "numbered from 0 for each book" becomes "numbered from 0 for each
sequence (`REQ-SL-1`); one book's record MAY run over several sequences".

**`REQ-SL-4`, one paragraph added:**

> An expiry MUST be computed from the latest finalised point of the settlement layer that aledgent holds. While
> the adapter has reported a later point that aledgent did not take, aledgent MUST NOT instruct an external leg
> that can expire, and refuses it with `ERR-SettlementLayerUnavailable`. *Amendment 0041.*

**`REQ-SA-10` (new):**

> When the settlement layer starts a new sequence of its record (`REQ-SL-1`, `REQ-SL-9`) under the book the
> adapter serves, the adapter MUST report the start to aledgent, with the book the sequence names, and MUST NOT
> report an entry of the new sequence until aledgent has taken the start in. Before it reports the start, the
> adapter MUST have shown the earlier sequence complete up to the start (`REQ-SA-7`); otherwise it MUST raise an
> alert and report neither. aledgent MUST take the start as its finalised point and compare every later point
> within the new sequence. When aledgent restarts its finalised point at the start, it MUST clear what it holds
> of an earlier sequence's gaps. aledgent MUST refuse the start, as an incident (`REQ-AT-1`), if it names another
> book, while any external leg it instructed is not yet decided, or while it holds a final record of an earlier
> sequence at a position the new sequence reuses. A refused start MUST change nothing else, and the adapter MUST
> raise an alert an operator sees. *Amendment 0041.*

**`ERR-SettlementLayerUnavailable`:** "The settlement adapter cannot be reached, or refuses to serve
(`REQ-SA-2`)" gains ", or aledgent's finalised point is behind one the adapter reported (`REQ-SL-4`)".

> **Trade-off, stated plainly.** This lets a running book follow a new sequence only while nothing of the earlier
> one can be confused with the new one. A book that has final records of the earlier sequence at the positions
> the new one reuses, or an instruction in flight, cannot follow it. Its start is refused, and it needs either a
> re-founding or the larger change of decision 3. Today that excludes neither deployment (facts 6 and 7). It
> excludes a second reset of a book once that book has settled anything in its current sequence.

## Migration and rollout

- **Release order: the core first, then the adapter's switch.**
  - A core host built before this amendment cannot decode the new input. It refuses the whole feed the input
    arrives in, so the book's inputs would wait behind it. Merge order alone cannot prevent this: test's
    ingress and settle follow every `develop` merge, while its core runs a pinned image.
  - So the adapter reports a start only when its configuration says `report_resets` (off by default). It is
    turned on in a deployment template once that deployment's core runs a build with `T-043-14` at
    `SETTLEMENT_EPOCHS`.
  - A core of such a build still at generation 0 refuses the input cleanly (`ERR-MalformedCommand`). The
    adapter then raises a non-paging alert.
  - A core at `SETTLEMENT_EPOCHS` with the switch off is never told. The guard then refuses withdrawals while
    the head is stale, as it should.
  - The core changes of `T-043-14` and `T-043-17` both decide generation-1 inputs. No generation-1 core
    package may be built or applied before both are on `develop` (decision 1).
- **Test.**
  1. The core's in-place upgrade to the package built as generation 1 (`T-037-19`'s path, the same upgrade that
     takes `T-043-06` to `T-043-08`).
  2. Then `report_resets: true` in test's settle template (bloque-ledgent). The adapter reports epoch 2
     once.

  The core takes it: no instruction is in flight, and its only finals are positions 0 and 1, both after the
  reset's block. Its head becomes `{ 40,195,735, 0 }`, and the adapter's next head (`next_seq` 2) is taken at
  once. Done when `GET /v1/ops/settlement/head` shows epoch 2 and a head at `next_seq` 2 or later, and a withdrawal
  created then has a first position at the head and an expiry above the finalised height.
- **Prod.** The same two steps before 0040's reset of the prod instance. The reset's `book` **must be
  `p2`'s chainspec digest**: the core refuses any other (`OtherBook`), and the adapter then stops with
  `reset:`. After the reset, the adapter reports epoch 1 to a core with no head (fact 7), and the core
  takes it. Prod would not hit the stale head anyway, since its core has none; the start mainly records
  the sequence there.
- **Rollback.** A core built before this amendment refuses the input and keeps its head. The adapter's
  confirmation table is a cache of the core's answers and can be emptied.

## Rejected alternatives

- **Re-found test after the reset** (book `r11`, 0040's own rule). It is cheap on test. But prod's reset has no
  re-founding, and a re-founding for every reset makes a reset an operation on the book, not on the layer.
- **Accept a lower `next_seq` for the head's height only.** It fixes the expiry and nothing else: the new
  chain's records still cannot become final at positions an earlier chain's records hold.
- **Positions keyed by sequence, `(instance, epoch, seq)`, now** (decision 3). It is the general answer, but it
  changes the key of every final record and every reader of one: `proof`, the tally, `MarkFinal`, the export.
  It is not needed by either deployment today.

## Decisions for the owner

1. **The generation.** *Ruled as built (q28).* `SETTLEMENT_EPOCHS = CHAINED_SETTLEMENT = 1`, so test needs one core upgrade,
   0 to 1, the same one `T-043-06` to `T-043-08` need. *Alternative:* a new generation 2, which costs a second
   in-place upgrade (1 to 2) on each book. Generation 1 is safe only because no book has decided an input at
   generation 1 yet (fact 8). Once one has, the rule needs 2. So no generation-1 package may be built or
   applied before `T-043-14` and `T-043-17` are both on `develop`.
2. **Instructions in flight at a start.** *Ruled as built (Pablo, 28 Oct):* the start is refused while any instruction is not yet
   decided. Such an instruction then cannot be decided by proof, so the operator resolves it through review
   with evidence (`REQ-RC-5`) before the start can be taken. *Alternative:* take the start and move each such
   operation to `Needs-Review`, with a mark that keeps `proof` from reading the new sequence for it. That needs
   a per-instruction leaf and a bound on how many move at once. None is in flight on either deployment today.
3. **A book that has settled in the earlier sequence.** *Ruled as built (q28):* refused (the trade-off above). *Alternative:*
   key positions by sequence (rejected alternatives), so that any start can be taken.
4. **The book's name.** *Ruled as built (q28):* the start's `book` must equal the book's chainspec digest. This rules amendment
   0039's q1 for the chainspec digest; a book founded without a chainspec is always refused. *Alternative:*
   the genesis roots, which would change what governance passes to `set_anchor_head`.
5. **A final record in the reset's own block.** *Ruled as built (q28):* counted as the earlier sequence's (refused).
   *Alternative:* the adapter tells the core which entries of that block follow the reset event. That needs one
   more field in the input.
6. **The gap end across a start.** *Ruled (q28): the start clears it*, rather than keeping it (the proposal's
   cautious option, under which a withdrawal whose range starts below an earlier sequence's gap went to review).
   Read as: a start that restarts the head clears the gap end; one that keeps a head at a later block keeps it,
   because that head, and a gap recorded since, may be the new sequence's. Keeping it there is harmless: the
   kept head already passed the earlier sequence's position, so every new withdrawal's `from_seq` is at or above
   any gap end of the earlier sequence. Clearing is safe because a start is
   refused while any instruction is live (decision 2), so `proof` has nothing of the earlier sequence left to
   decide.
7. **The guard's error.** *Ruled as built (q28):* `ERR-SettlementLayerUnavailable`, with its meaning extended. *Alternative:*
   a new error condition.
8. **Who reads the head route.** *Ruled as built (q28):* the operator's principals, read-only, with no justification, like
   `GET /v1/ops/anchors`. *Alternative:* the ops token only.

## Plan and code

`F-043` carries the tasks: `T-043-13` (this design) to `T-043-18` (the rollout on test), in
`features/F-043-chained-settlement/`.
