<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0014 — the book's writers, and the messages between them

**Status:** the direction was ruled by the owner on 2026-10-03: a set of Agents, each with its own
responsibility, shards the system's work. The points the ruling leaves open carry a conservative default
and wait for the owner: each names its question (`r-01` to `r-11`). Applied to `SPEC.md` v0.4.8.
**Target:** `SPEC.md` v0.4.7 (amendments 0001 to 0013 applied); this one takes it to **v0.4.8**.
**Date:** 2026-10-03
**Plan:** `DEC-23`, `F-033`.
**Rationale in one line:** amendment 0012 split the book's state among partitions and put identities,
credentials and grants apart from them; the settlement record and the audit trail now follow. A book
with several writers needs one rule for what passes between them, and that rule belongs in the spec.

---

## The problem

Amendment 0012 made a partition's writer the unit of commit (`REQ-RT-1`) and gave one kind of message
between writers its rule: a credit is enqueued in its debit's commit and applied in the payee's partition
exactly once (`REQ-RT-11`). It left the registry of identities, credentials and grants apart from the
partitions, consulted by them, with a revocation that takes effect within a bound (`REQ-AU-3`, `NFR-20`).

The plan now holds four kinds of state in separate writers: the partitions, the registry, the record of
settlement instructions and their observations, and the audit trail with its anchors. Each keeps its own
commit boundary, and each receives what the others decide: credits, revocations and account openings,
settlement instructions and the evidence that comes back, audit records and the roots an anchor commits
to. The spec says how a credit travels. It says nothing about the others, and two of its requirements read
as if there were one writer:

- `REQ-AT-2` keeps the audit trail apart from operational storage, which a separate writer does. It does
  not say how a record reaches a trail kept elsewhere, or how a missing one would show.
- `INV-19` makes an anchor commit to each partition's entries. The registry is the book's, not a
  partition's (`REQ-RT-9`), so as written no anchor covers a grant or a revocation.

## What does not change

- **One book** (`INV-25`). Several writers are one book: one registry, one trial balance, one sequence of
  anchors, one audit trail.
- **Exactly once** (`INV-18`, `REQ-RT-2`), **balances** (`INV-1` to `INV-7`), **no external leg before
  finality** (`INV-10`), and every bound already stated (`NFR-19`, `NFR-20`).

---

## R1 — `REQ-RT-1` (revised): the boundary encloses every message owed

The last item of the list becomes:

> - the enqueuing of every message the commit owes another of the book's writers (`REQ-RT-13`), a credit
>   to another partition (`REQ-RT-11`) among them.

## R2 — Messages between the book's writers (new, §4.4 (b), after `REQ-RT-12`)

> **`REQ-RT-13`** **The book's writers and the messages between them.** When the book's state is held by
> more than one writer (each partition's (`REQ-RT-9`), the registry of identities, credentials and grants,
> the record of settlement instructions and their observations, and the audit trail with its anchors), a
> message from one writer to another:
>
> - is enqueued in the sender's commit (`REQ-RT-1`), numbered in sequence per sender, receiver and kind;
> - MUST be applied by its receiver **exactly once and in its sequence**: a repeat changes nothing, and a
>   message after a gap waits until the gap is filled (`INV-18`);
> - is applied inside the receiver's own commit boundary, together with everything it causes there;
> - is dropped by its sender only after the receiver has applied it.
>
> A writer decides from its own state and the messages it has applied, never by reading another writer's
> state. Each kind's delay, from the sender's commit to the receiver's, is bounded where this specification
> bounds it (`NFR-19`, `NFR-20`) and is otherwise measured and reported with `NFR-18`.

*Trade-off, stated plainly.* Between two writers there is always a delay: the receiver acts on what the
sender decided some time ago. A shard refuses a revoked grant only once the revocation has reached it
(`NFR-20`); a settlement instruction leaves only once the settlement record has applied it; the audit trail
holds a partition's record only once it has arrived. The spec bounds the delays that a holder can feel, and
the plan measures the rest.

## R3 — `REQ-AT-2` (revised): one trail fed by every writer

`REQ-AT-2` gains a paragraph:

> When the book has more than one writer, each writer's audit records reach the one audit trail as messages
> (`REQ-RT-13`), in that writer's sequence. The trail records, for every writer, the last sequence it holds,
> so that a record not yet arrived is a visible gap, never a silent one.

*Trade-off, stated plainly.* A record exists in its writer's commit at once, and in the trail after the
message's delay. A reader of the trail sees each writer up to its own last sequence, which the trail shows.

## R4 — `INV-19` (revised): an anchor covers the registry and the trail

> **`INV-19`** An anchor commits to every entry up to and including a stated sequence of each partition
> (`REQ-RT-9`), to the registry of identities, credentials and grants at a stated version, and to the audit
> trail up to a stated sequence of each writer, and to nothing else.

`REQ-PR-1` is unchanged: an anchor is a bounded-size commitment, now over all of these at one cut. A
disclosure about a grant or a revocation proves it against the registry's state at its version, and that
state against the anchor (default for `r-08`).

---

## What this costs, stated plainly

- **More delays to watch.** Every kind of message has a lag, and each one is a signal to operate (`NFR-11`).
- **A gap is visible.** The trail shows each writer's last sequence; operators see a stalled writer as a gap.
- **Anchors grow by two roots.** The registry's and the trail's, beside each partition's: bounded, and
  committed once an hour (`REQ-PR-1`).

## Carried to the plan

`DEC-23` binds this amendment: the book as a set of Agents (book shards, the principals Agent, a
settlement Agent and an anchor Agent), all pinned to aledgent's own runtime, which applies every message of
`REQ-RT-13` exactly once and in sequence as part of the receiver's transition. `F-033` builds the runtime
and measures it against VOS's standard one.
