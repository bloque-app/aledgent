<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0015 — a consistent cut of the book

**Status:** proposed on 2026-10-03, after the architecture review's follow-up ("there is no rule: the
prototype anchors only between idle steps, and binds compare the book's total with the settlement layer,
which across partitions needs a cut"). Where the review left a choice open, this takes the conservative
default and names its question (`gc-05` to `gc-08`). Applied to `SPEC.md` v0.4.9.
**Target:** `SPEC.md` v0.4.8 (amendments 0001 to 0014 applied); this one takes it to **v0.4.9**.
**Date:** 2026-10-03
**Plan:** `DEC-23` (the anchor Agent's assembly, the settlement Agent's binding), `F-031` (`crates/shards`),
`F-033` (`T-033-11`).
**Rationale in one line:** with several writers, "the book at an instant" is a choice of one commit per
writer; some choices count a transfer twice or not at all, so the spec must say which choices are allowed.

---

## The problem

Amendment 0012 held the book in partitions and made a transfer between two of them two commits: the
payer's partition commits the debit to the in-transit position, the payee's applies the credit later
(`REQ-RT-11`). Amendment 0014 added the registry, the settlement record and the audit trail as writers,
with messages between them (`REQ-RT-13`), and an anchor that commits to "a stated sequence of each
partition", the registry at a stated version and the trail at a stated sequence of each writer (`INV-19`).

Nothing says which combinations of those sequences an anchor, a reconciliation or a binding may use:

- Take the payer's partition **before** a debit and the payee's **after** its credit: the value is counted
  at both, and the book appears to hold more than was issued.
- Take any combination and add up only the holders: a credit in flight is in neither, and value appears
  lost. The in-transit positions fix that only if both partitions' views agree on which credits are in
  flight.
- `REQ-AS-8` makes a binding succeed only if the settlement layer holds "the book's outstanding balance".
  With partitions that balance is a sum over writers read at different times, while issuance can still move
  in the partitions that have not yet learnt that the asset is being bound.
- `NFR-9`'s second axis compares the in-transit position with "the credits enqueued and not yet applied",
  which is only defined against one view of both ends.

The prototype avoids all of this by anchoring when nothing is in flight. A book under load is never idle.

## What does not change

- **One book** (`INV-25`), **exactly once** (`INV-18`, `REQ-RT-13`), **balances** (`INV-1` to `INV-7`),
  hourly anchoring (`REQ-PR-1`) and what an anchor proves (`REQ-PR-3`).
- **No coordination on the write path.** A cut is read from records the writers already commit; no writer
  waits for another, and nothing stops, drains or pauses to take one.

---

## R1 — `REQ-RT-14`: a consistent cut of the book (new, §4.4 (b), after `REQ-RT-13`)

> **`REQ-RT-14`** **A consistent cut.** When the book has more than one writer (`REQ-RT-13`), a **cut** names
> one commit of each writer. In the commit it names, each writer's audit record carries its **stream heads**:
> for each other writer and each kind of message between them, the last sequence it has enqueued for that
> writer, the last it has applied from it, and the last that writer has acknowledged, up to which it has
> dropped what it sent.
>
> A cut is **consistent** when, for every ordered pair of writers and every kind of message from the first to
> the second:
>
> - the receiver's commit in the cut has applied no message that the sender's commit in the cut had not
>   enqueued; and
> - the sender's commit in the cut still holds every message that the receiver's commit in the cut had not
>   applied.
>
> The messages enqueued in a consistent cut and not applied in it are **in transit at the cut**. A credit in
> transit is counted once, in the in-transit position (`REQ-RT-11`), and nowhere else.
>
> An anchor (`INV-19`), the second axis of reconciliation (`NFR-9`) and a binding's zero divergence
> (`REQ-AS-8`) are each taken at a consistent cut. A cut MUST be decidable from the stream heads alone, and
> taking one MUST NOT require any writer to stop, drain, wait or be idle. Of the consistent cuts available,
> the one used is the latest, so that a cut trails the writers' newest commits by about one message delay.

