<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0032 — A read whose shape aledgent does not accept is `ERR-MalformedCommand`, as a command is

**Status:** **proposed** on 2026-10-16, on the owner's direction ("a misspelled query parameter should be
400; a missing one 422"). Applied to `SPEC.md` **v0.4.26** as proposed, and it waits for the owner's
approval. Until then the change is a proposal carried by its pull request.
**Target:** `SPEC.md` v0.4.25 as on `develop`.
**Date:** 2026-10-16
**Rationale in one line:** a request that is the wrong shape is one condition whichever surface it
arrives on, and a request that is missing something is another. Reads now answer the first with 403 or
404 and, on one route, 422. A misspelt parameter reads as a permissions problem.

---

## The problem

`REQ-ER-3` (amendment 0021) gives every shape fault of a **command** one condition,
`ERR-MalformedCommand` (400). It says that a query's parameters "keep the conditions of the query
surface". That left the shape faults of a read with no condition of their own, and the code filled the gap
differently on each route. On `develop`, with the same mistake:

- A misspelt parameter, `GET /v1/movements?account=…&limit=2&acount=…`, is `ERR-NotAuthorised` (403) on
  `balances`, `movements` and `trial-balance`, and `ERR-QueryUnbounded` (422) on `statements`.
- A value that does not parse, `limit=ten`, is 403 on `movements`. `from=abc` is 422 on `statements`.
- A `direction` outside `debit` and `credit` is 403.
- A malformed identity (`identity=nobody`) is 403. A malformed account is `ERR-UnknownAccount` (404), as
  if it named an account the caller cannot reach. A malformed operation reference is
  `ERR-UnknownOperation` (404).
- A `cursor` that is not hex is `ERR-QueryUnbounded` (422).
- An `amount` on `can_spend` that is not digits is `ERR-PrecisionExceeded` (422).

A client that gets 403 for `acount=…` looks for a missing permission, where the fault is a typo. A client
that gets 422 for the same typo on `statements` cannot tell it from a missing bound. `ERR-NotAuthorised`
is a condition about what the caller may read (`INV-16`). It should not also be the answer to a shape
fault.

The specification already has the condition. §10 defines `ERR-MalformedCommand` as "the request does not
have a shape aledgent accepts: a field unknown, missing or of the wrong type, a value that does not
parse…". `REQ-ER-3` only names commands.

## The change

### §10, `REQ-ER-3`: extended to queries

The paragraph of `REQ-ER-3` that lists what is not a shape fault no longer carries the bullet "A query's
parameters keep the conditions of the query surface". In its place `REQ-ER-3` states the rule for reads:

> **A read's shape is held to the same rule.** A read, on the query surface or the operator surface, whose
> shape aledgent does not accept MUST be refused with `ERR-MalformedCommand`, and nothing is read. That
> covers: a parameter the route does not know, or one that is repeated; a value that does not parse into
> the type of its parameter (`limit=ten`, `from=abc`); an enumerated value outside its set (`direction`);
> an account, identity or operation identifier, in a parameter or in the path, that does not parse; and a
> cursor that is not hex. A shape fault is found before anything is read, as it is for a command.
>
> A parameter that is **missing**, or bounds that **conflict**, are not shape faults. They keep
> `ERR-QueryUnbounded` (`REQ-CS-5`): no subject, or two; no `limit`, or one outside 1 to 1000, however
> large; half a time range, or one reversed; a cursor issued for other filters; a statement without an
> `account`, an `asset`, a `from` or a `to`; a statement period over its bound; an `as_of` the book has
> not reached. A value that parses but names nothing the caller may read keeps its own condition
> (`ERR-UnknownAccount`, `ERR-UnknownAsset`, `ERR-UnknownOperation`, `ERR-NotAuthorised`).

### The mapping, as a table

| What the read sent | Condition | Status |
|---|---|---|
| A parameter the route does not know, or a repeated one | `ERR-MalformedCommand` | 400 |
| A value that does not parse into its type (`limit=ten`, `from=abc`, a negative number, `amount=12x`) | `ERR-MalformedCommand` | 400 |
| A `direction` that is neither `debit` nor `credit` | `ERR-MalformedCommand` | 400 |
| An account, identity or operation identifier that does not parse (parameter or path) | `ERR-MalformedCommand` | 400 |
| A `cursor` that is not hex | `ERR-MalformedCommand` | 400 |
| No subject, or both `account` and `identity` | `ERR-QueryUnbounded` | 422 |
| No `limit`, or a `limit` of 0 or above 1000 | `ERR-QueryUnbounded` | 422 |
| Only one of `from` and `to`, or `from` after `to` | `ERR-QueryUnbounded` | 422 |
| A hex `cursor` issued for other filters or another subject | `ERR-QueryUnbounded` | 422 |
| A statement without `account`, `asset`, `from` or `to` | `ERR-QueryUnbounded` | 422 |
| A statement period over its bound, or an `as_of` not yet reached | `ERR-QueryUnbounded` | 422 |
| A well-formed account the caller does not reach, or an `on_account` outside the subject's | `ERR-UnknownAccount` | 404 |
| A well-formed asset that is not in the registry | `ERR-UnknownAsset` | 404 |

### §10, the two rows

The rows of `ERR-QueryUnbounded` and `ERR-MalformedCommand` do not change. Their wording already covers
both cases.

## What does not change

- Commands. `REQ-ER-3`'s rule for them, its three exceptions and every status stay as they are.
- `REQ-CS-5`. An unbounded query is still refused with `ERR-QueryUnbounded`. What counts as unbounded does
  not change, with one reading made explicit: a page size is outside its bound however large the number,
  so `limit=5000000000` is `ERR-QueryUnbounded`, not a value that fails to parse.
- What the caller may read (`INV-16`). A well-formed identifier that names nothing the caller reaches is
  still `ERR-UnknownAccount`, `ERR-UnknownOperation` or `ERR-NotAuthorised`, as before. A malformed one is
  now told apart from it, which discloses nothing: whether an identifier parses depends on its text alone.
- A shape refusal names no field and quotes nothing the caller sent (`REQ-AU-8`).
- No identifier is withdrawn, renumbered or reused.

## What it costs

- **A change a client can see.** A client that matches on `ERR-NotAuthorised` for a misspelt parameter, or
  on `ERR-UnknownAccount` for a malformed account, now gets `ERR-MalformedCommand`. The statuses move from
  403, 404 and 422 to 400. The HTTP document and the guide to reading the book say so.
- **A wrong-shaped asset symbol is not moved.** An `asset` that is not 2 to 12 letters and digits names no
  asset: `ERR-UnknownAsset` (404), as `AssetId::parse` defines it. The owner's direction did not name it. It can move with this rule if the owner wants it to.

## Plan

- `F-009` gets one task, `T-009-28`: every read route takes the mapping above, in one place, before the
  core is asked. The tests pin each row over HTTP for `balances`, `movements` and `statements`, and the
  HTTP document and the guide are updated.
- No other feature changes. The conformance suite works on the ledger's own entry points, not on HTTP,
  and encodes none of these statuses.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `T-009-28` |
| Withdrawn | None |
| Changed | `REQ-ER-3` (extended to reads) |
