<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-011 — Queries and statements

| | |
|---|---|
| Milestone | M-1 — The book, in a process |
| Track | A |
| Home | `crates/ledger-core` |
| Depends on | `F-001`, `F-003`, `F-004`, `F-007`, `F-008` |
| Unblocks | `F-006` (the index contracts it must serve at scale), `F-009` (query serving), `F-017`, `F-018` |
| Realises | `DEC-1`, `DEC-2` (`reserved` as a derived sum), `DEC-4` (index contracts for the side store), `DEC-11` (the core opens amounts for authorised readers) |

## Goal

A holder reads what they hold across every account and asset in one answer. `available` and `total` come
from one consistent view, and `available` is the same number a spend is checked against. Movements page
on the entry sequence and stay stable under concurrent writes. A statement balances exactly, and the
same statement asked twice is identical. No query changes anything, and no principal can read what it
has no authority over.

## Spec traces

- `REQ-BA-5`, `AC-H1.1` — identity-wide balances in one result.
- `REQ-BA-2`, `REQ-BA-3`, `REQ-BA-6`, `AC-H1.2` — `available` and `total` labelled, one view, `reserved`
  derived.
- `REQ-PO-2`, `REQ-QR-5`, `AC-H2.1` — cursor on the sequence, no offset, stable under writes.
- `REQ-QR-1`, `AC-H2.2` — filters by account, asset, direction, counterparty, time range.
- `REQ-PO-1`, `AC-H2.3` — status from the posting kind, never from a string.
- `INV-6`, `AC-H3.1` — opening plus movements equals closing.
- `REQ-QR-2`, `AC-H3.2` — one consistent view; the same statement twice is identical.
- `REQ-QR-3`, `AC-H4.1` — no side effects.
- `REQ-QR-4`, `INV-20`, `AC-H5.1` — every entry visible, with the authority that caused it.
- `INV-16`, `REQ-PV-1` — no fact without authority.
- `REQ-QR-6` — no settlement-layer concept needed to read one's own facts.
- Supporting: `INV-5` (`trial_balance`), `REQ-CS-5`, `ERR-QueryUnbounded`, `ERR-UnknownAccount`,
  `ERR-UnknownOperation`, `REQ-OP-7` (`operation(reference)`), `REQ-AS-1` (`assets()`), `NFR-3`.

## Scope

**In:** `balances`, `movements`, `statement`, `operation`, `assets`, `trial_balance` and `can_spend`
from §8.2; the read view; the authorised read scope; the cursor; query bounds; the index contracts on the
storage trait, with in-memory implementations.

**Out:** `verify` and `assurance` (`F-017`, `F-015`); the privileged read mechanism of `REQ-PV-4` and the
access audit (`F-013`); indexes at 150M entries and the `NFR-3` latency proof (`F-006`); the HTTP surface
(`F-009`); authority evaluation itself (`F-003`, called here).

## Design

**1. One view per query (`REQ-QR-2`).** Every query takes a `ReadView`. A view is fixed at one committed
posting sequence `S`, the value `F-007`'s commit assigns (`REQ-PO-2`). Everything a query returns is as
of `S`: kernel account leaves, holding positions, operation records, indexes. A query borrows the view
immutably (`&ReadView`). The read API has no `&mut` method. So a query cannot write, by construction
(`REQ-QR-3`). In M-1 the in-memory store gives a view by cloning its roots under a read lock. `F-006`
gives the same contract over the side store.

**2. Authorised scope (`INV-16`, `REQ-PV-1`).** A query first asks `F-003` for an `AuthorisedScope`: the
accounts, and the reach within each, that the calling principal holds authority over. Every read method
on the storage trait takes the scope as an argument and filters on it inside the store. It is not
filtered afterwards in the caller. So the store enforces `INV-16`, the layer §9 names. An account outside
the scope gives `ERR-UnknownAccount`, the same answer as a missing account. Delegated reach is only as
wide as the grant (`REQ-PV-2`).

**3. Balances (`REQ-BA-2`, `REQ-BA-3`, `REQ-BA-5`, `REQ-BA-6`).** For each (account, asset):
- `available` is the account's `Layer::Settled` net, opened by the core's oracle. The core holds the
  openings at L0 (`DEC-11`).
- `reserved` is the sum of the account's open holding positions (`DEC-2`), read from the holding index in
  the same view.
- `encumbered` is read the same way. It is zero in V0, because `DEF-11` defers encumbrance.
- `total` is the sum of the three.

The reply labels each field by name, and never shows `total` unlabelled. `can_spend` and the reserve path
of `F-004` call the same function, `available(view, account, asset)`. So the number shown and the number
checked cannot differ. For an identity, `balances` walks the identity → accounts index of `F-003` inside
one view and returns one result.

**4. Movements (`REQ-QR-1`, `REQ-QR-5`, `REQ-PO-1`, `REQ-QR-4`).** A movement is derived from a posting.
It is never stored as a separate record. The storage trait gets an index `account_postings(account,
scope, from_seq, to_seq, limit)`, ordered by sequence. Each movement carries: sequence, posting kind,
direction, amount, asset, counterparty, time, operation reference, and the authority used with its
principal (`REQ-AU-7`, from the operation record). That last field makes an entry made under operator
authority visible as such (`AC-H5.1`). Status is a total `match` from `PostingKind` to a movement status,
with no wildcard and no string input (`AC-H2.3`). Counterparty is the other side's account for an
internal posting, and the external destination or source for a settled external leg. It is shown when
aledgent knows it and never needed as an input (`REQ-QR-6`).

