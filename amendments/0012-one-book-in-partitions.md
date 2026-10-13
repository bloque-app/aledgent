<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0012 — one book in partitions: throughput for agents at scale

**Status:** the direction was ruled by the owner on 2026-10-03 (with `OQ-17`; and, the same day,
`NFR-18` measured rather than targeted, and the core Shared with one voter at first). The numbers and the
points the rulings leave open carry a conservative default and wait for the owner: each names its
question (`c-01` to `c-12`). Applied to `SPEC.md` v0.4.6.
**Target:** `SPEC.md` v0.4.5 (amendment 0011 applied); this one takes it to **v0.4.6**.
**Date:** 2026-10-03
**Plan:** `DEC-22`, `F-031`.
**Rationale in one line:** aledgent's customers include agents, and the owner's target is millions of them
acting at once. One serial commit boundary for the whole book cannot carry that. The book stays one book,
but its state is held in partitions of accounts, each with its own commit boundary: a transfer is final when
its payer's partition commits it, and its credit follows, guaranteed.

---

## The problem

`REQ-RT-1` asks for "a single durable commit boundary" around every posting and its consequences, and
`INV-25` asks for exactly one book. Read together, and as the plan has built them, every command of the
deployment passes through one serial writer. The rate of that writer is the rate of the book.

That rate is far below the owner's target. The figures are in the plan's notes, not here (`notes/`,
`F-031`). In short, one writer decides a few commands per second, and the target is millions of concurrently
active agent principals. Making the writer faster helps. Batching many commands into one commit and making
each command's cost proportional to the state it touches are plan matters, and the plan takes both
(`DEC-22`). They do not remove the ceiling: one serial writer still orders commands that do not need
ordering.

Most of them do not. Two operations on **different payers** never need an order between them. Credits
**commute**: crediting an account twice gives the same result in either order. Only an operation that
**debits** an account or changes its **reservations** must be ordered against the others on that account,
because each one checks `available` and changes it (`REQ-BA-4`, `INV-3`).

## What does not change

- **One book** (`INV-25`). One registry, one trial balance, one sequence of anchors, one audit trail.
- **Balances** (`INV-1` to `INV-7`). Every commit balances on its own, in every partition, at every instant.
- **Exactly once** (`INV-18`, `REQ-RT-2`).
- **No external leg before finality** (`INV-10`), and every settlement-layer rule (`REQ-SL-*`).
- **Reservations** (`REQ-BA-4`): taken at the instant they are accepted, against the account's partition.

---

## R1 — Partitions (new, §4.4 (b), after `REQ-RT-8`)

> **`REQ-RT-9`** **Partitions.** The book's state MAY be held in **partitions**. Each ledger account belongs
> to exactly one partition, fixed by a deterministic function of the account's identifier when it is opened.
>
> - Everything that is ordered against an account lives in its partition: its balances in every asset and
>   kind, its reservations, and every operation it pays.
> - The partition is part of the identifiers of what it holds, so that a later change in how many partitions
>   there are moves state and never renames it. No identifier changes when the book is re-partitioned.
> - Holding the book in partitions does not split it (`INV-25`). The registry, the roles, the trial balance,
>   the anchors and the audit trail are the book's, not a partition's.

> **`REQ-RT-10`** **Ordering per payer.** Every operation that debits an account, or takes, captures or
> releases a reservation on it, MUST be ordered against every other such operation on that account: their
> effects are those of some serial order, and each sees the effects of those before it. Operations on
> different accounts need no order. A credit needs no order: credits to one account MAY apply in any order.

*Trade-off, stated plainly.* A partition is still a serial writer, for its own accounts. An account that
receives debits faster than one writer can decide them is a hot spot. `REQ-RT-12` says how such an account is
held.

## R2 — `REQ-RT-1` (revised): the commit boundary is a partition's

The requirement becomes:

> **`REQ-RT-1`** The runtime MUST provide, **for each partition** (`REQ-RT-9`), a single durable commit
> boundary that encloses:
>
> - the entries of a posting;
> - the operation record's state change;
> - the evidence that caused it;
> - the enqueuing of any notification;
> - the enqueuing of any credit that the posting owes another partition (`REQ-RT-11`).
>
> Nothing in that set may commit without the rest. When the book has one partition, this is the single
> boundary it always was.

## R3 — Transfers between partitions (new)

> **`REQ-RT-11`** **A transfer is final when its payer's partition commits it; its credit is guaranteed.**
> When the payer and the payee of a transfer, or of a capture, are in different partitions:
>
> - The payer's partition decides the transfer and commits the **debit**. The posting moves the value from
>   the payer to the book's **in-transit position** for the asset, and enqueues the credit (`REQ-RT-1`). From
>   that commit the transfer is **final**: its operation reaches its terminal state, and nothing undoes it
>   except a new operation (`INV-2`, `INV-21`).
> - The **credit** MUST then be applied in the payee's partition **exactly once** (`INV-18`), within the
>   bound of `NFR-19`. Its posting moves the value from the in-transit position to the payee. It MUST NOT be
>   refused: an account that exists can be credited (`INV-9`), and every check the transfer needs is made
>   by the payer's partition before the debit commits.
> - The in-transit position is part of the book. The trial balance includes it and is exactly zero at every
>   commit of every partition (`INV-5`). Its balance for an asset is the sum of the credits enqueued and not
>   yet applied, and reconciliation checks exactly that (`NFR-9`).
> - The payee's view, its statement and its notification follow the credit, not the debit. The payer's
>   follow the debit.

