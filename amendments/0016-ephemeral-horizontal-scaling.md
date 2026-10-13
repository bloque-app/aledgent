<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0016 — ephemeral horizontal scaling

**Status:** ruled by the owner on 2026-10-04: allowing ephemeral horizontal scaling is a non-functional
requirement. No question is left open.
Applied to `SPEC.md` v0.4.10.
**Target:** `SPEC.md` v0.4.9 (amendments 0001 to 0015 applied); this one takes it to **v0.4.10**.
**Date:** 2026-10-04
**Plan:** `DEC-22` ("Replication"), `DEC-23` (every Agent Shared), `F-021`, `DEP-11`.
**Rationale in one line:** the plan already runs the book on replicas that keep nothing of their own and can
be added and replaced at will, but the spec never asked for it; a property the operator depends on to scale
and to survive a lost machine belongs in the spec, where no plan can quietly drop it.

---

## The problem

The ability to scale out on disposable machines exists only at plan level. `DEC-22` says the core's
replicas carry no persistent volume and resynchronise from a certified copy of the state plus the log's
tail after admission; `F-021` adds voters and such replicas. Nothing in `SPEC.md` requires any of it:

- `NFR-5` asks only that aledgent be deployable without an outage of the book. A deployment pinned to
  one machine with one disk meets it, through a buffered restart.
- `NFR-4` and `NFR-10` say no acknowledged operation is lost and that a restore works. They say nothing
  about losing a machine without a restore.
- `NFR-17` makes every change after the initial load an online one, and `NFR-18` makes the design scale by
  adding partitions. Neither says that capacity may be added by adding machines, nor that any one of them
  may disappear.

So a plan could pin the book to a durable machine and remain conforming. The owner's ruling makes that a
non-conformance.

## What does not change

- **One book** (`INV-25`), partitions (`REQ-RT-9`) and the messages between writers (`REQ-RT-13`).
  Execution nodes are where writers' state is held and committed; they add no writer and split nothing.
- **The latency bounds** (`NFR-1`, `NFR-15`, `NFR-16`) and **no loss** (`NFR-4`). This amendment makes
  them hold through a node's admission and a node's loss as well.
- **Restores** (`NFR-4`, `NFR-10`). They remain the answer to losing a quorum, or the whole deployment.
- **No technology is named.** "Execution node" is defined by what it holds and what it may lose, not by
  what it runs on.

---

## R1 — `NFR-21`: ephemeral horizontal scaling (new, §11, after `NFR-20`)

> | `NFR-21` | **Ephemeral horizontal scaling.** aledgent runs on execution nodes that can be added, replaced and
> lost without an outage of the book.
> (a) **Scale out.** aledgent's capacity MUST be increasable by adding execution nodes, with no change to the
> book, to any identifier in it or to any operation, and online as `NFR-17` requires: every balance
> unchanged, and every operation decided as usual or refused under its own named condition (§10).
> (b) **Replace.** Any execution node MUST be replaceable by a new one that starts with no local state. The
> new node is admitted online and brought up to date from the nodes already serving and from the
> deployment's durable copies, with no operation that aledgent has acknowledged or answered with its
> decision lost (`NFR-4`) and no outage of the book (`NFR-5`): while a node is admitted, and after one is
> lost, every command is still acknowledged within `NFR-1` or answered within `NFR-16`, and completes within
> `NFR-15`.
> (c) **Lose local storage.** The loss of the local storage of any single execution node, at any instant,
> MUST NOT lose an operation that aledgent has acknowledged or answered with its decision. An operation is
> therefore acknowledged or answered only once the record it rests on (the command taken in, or the commit
> that decided it) is durable on a **quorum** of the nodes that hold that record: at least two of them, and
> a majority.
> *Bounds, measured rather than committed (the owner, 2026-10-04).* Each deployment states how many nodes
> hold each writer's state and its quorum; the design value is three and two. The stress suite measures,
> and reports on every release with `NFR-18`: the time to admit a new node and bring it up to date at the
> book's size (design value: 15 minutes at `NFR-2`'s volume); the time a writer takes to resume deciding
> after losing the node that orders its commands (design value: 10 s); and the decision rate while a node
> is admitted. Missing a design value is reported, not a breach; `NFR-1`, `NFR-15` and `NFR-16` still bind
> throughout.
> *Ruled by the owner on 2026-10-04 (amendment 0016).* |

**The bounds, and why these.** Like `NFR-18`, the requirement is the property, and the numbers are measured
and reported, not committed:

- *The quorum* is the one structural bound, and it follows from (c): if one node's acknowledgement were
  enough, losing that node could lose the operation. Two of three is the smallest group in which any one
  node can be lost, and a majority keeps two groups from deciding apart.