**5. Cursor.** The cursor is opaque bytes: `(last_sequence, filter_digest, view_sequence)`. The next page
starts after `last_sequence` and is still read at `view_sequence`, so pages do not shift when new postings
arrive. A cursor used with a different filter is refused, because its digest will not match. There is no
offset parameter anywhere. Page size has a maximum.

**6. Time filters.** A posting's `at` is the injected time of the commit that made it (`F-007`, `DEC-6`).
The commit sets `at = max(injected, previous at)`, so `at` never decreases along the sequence. A time
range then maps to one contiguous sequence range by binary search on a sparse `(sequence, at)` index.
Paging stays on the sequence even though many postings share one time (`REQ-PO-2`).

**7. Statements (`INV-6`, `AC-H3.1`, `AC-H3.2`).** `statement(subject, period, as_of?)` resolves the
period to `[s0, s1]` at the view. The opening balance is derived from entries up to `s0`. The movements
are the entries in `(s0, s1]`. The closing balance is the opening plus the movements. So the statement
balances by construction. It is then compared with the stored balance at `s1`. A difference is an `INV-6`
incident, raised to `F-018`'s reconciliation channel. The statement still reports the entry-derived
figures, because entries are the source of truth. The reply names `[s0, s1]`. A period that is closed, or
a request that repeats `as_of`, gives the same bytes every time. **Proposal:** per-account balance
checkpoints every K postings, so the opening is a checkpoint plus a short run of entries. *Alternative:*
walk from the account's first entry each time. That is simple and correct, but it does not scale to
skewed accounts (`NFR-3`).

**8. Trial balance (`INV-5`).** Two checks. First, on commitments: per currency and per layer, the sum of
every account's `dr − cr` commitment is the identity point. Pedersen commitments add, and the kernel
enforces zero-sum per `(ledger, layer)` on every transfer. This check needs no
openings, so it can run in the store. Second, on openings: the core's oracle opens the per-asset sums and
reports exact integers. Both must say zero. Holding positions and the issuance counterparty are included.

**9. Other queries.** `operation(reference)` returns state, `StateClass` (`F-007`), deadline, evidence
coordinates, outcome and the named error if refused. `can_spend(account, asset, amount)` runs the same
checks as `F-008`'s validation (steps 2–4) against the view, and returns `Ok` or the named reason. It
never returns a bare boolean. `assets()` lists the `F-003` registry.

**10. Bounds (`REQ-CS-5`).** Every list query needs a page size ≤ the maximum. A statement needs both ends
of its period. A request without them gets `ERR-QueryUnbounded`, before any read.

## Test plan

- `AC-H1.1`: an identity with three accounts in two assets gets one result with six rows.
- `AC-H1.2`: every row has labelled `available` and `total`. A concurrent reserve during the query does not
  split the numbers. Property test with writers running.
- `AC-H2.1`: paging while writers append. The union of pages equals the sequence range at `view_sequence`,
  with no duplicate and no gap.
- `AC-H2.2`: each filter alone and in pairs, checked against a brute-force scan.
- `AC-H2.3`: the status mapping is total. There is no function taking a string in the movement module
  (a lint checks this).
- `AC-H3.1`: statements over random periods balance to the minor unit. `AC-H3.2`: the same request twice
  gives identical bytes.
- `AC-H4.1`: the composite root and the sequence are unchanged after every query in a 10k-query fuzz run.
- `AC-H5.1`: an operator-caused entry on a delegated account is visible to the holder, marked as operator
  authority.
- `INV-16`: a principal with no grant gets `ERR-UnknownAccount` for every query and account. The store is
  called with a narrowed scope in the test, not the service.
- `INV-5`: both trial-balance checks are zero after random workloads from `F-008`, `F-004` and `F-010`.
- All of the above go into the `F-015` suite.

## Exit criterion

`AC-H1.1`, `AC-H1.2`, `AC-H2.1`–`AC-H2.3`, `AC-H3.1`, `AC-H3.2`, `AC-H4.1` and `AC-H5.1` pass in the
conformance suite against the library in a plain process. `INV-5`, `INV-6` and `INV-16` tests pass at the
layer §9 names.

## Open points

- **`OQ-36`.** Time filters and statement periods use the injected time of the commit. Which clock is
  authoritative is open. The plan relies only on `at` never decreasing, which the commit guarantees.
- **Statement for a period that is still open.** The spec requires identical results for the same period.
  For a period that has not ended, this plan makes the result identical for the same `as_of`, not for the
  same wall-clock request. The owner should confirm this reading.
- **Proposal:** balance checkpoints (design point 7). **Proposal:** the cursor format (design point 5).
  Alternative for the cursor: a server-side cursor table. It gives smaller tokens, but it is a side effect
  of a query.
- **Access trace.** `REQ-PV-5` wants every access traceable to a principal and a request. Writing that
  trace is not a ledger fact, so it does not break `REQ-QR-3`. It belongs to the audit trail of `F-013`.
  This feature emits the event and does not store it.
- **Counterparty for external legs.** The destination of an outbound leg is known. The source of an
  inbound deposit may be only what the settlement layer reveals. What aledgent shows is "every
  counterparty aledgent knows" (`REQ-PV-2`), no more.