*Trade-off, stated plainly.* For up to `NFR-19`'s bound, the value is neither the payer's nor the payee's: it
is in transit, and both can see that. A payee cannot spend a credit before it applies. "Final" means final
for the payer and for the book; it does not mean that the payee can already spend it. When payer and payee
share a partition, nothing changes: both sides commit in one posting.

**The payer's checks.** Before a debit commits, the payer's partition checks that the payee exists and can
hold the asset. An account's existence and an asset's registration only ever become true and stay true:
accounts are never deleted (`REQ-LA-4`), and assets are never removed from the registry (`REQ-AS-1`). So a
partition MAY check them against a copy of the book's directory that lags behind it. A lagging copy can only
refuse a transfer to an account just opened (`ERR-UnknownAccount`, which the caller may retry). It can never
accept a transfer whose credit will fail. An asset's deprecation (`ERR-AssetDeprecated`) and an account's
closure take effect in a partition when its copy shows them; a credit already enqueued still applies
(`INV-9`).

**Closure.** An account in `Closing` refuses debits to it and new transfers to it, and still accepts credits
in transit (`INV-9`). It reaches `Closed` only when every partition has confirmed that it holds no credit to
it in transit (default for `c-04`). `REQ-LA-4` is unchanged.

## R4 — Ordering of a multi-leg unit across partitions

`AC-B3.1` keeps its wording. `REQ-KER-10` gains one sentence:

> When the debit legs of one composition are in more than one partition (`REQ-RT-9`), it MUST still apply
> completely or not at all.

The plan does it with the reservation model: each debit
leg is reserved in its own partition, and the unit captures every leg only when every leg is reserved, or
releases every leg that was. The outcome is atomic; the latency is that of two rounds (default for `c-05`).

## R5 — Hot accounts (new)

> **`REQ-RT-12`** **Pools for hot accounts.** An account whose debits arrive faster than one partition can
> order them (an issuer's escrow, a fee collector, a treasury) MAY be held as a **pool** of member positions
> in several partitions.
>
> - The pool is one ledger account. Its owner, its authority holders, its statement and its balance are the
>   account's. Its balance is the sum of its members', read from one consistent view (`REQ-BA-3`).
> - Each debit draws on one member, in that member's partition, and is ordered against that member only.
> - A debit that the pool's balance covers MUST NOT be refused because no single member covers it. Such a
>   debit is served atomically across members, as a multi-leg unit (R4).
> - A credit MAY land on any member.
> - Moving value between members of one pool is the operator's rebalancing. It is an ordinary, posted,
>   audited operation (`REQ-AT-3`). It changes no balance of the account.
> - An account of kind **Issuance** has no limit (`REQ-LA-2`). Its debits need no check, so they commute,
>   and its pool members need no balance of their own.

*Trade-off, stated plainly.* A pool's members can drift apart, so that one runs dry while others hold value.
Rebalancing keeps them level; it is an operation to run, watch and pay for. A large debit that has to
gather from several members costs the two rounds of R4. `DEF-8` (sub-account structures under a ledger
account) is not reopened: a holder never sees a member, and a member is not an account.

## R6 — Anchoring and disclosures over partitions

`INV-19` becomes:

> **`INV-19`** An anchor commits to every entry up to and including a stated sequence **of each partition**,
> and to nothing else.

`REQ-PR-1` is unchanged: an anchor is a bounded-size commitment. Over partitions, it commits to each
partition's state at a stated sequence, all taken at one cut. A disclosure proves a fact against its
partition's state, and that state against the anchor. Value in transit at the cut is in the in-transit
position, which the anchor covers like any other.

## R7 — Balance reads across partitions

`REQ-BA-5` becomes:

> **`REQ-BA-5`** aledgent MUST answer a balance read for an identity, across all its accounts and assets, as one
> query. A caller MUST NOT need to query each account separately. Each account's balances MUST come from one
> consistent view of that account (`REQ-BA-3`). Value moving between two of the identity's own accounts in
> different partitions MUST be shown as in transit, never as missing or as counted twice.

*Trade-off, stated plainly.* Two accounts of one identity, in two partitions, are each read at their own
partition's latest commit. That is not one snapshot of the whole book. An identity moving value between its
own accounts sees it in transit for up to `NFR-19`'s bound (default for `c-07`; its alternative, a
partition per identity, keeps one snapshot per identity and makes a large organisation a hot spot).

## R8 — Revocation takes effect within a bound