- *Admission time* is measured at `NFR-2`'s volume because the book's size, not load, sets how long a new
  node takes to copy its writer's state. Fifteen minutes is a design value: long enough to copy a writer's
  state at that volume, short enough that a deployment of three is rarely down to two for long.
- *Resumption time* is how long a writer stops deciding after losing the node that orders its commands.
  Its design value, 10 s, sits inside `NFR-15`'s 30 s ceiling, which still binds; commands keep being
  acknowledged meanwhile (`REQ-AR-7`).

## R2 — Glossary: *Execution node* (new, §2, after *Consistent cut*)

> | **Execution node** | One running instance of aledgent, with its own local storage, that holds a copy of
> the state of one or more of the book's writers (`REQ-RT-13`) and takes part in committing it. Execution
> nodes are interchangeable and disposable: none holds anything that is not also held elsewhere, and none
> is part of the book's identity (`NFR-21`). |

## R3 — `US-K6` (new, Epic K, after `US-K5`)

The requirement serves the operator, who adds capacity and loses machines, and through it every holder and
integrating party, who see no outage (`US-M2` to `US-M6` depend on it as on `NFR-5`). It gets a story of its
own so that it is tested, not only stated:

> **`US-K6`** — As the **operator**, I add capacity, replace any execution node, or lose one, without an
> outage of the book and without losing an operation.
> - `AC-K6.1` Given a deployment under load, When I add execution nodes, Then its capacity grows with no
>   change to the book, to any identifier in it or to any operation. Every balance is unchanged, and every
>   command is acknowledged, answered and completed within its bound (`NFR-21` (a), `NFR-17`, `NFR-18`).
> - `AC-K6.2` Given a deployment under load, When I replace any execution node with a new one that starts
>   with no local state, Then the new node is admitted online and brought up to date. No operation that was
>   acknowledged or answered with its decision is missing, and every command is acknowledged, answered and
>   completed within its bound (`NFR-21` (b), `NFR-4`, `NFR-5`).
> - `AC-K6.3` Given a deployment under load, When the local storage of any single execution node is
>   destroyed at an arbitrary instant, Then no operation that was acknowledged or answered with its decision
>   is missing, every balance re-derives from entries alone, and no restore from backup was needed
>   (`NFR-21` (c), `NFR-4`, `INV-6`).

## R4 — §11: a node's loss is not a restore, and the trade-off

After the paragraph on latency, §11 gains:

> Losing an execution node is not a restore. `NFR-21` keeps the book whole and online through the loss of
> any one node; a restore (`NFR-4`, `NFR-10`) is for the loss of a quorum, or of the whole deployment.

After the table:

> **Trade-off, stated plainly.** `NFR-21` (c) puts a round trip between execution nodes inside every
> acknowledgement and every decided answer: `NFR-1` and `NFR-16` are spent partly on it. A node's local
> storage is a cache, so a new node starts by copying its writer's state from the others, and the book's
> size sets how long that takes. And a quorum of two out of three tolerates the loss of one node at a time,
> not two: losing a quorum is a restore (`NFR-10`), with the outage that implies.

## R5 — §15

Epic K's stories become `US-K1`–`US-K6`, in the traceability table and in V0's release scope. `US-K6` is
V0: the requirement is not deferred.

---

## What this costs, stated plainly

- **At the design value, three copies of every writer's state**, and a quorum's round trip in every acknowledgement and decided
  answer.
- **A test of node loss under load** on every release, in the stress suite: adding, replacing and killing
  nodes, with the three measurements reported.
- **A plan dependency that is not met today.** Admitting a fresh node online and certifying a copy of a
  writer's state that a new node can start from are not yet available in the runtime the plan binds (see
  below). That is a gap in the plan, which the plan carries as a dependency. It does not weaken this
  requirement.

## Carried to the plan

`DEC-22` ("Replication"), `DEC-23` (every Agent Shared) and `F-021` bind `NFR-21`. The plan-level
realisation is the one `DEC-22` already chose: replicas that are ephemeral and carry no persistent volume,
admitted by a membership change, installing the latest quorum-certified checkpoint and replaying the log's
tail; capacity added by more shards and more replicas per shard. What is not yet satisfiable on the runtime
as released, membership change on a Shared Agent and portable, quorum-certified checkpoints a new replica
can install, is `DEP-11`'s; `F-021` closes it once those land. Until then a deployment with one voter per
Agent scales out only by adding shards (`F-033`'s online splits), and meets neither (b) nor (c): the plan
says so, as a dependency, and the requirement stands. `F-021` owns the three pending conformance cases of `US-K6`.
