<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0036 — The fence answers a definite "not run", so a way out can overtake a carried row

**Status:** **ruled by the owner on 2026-10-21: approved** (Decisions form, ruling w8), with the question on
`SPEC.md`'s scope answered "plan only": the second sentence of the former R1 and the former `AC-K7.23` are **not**
added to `SPEC.md`; they bind in the plan and are `T-037-18`'s own criterion. The direction was ruled on 2026-10-20
(owner ruling w7 (a), in the review of `T-037-04`, PR #47, finding B1): the core answers a refusal by the fence with
a definite "not run", and the feeder gives back the sequences it had carried. Not yet applied to `SPEC.md`: the code
and the plan cite it as `[PENDING AMENDMENT 0036]` until the pass that applies R1 and R2.
**Target:** `SPEC.md` v0.4.28 as on `develop`; `PLAN.md` `DEC-27` (d); `F-038` plan §3 and §4.
**Date:** 2026-10-20
**Number:** 0034 and 0035 are taken; this is the next free one.
**Amends:** `REQ-RT-20`, the way-out point (amendment 0035, R1), by one sentence, and `US-K7` by two acceptance
criteria. Changes the feed rule of `F-038` §3 and §4, in the plan. Nothing is withdrawn.
**Serves:** `REQ-RT-20`, `REQ-CS-7`, `NFR-17`, `INV-18`.
**Rationale in one line:** the feed rule keeps a row that may have run at the sequence it was first given, for
ever, so a fenced writer whose head row is such a row can never be sent the way out that would unfence it; the
core can say, definitely, that it did not run an input the fence refused, and then the sequence can be given back.

---

## The problem

Amendment 0035 made every way out decided, and `T-037-04` made an operator's way out overtake the forwarder's queue
in the received log. The review of that task (#47, B1) found a case the overtaking does not reach.

A row that was sent and whose answer was lost (a link that failed, a core host that restarted) is *in flight*: it
holds a feed sequence, and `F-038` §3 resends it first, with that sequence, and never gives a sequence back below
the highest position answered ambiguous. The reason is sound: the core may have run it, the core expects exactly
the next sequence, and a different input on the same sequence would be a second input at one position.

When the writer is fenced (`REQ-RT-20`), the core refuses the carried row, nothing is committed and the input
number is not taken, and it answers the feed `Unavailable`, the same answer as for any input it did not reach.
The forwarder cannot tell that answer from "the core is down, and the earlier copy may have run", so the row stays
in flight and is retried for ever. The way out:

- cannot take the carried row's sequence: two rows on one sequence;
- cannot take a later one: the core holds a gap and answers `Gap`;
- is not even sent, because the forwarder never leaves the retry of the carried row and never reads the store again.

The trigger is an ambiguous send and a fence together. Restarting the ingress does not clear it (the carried row is
rebuilt from the store with its sequence). When the fence is there because the new code cannot be run, which is the
case the way out exists for, nothing inside the system recovers it. `T-037-18` is the task, and gates enabling any
code source in `T-037-06`.

## The ruling, worked through

### The core: a definite "not run"

N1. **A new answer.** The core answers an input it refused because of the fence with `NotRun`: a `BatchAnswer` of
its own (`not_run` on the wire), carried to ingress's `Core` boundary as `CoreError::NotRun`. It is a statement
about the input at the moment of the answer: *the writer's record holds no decision for this input, it was refused
without committing anything, and its input number was not taken.*

N2. **Where it comes from, and only there.** `NotRun` is given for the Agent's refusal as fenced, after the replica
has been rebuilt to what the Agent committed, and for nothing else. A port failure, an aborted call, a failed
rebuild, a halted core and back-pressure are still `Unavailable` or `Ambiguous`, and so is an input past what one
invocation carries, unless the fence refused an earlier input of the same call (N4). The rebuild comes first: a call
whose rebuild fails is not certified `NotRun`, whatever refused it. An input the core refuses before the fence (a way
out that is malformed, `ERR-MalformedCommand`) or decides as a recorded refusal is answered with its recorded
decision, as any decided input is.

N3. **A committed input is never refused.** The record of replies is read before an input is staged, whether or not
the writer is fenced, so an input the core decided is answered `Applied`, from the record, never `NotRun`. This is the
property that makes `NotRun` definite; it is as strong as the record of replies is durable (`INV-18`), no stronger.

*Precondition, stated plainly.* The core reads the record input by input, and stops reading at the first input it
fails to stage or whose reply it cannot read; every input after that one is answered `NotRun` without its record
having been read (N4). That is safe only while **the order of a feed is the order of decision**: no later input of a
call can have been decided while an earlier one has not. It holds today, because the forwarder reads its queue in
flight first by sequence, then the way out, then reservations, then arrival, and the core runs a feed in order. Any
change to the queue's order, or to how the core runs a feed, has to keep it. One double fault gets past it: a fence
together with a reply that cannot be read from the log, in the tail of a call. This amendment does not close that case
(F2 says what the feeder then does). Reading the record for every input of the tail before answering would close it,
at the price of one more read per refused call; it is offered as a hardening, not required here.

N4. **It covers the tail.** `NotRun` is given for the first input of a call that did not commit and for every later
input of that call, whether the core staged it or not. The core takes sequences in order and gapless within an
epoch, so no later input of the feed can have committed while this one did not.

N5. **The sequencer passes it through.** `NotRun` runs nothing and takes no sequence: the expected sequence moves
only past the items answered `Applied`, as it does now. The sequencer's rules for an epoch below the highest seen
(`Fenced`) and for an early arrival (`Gap`) are unchanged. The sequencer does not recognise a duplicate by its
sequence: it hands every item of a feed under a current epoch to the core, and the core recognises a repeat by the
input's key, in the record of replies (`INV-18`). A feed's sequence is therefore only a device for ordering and for
detecting a gap. It follows that a copy of a feed under the *current* epoch runs in full, whatever its sequences have
come to mean since they were given back; only a higher epoch stops it (fact 2).

### The feeder: giving back, and the epoch

F1. **Unchanged.** A row the core may have run (carried into the call with a sequence, or answered ambiguous in any
round) is resent first, with the sequence it was first given. `Unavailable` and `Ambiguous` keep this rule exactly as
today; so do `Fenced`, `Gap` and `Refused` of the feed as a whole.

F2. **Giving back on `NotRun`.** When the answers of a send are `Applied` up to a position *p* and `NotRun` from *p*
to the end, the feeder decides the `Applied` rows as always, and gives back the feed sequences from *p*'s on, the
partition's tail, in one store transaction under its lease:

- the sequences of the undecided rows from there on are cleared, so the rows return to the queue unfed, including
  rows of the tail that were not in this send. That is safe because the forwarder reads its queue in flight first: no
  row that holds a sequence lies beyond a batch that has a fresh row in it, so every row that holds a sequence from
  *p* on is in the send or before the first fresh row, and N4 says none of them committed;
- the lease's next sequence is lowered to *p*'s sequence, so the sequences assigned stay gapless: the next row
  marked takes it;
- **if any row at or after *p* was in flight** (it arrived with a sequence, or any round answered it ambiguous), the
  lease's **epoch rises by one**, in the same transaction. The transaction returns the epoch the lease now has, and
  the feeder carries on under it.

The core gives `NotRun` followed by `Applied` only for a later input of the call whose record of replies it had
already read (N3 keeps it `Applied`), which the order of decision (N3's precondition) makes impossible except in the
double fault N3 names. A feeder that sees that mix, or any other, keeps every row in flight and gives nothing back, so
the rule fails towards the old one. Plainly: in the double fault that is the wedge this amendment exists to remove,
and the way out is not read; the amendment does not cure it.

F3. **The queue after the release.** Nothing is in flight from *p* on. The store is read as before: rows in flight
first (none), then the way out that overtakes, then reservations, then arrival. The first row marked takes *p*'s
sequence again. So the way out takes the first of the sequences returned, and the rows released take the sequences
after it, in the order they would have had: released, they are ordinary rows again.

F4. **The backoff stays.** A round in which nothing was decided waits `retry` (doubling to its maximum) before the
rows are given back, so a fenced writer with no way out costs one send, one mark and one release per backoff period,
never a spin. A round that decided a prefix gives the tail back and returns at once, as today.

F5. **Rows queued behind keep their identity.** A released row keeps its operation reference and its client key
(`REQ-OP-10`); only its feed sequence, a property of the send, changes. It is decided once, by key (`INV-18`), at
whichever sequence is the one that reaches the writer first.

### Why giving a sequence back is then safe — the invariant

> **A row's feed sequence is given back, and the row fed at another, only when the core has answered `NotRun` the
> head of the tail that is given back, in the send that gave it back (N4 covers the rest of the tail); and when an
> earlier copy of any of those rows may still be on its way to the core, the same transaction raises the epoch above
> the one that copy was sent under.**

Three facts follow, and the reason for each is the whole case for the rule:

1. **The row is undecided at the moment of the answer** (N3, under its precondition): the record of replies was read
   first. So giving the sequence back loses no decision.
2. **No earlier copy can run after the next feed has reached the core.** The copy of the row that the earlier,
   ambiguous send left in the core's hands (a host finishes a feed it has been handed even if its stream is gone) is
   under an older epoch, and the sequencer refuses an epoch below the highest it has seen (`Fenced`), under the
   partition's lock and before anything reaches the core, running nothing. The epoch is inside the signed message, so
   a copy cannot be replayed under a higher one. A copy that reaches the core *before* the next feed still has its
   epoch current, and the core runs all of it, in the order the feeder composed it, which is queue order, so the rows
   it decides are the oldest undecided ones; each is then answered from the record when it is fed again. What a
   sequence number cannot do is stop that copy: the sequencer does not drop an item for its sequence (N5), so after
   a sequence is given back and reused, a copy under the same epoch would still run in full, in whatever state the
   rows now are, including after rows logged behind it have been decided. That is why the epoch rises, and why the
   rise is required rather than a refinement.
3. **Each row is decided at most once, by key,** whichever copy reaches the writer first (`INV-18`), and at least
   once, because it stays in the received log and the forwarder reads it until it is decided (`REQ-CS-7`).

*Trade-off, stated plainly:* the rule is as strong as the core's `NotRun`. A core that answered `NotRun` for an input
it had committed, or that commits one it has answered `NotRun`, would let a sequence be reused for another input; the
row would still be decided once, by key, but not in its place. The rule therefore puts a duty on the core (N1 to N3)
that no other answer carries, and the code that builds it must show N3 by test. Further:

- The epoch rises in a third case. It rose on every change of holder; it now also rises when the holder gives back
  rows that were in flight. Nothing else reads the epoch as "the holder changed": the sequencer rebases on any higher
  epoch, as it does after a change of holder, and the signed message already binds it.
- A released row **is** fed at more than one sequence, and while the writer stays fenced with no valid way out it can
  be released repeatedly. The `t_038_03` invariant, "an input is never fed at two sequences", still holds for every row
  the core has not certified `NotRun`, which is every row today's tests exercise, and it is replaced for a certified
  row by "decided exactly once, by key".
- A released row loses its place to the way out (by design) and to any reservation logged meanwhile (as every unfed
  row does, `T-009-04`): the "in flight keeps its place ahead of a reservation" promise of `F-038` §3 holds until the
  core says the row did not run.
- Each backoff period while fenced costs a send, a mark and a release in ingress's database, and, once, the epoch rise.
- **An ingress that does not know `not_run` fails harder than "ambiguous".** On the stream link it cannot decode the
  frame, its read loop ends and the **whole session** is dropped: the feeds in flight fail as ambiguous, the reads as
  unavailable, the core host dials again, and the next fenced feed does the same. On the legacy socket link an
  undecodable response reads as the **whole batch ambiguous**, and the forwarder keeps ambiguous rows in flight for
  ever, so a fenced writer wedges where it did not before. The core only gives `NotRun` while fenced, and a fence needs
  a code source, which `T-037-06` enables and this task gates. The safe order is therefore: **ingress is released
  first, then the core hosts** (`F-038` §12 already orders it so), and **ingress is not rolled back below this release
  while a core host speaks it**. The alternative, raising the stream's version, is Decision 3.
- A single call and the legacy link read `NotRun` as `Unavailable`, as they read every unrun input today; only the
  sequenced feed gives sequences back.

### The cases

| Case | What happens |
|---|---|
| Carried head row, fenced writer, way out logged behind it | The carried row's resend is answered `NotRun`; its sequence and any after it are given back and the epoch rises; the way out is read first, takes that sequence and is decided as the writer's next input; the fence is lifted (a cancel) or the upgrade moved; the released rows are fed after it, once each. |
| Ordinary rows queued behind the carried one | Released with it, unfed, in the queue behind the way out and the reservations logged since; fed again afterwards at the sequences following the way out's. |
| Prefix committed, the rest fenced | The prefix is `Applied` and decided; the first refused input and everything after it is `NotRun`; the tail is given back from there. The sequences of the decided prefix are never given back. |
| Ingress restarts between the `NotRun` answer and the release | Nothing was written: the rows still carry their sequences, are rebuilt as carried, resent, answered `NotRun` again and released. The release is one transaction, so a restart after it finds either the rows unfed under the new epoch or, if it did not commit, the rows as they were. |
| The release fails (the store errors, or the lease moved) | The rows keep their sequences, which is safe: they are resent first. A moved lease stops the feeder as it does today. |
| Core restarts between the release and the next feed | Stale copies die with the process. The sequencer's position is gone; it adopts the next feed's epoch and rebases on its first sequence. The record of replies is durable, so a row decided before the restart is answered from it. If the fence outlasts the restart, the way out runs; if it does not, every row runs in log order. |
| The way out itself is answered ambiguous | It is in flight like any row: resent first with its sequence. The fence takes it, so it is decided (or answered from the record, if the first copy committed). It is never answered `NotRun`; if it were, it would be released like any row and still be read first (`overtakes`). The rows behind it are fed after it, as now. |
| A stale copy of a released row is still in the core's hands | It is under an older epoch: `Fenced`, nothing runs. If it arrives before the next feed and the fence has been lifted meanwhile, it runs in log order and the row is answered from the record after. |
| Two ingress instances | Only the lease's holder feeds. A holder that loses the lease while releasing is refused by the store (`fenced`) and stops; the new holder rebuilds the queue from the rows' sequences, which the release either cleared or left. |
| The legacy socket link | Unsequenced, no feed sequences: `NotRun` is read as `Unavailable`, which it was before. Nothing changes (`F-038` §12 step 0). |

## R1 — `REQ-RT-20`, the way-out point

Add, at the end of the way-out point as amendment 0035 left it:

> A way out MUST reach a fenced writer even when commands logged before it were being handed to it when the
> handing over was interrupted, so that it is not known whether the writer decided them. *Amendment 0036.*

## R2 — `US-K7`, two acceptance criteria

Add after `AC-K7.20`:

> - `AC-K7.21` Given a writer fenced at input *n*, and a command logged before a way out whose handing to the
>   writer was interrupted so that it is not known whether the writer decided it, When the way out is logged, Then
>   it is decided as input *n*, whether or not the command was interrupted, and the upgrade is moved or cancelled as
>   it asks (`REQ-RT-20`, `REQ-CS-7`).
> - `AC-K7.22` Given the commands that a fenced writer refused when the way out overtook them, When the way out is
>   decided and the fence is gone, Then each of them is decided exactly once, none is lost and none is decided twice,
>   in the order in which they were received, after the way out (`REQ-CS-7`, `INV-18`).

## The criterion that stays in the plan (owner ruling w8)

What the writer answers the component that feeds it is a boundary between ingress and the core, which `SPEC.md`
leaves to the plan (`CLAUDE.md`, guardrail 1). It is therefore not in `SPEC.md`; it is a criterion of `T-037-18`:

> **`T-037-18` criterion (amendment 0036, N1 to N4).** Given a command the writer decided, When it reaches the writer
> again, whether or not the writer is fenced, Then it is answered with that decision and never with "not run"; and
> Given a command a fenced writer refused, Then it is answered "not run", nothing of it is recorded and its input
> number is not taken (`REQ-RT-20`, `INV-18`).

## What does not change

The fence itself (`REQ-RT-20`, D8) and its refusal: nothing committed, the input's number not taken. The way-out
rules of amendment 0035 (every way out decided, an input of its own, the numbering). The feed lease, its holder, its
renewals and its expiry; the gapless feed sequences; the signed feed and its epoch binding; the sequencer's fencing of
a stale epoch, its hold of an early arrival and its answer to a duplicate. The rule for `Unavailable`, `Ambiguous`,
`Fenced`, `Gap` and `Refused`. The legacy link. The `t_038_03` and `t_038_04` tests pass unchanged: none of them
involves `NotRun`.

## Plan

`F-038` plan §3 (the queue, the give-back, the trade-off) and §4 (the answer of a fenced input) carry the rules above,
marked `[PENDING AMENDMENT 0036]`. `T-037-18` builds them: the core's answer (`core_binding::core`,
`ingress::boundary`, `ingress::remote`, `ingress::feed`), the store's release with the epoch (`MemStore`, `PgStore`),
and the forwarder (`ingress::service`). Its acceptance test is
`ac_k7_12_a_way_out_overtakes_a_row_carried_from_an_ambiguous_send` (`AC-K7.21`), without `#[ignore]`; the criterion
above is shown by `T-037-18`'s own tests, which name `T-037-18` and not an `AC-*` that `SPEC.md` does not hold.

## Decisions (approved with the text)

1. **The epoch rises on a release of in-flight rows (F2).** The alternative is key idempotency alone: no epoch change.
   Then nothing stops a late copy of a released row, which runs in full under the old epoch whatever rows now hold its
   sequences, so only the key would keep order, and a row could be decided out of the received log's order in a narrow
   window (after the fence has lifted and rows logged behind it have run). It is the smaller change; the amendment takes
   the epoch because "out of order" is a guarantee `F-038` makes (`DEC-27` (d)).
2. **`NotRun` covers the tail (N4)** instead of the one refused input, so a feeder never gives back a sequence while
   a later sequence stays carried.
3. **The stream's version is not raised for the new answer.** The argument is the release order
   and the gate on `T-037-06`, plus the runbook rule "never roll ingress back below this release while a core host
   speaks it". Raising it would have given an old ingress a clean refusal at the first message instead of a dropped
   session, at the cost of a version negotiation for an answer that cannot occur until a code source is enabled.
4. **`NotRun` on a single call and on the legacy link reads as `Unavailable`** (behaviour, so it is stated here):
   only the sequenced feed gives sequences back. The store gains a method of its own for the release
   (`unfeed_in_flight`) rather than changing `unfeed`; that is an implementation choice, recorded here so it is not a
   silent one.

## Not chosen

Ruling w7 (b): the sequencer takes a way-out input out of sequence, as an input that holds no sequence and is
idempotent by its key. It leaves the feed rule as it is and moves the exception into the core host, which would then
run an input outside the order the received log fixes, for the one input class that most needs to be in it.