The owner ruled (`split-principals`) that identities, credentials and grants are held apart from the book's
partitions and consulted by them, and that a revocation may take effect after a short lag.

`REQ-AU-3` becomes:

> **`REQ-AU-3`** Revocation MUST take effect for every credential derived from the revoked authority,
> including credentials already issued, within the bound of `NFR-20`, measured from the revocation's commit.
> A revocation that waits for an expiry is not a revocation. The bound MUST be declared in the assurance
> level (`REQ-AR-5`).

`US-G3` becomes "I revoke an agent completely, within a stated bound", and `AC-G3.1`:

> - `AC-G3.1` Given a revocation, Then every credential derived from the revoked authority stops working
>   within the bound of `NFR-20` (`REQ-AU-3`).

*Trade-off, stated plainly.* For up to `NFR-20`'s bound after a revocation commits, the revoked authority can
still be used. What it can do in that time is bounded by the grant itself, because every grant has a bound
(`REQ-AU-2`): the exposure is the grant's limits over that window. A holder who revokes an agent mid-spend
should expect the agent's last operations inside the bound to stand.

## R9 — `OQ-17` closed: conversion is not an aledgent operation

**The ruling (2026-10-03).** Conversions happen outside aledgent, by the integrating party or the service
it uses, as a composition of a redemption of one asset and an issuance of another. Atomicity across assets is
never a given.

Applied:

- `OQ-17`: *Closed by amendment 0012: conversion is a redemption and an issuance by the integrating party,
  two operations, with no atomicity between them.*
- `US-F2` (and `AC-F2.1` to `AC-F2.3`), `REQ-AS-5` and `DEF-7` are **withdrawn**. Their identifiers are not
  reused. The `convert` command and the `conversion` operation kind leave §8.1.
- The non-goal "aledgent does not decide who a holder is…" drops its reference to `REQ-AS-5`: aledgent
  records what it was told to move.
- §13 gains:

> - **aledgent does not convert between assets.** A conversion is a redemption of one asset and an issuance
>   of another, each an operation of its own, under its own authority (`REQ-AS-6`). aledgent offers no
>   atomicity between them, and no rate.

*Trade-off, stated plainly.* A conversion that fails between its two halves leaves the integrating party
holding a redemption without the matching issuance. Recovering is the integrating party's operation, and
the book shows exactly where each half stands. This also keeps every posting single-asset per partition,
which partitions need.

## R10 — `INV-25` (revised)

> **`INV-25`** A deployment has exactly one book. It is not split by asset, by account kind or by integrating
> party. Holding its state in partitions of accounts (`REQ-RT-9`) does not split it: one registry, one trial
> balance, one sequence of anchors.

## R11 — Non-functional requirements (new; `NFR-18` measured, the two lags proposed)

> | `NFR-18` | **Throughput, measured.** aledgent's sustained decision rate MUST be measured by the stress suite on every release and reported with the mix of commands and the number of concurrent principals it was measured at. The design MUST scale by adding partitions (`REQ-RT-9`), not by a faster single writer. *No target figure is stated (the owner, 2026-10-03): the aim is the greatest throughput achievable, for a deployment whose principals include millions of concurrently active agents.* |
> | `NFR-19` | **Credit lag.** The credit of a transfer between partitions MUST be applied within **2 s at p99** of its debit's commit (`REQ-RT-11`). *Proposed; waits for the owner (`c-02`).* |
> | `NFR-20` | **Revocation lag.** A revocation MUST take effect within **1 s at p99** of its commit (`REQ-AU-3`). *Proposed; waits for the owner (`c-03`).* |

`NFR-9`'s second axis gains one sentence: "The in-transit position of each asset MUST equal the credits
enqueued and not yet applied."

## R12 — `AC-B1.1` (revised)

> - `AC-B1.1` Given enough available balance, When I transfer, Then my account is debited and the payee's
>   credited by postings that each balance: one posting when both accounts are in one partition, otherwise
>   the debit, final at once, and the credit, applied within `NFR-19`'s bound (`REQ-RT-11`).

---

## What this costs, stated plainly

- **"Instant" now has a bound.** `US-B1`'s instant transfer is instant for the payer and the book. The payee
  of a transfer from another partition waits up to `NFR-19`'s bound.
- **"Immediately" is now "within a bound".** `REQ-AU-3`'s revocation was immediate. It now takes up to
  `NFR-20`'s bound, and the assurance level says so.
- **Conversion leaves the book.** `US-F2` is withdrawn. A conversion is two operations, and their atomicity is
  the integrating party's problem.
- **One more position.** The in-transit position is in every trial balance and every reconciliation.
- **Hot accounts need operating.** Pools need rebalancing, and a few large debits pay two rounds.

## Carried to the plan

`DEC-22` binds this amendment: one Shared Agent per partition group (a **shard**), each holding its book state
in committed maps; credits between shards on an exactly-once, per-pair ordered path; a principals Agent the
shards consult; anchors over every shard's roots. `F-031` builds it in three levers: batching, cost
proportional to touched state, and the split by account.