*Trade-off, stated plainly.* A cut is a little behind now: under load, about one message delay (`NFR-19`'s
order). An anchor therefore commits to a state some seconds old, which hourly anchoring makes immaterial.
And a writer whose audit records stop arriving holds every later cut back to its last record: the trail shows
it as a gap (`REQ-AT-2`), and the anchor's lag grows until it reaches `NFR-11`'s alert. That is the price
of never stopping the book to anchor it.

## R2 — `INV-19` (revised): one anchor, one consistent cut

Before:

> An anchor commits to every entry up to and including a stated sequence of each partition (`REQ-RT-9`), to
> the registry of identities, credentials and grants at a stated version, and to the audit trail up to a
> stated sequence of each writer (`REQ-RT-13`), and to nothing else.

After:

> An anchor commits, at one consistent cut (`REQ-RT-14`), to every entry up to and including the cut's commit
> of each partition (`REQ-RT-9`), to the registry of identities, credentials and grants at the cut's
> version, to the audit trail up to the cut's sequence of each writer (`REQ-RT-13`), and to each writer's
> stream heads at the cut, and to nothing else.

The heads are in the anchored value so that a verifier holding the anchor's inputs can check the cut was
consistent without aledgent's help (`REQ-PR-4`). They are sequence numbers: no account, amount or identity
(`REQ-PR-2`, `INV-17`).

## R3 — `REQ-AT-2` (revised): the trail carries the heads

The paragraph amendment 0014 added gains one sentence:

> Each writer's record of a commit carries its stream heads at that commit (`REQ-RT-14`).

## R4 — `REQ-AS-8` (revised): a binding across partitions

A paragraph is added after the list of conditions:

> When the book is held in partitions (`REQ-RT-9`), the asset's `Binding` state reaches every partition as a
> message from the registry (`REQ-RT-13`). Each partition refuses the asset's issuance and redemption from the
> commit in which it applies that state, and in that commit reports its issuance counterparty's position in
> the asset. The outstanding balance is taken at the latest consistent cut (`REQ-RT-14`) in which every
> partition has applied the state, and is the negation of the sum of those positions: equal, to the minor
> unit, to the sum of every holder's balance in the asset at that cut plus the value in transit at it. The
> binding asks the settlement layer to hold that amount.

*Trade-off, stated plainly.* Between the registry recording `Binding` and the last partition applying it,
the partitions that have not yet applied it can still issue and redeem. Those operations are decided, final
and included in the amount; nothing is lost and nothing is counted twice, but "issuance is refused while the
asset is `Binding`" takes effect partition by partition, within the delay of a registry message. This
amendment holds that delay to `NFR-20`'s bound, as for a revocation (`gc-05`).

## R5 — `NFR-9` (revised): the second axis at a cut

In `NFR-9`, "the in-transit position of each asset, which MUST equal the credits enqueued and not yet
applied (`REQ-RT-11`)" becomes:

> the in-transit position of each asset, which MUST equal the credits in transit at a consistent cut
> (`REQ-RT-11`, `REQ-RT-14`)

---

## What this costs, stated plainly

- **Every audit record grows by its heads**: three sequence numbers per other writer and kind. Bounded by
  the number of writers, not by load.
- **The anchor writer keeps a window of records** from its last anchored cut to the newest, per writer, to
  find the latest consistent cut. An hour of records between anchors at one record per commit.
- **A stalled writer stalls anchoring** (R1's trade-off). It is visible, and it is what the trail's gap
  already signals.
- **A binding waits** for one consistent cut past the last partition's application of `Binding`: about one
  registry delay plus one message delay.

## Carried to the plan

`DEC-23` binds this amendment: each shard's round commits its heads in its `audit/<n>` record; the anchor
Agent keeps each writer's records since its last anchor and assembles the latest consistent cut by moving
back the record that is ahead until no pair breaks the condition; the settlement Agent takes a binding's
amount from the shards' shares at a cut the anchor Agent sends it on `anchor`. `crates/shards` implements
the heads, the assembler, the anchor at a live cut and the binding's freeze and shares, with property tests
under duplication, reordering, dropped heads, lost acknowledgements and a lagging relay
(`crates/shards/tests/cut.rs`).
