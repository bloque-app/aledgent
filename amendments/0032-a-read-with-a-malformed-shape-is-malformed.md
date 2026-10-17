<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0032 — A read whose shape aledgent does not accept is `ERR-MalformedCommand`

**Status:** **ruled by the owner on 2026-10-17.** Proposed on 2026-10-16, on the owner's direction ("a
misspelled query parameter should be 400; a missing one 422"), and applied to `SPEC.md` **v0.4.26** as
proposed. The owner approved it on 2026-10-17; `SPEC.md` **v0.4.27** marks it ruled (with amendment 0033), and
the text is unchanged.
**Target:** `SPEC.md` v0.4.25 as on `develop`.
**Date:** 2026-10-16
**Rationale in one line:** on the reads, a request that is the wrong shape is one condition on every route,
and a request that is missing something is another. Today the reads answer a shape fault with 403, 404 or
422 depending on the route, and a misspelt parameter reads as a permissions problem.

---

## The problem

`REQ-ER-3` (amendment 0021) gives every shape fault of a **command** one condition,
`ERR-MalformedCommand` (400). It says that a query's parameters "keep the conditions of the query
surface". That left the shape faults of a **read** with no condition of their own, and the code filled the
gap differently on each route. On `develop`, with the same mistake:

- A misspelt parameter, `GET /v1/movements?account=…&limit=2&acount=…`, is `ERR-NotAuthorised` (403) on
  `balances`, `movements` and `trial-balance`, and `ERR-QueryUnbounded` (422) on `statements`.
- A value that does not parse, `limit=ten`, is 403 on `movements`. `from=abc` is 422 on `statements`.
- A `direction` outside `debit` and `credit` is 403.
- A malformed identity (`identity=nobody`) is 403. A malformed account is `ERR-UnknownAccount` (404), as
  if it named an account the caller cannot reach. A malformed operation reference is
  `ERR-UnknownOperation` (404).
- A `cursor` that is not hex is `ERR-QueryUnbounded` (422).
- An `amount` on `can_spend` that is not digits is `ERR-PrecisionExceeded` (422).
- An `asset` that cannot be a symbol (`asset=US$`) is `ERR-UnknownAsset` (404), the answer for a symbol
  that is well formed and not in the registry.

A client that gets 403 for `acount=…` looks for a missing permission, where the fault is a typo. A client
that gets 422 for the same typo on `statements` cannot tell it from a missing bound. `ERR-NotAuthorised`
is a condition about what the caller may read (`INV-16`). It should not also be the answer to a shape
fault.

The specification already has the condition. §10 defines `ERR-MalformedCommand` as "the request does not
have a shape aledgent accepts: a field unknown, missing or of the wrong type, a value that does not
parse…". `REQ-ER-3` only names commands. The word "missing" in that definition is also the one place it
conflicts with the rule this amendment states for reads, where a missing parameter is a bound
(`ERR-QueryUnbounded`).

## The change

### §10, `REQ-ER-3`: extended to reads

The bullet "A query's parameters keep the conditions of the query surface" is removed from the list of what
is not a shape fault. The two bullets that remain are renumbered from "Three things" to "Two things". After
the list, and before the closing sentence of `REQ-ER-3` ("A shape refusal names no field…"), `SPEC.md`
v0.4.26 carries this paragraph. It is quoted here as it stands there; since v0.4.27 its last sentence reads
"*Ruled by the owner on 2026-10-17 (amendment 0032).*":

> A read's shape is held to the same rule. A read, on the query surface or the operator surface, whose shape
> aledgent does not accept MUST be refused with `ERR-MalformedCommand`, and nothing is read. A shape fault is
> found before anything is read, as it is for a command. It is: a parameter the route does not know, or one
> that is repeated; a value that does not parse into the type of its parameter, where any value made of digits
> is a count; an enumerated value outside its set; an account, identity or operation identifier, in a
> parameter or in the path, that does not parse; an asset symbol that is not 2 to 12 ASCII letters and digits;
> an alias source that cannot be one; and a cursor that is not hex. A parameter that is **missing**, or bounds
> that **conflict**, are not shape faults. They keep `ERR-QueryUnbounded` (`REQ-CS-5`): no subject, or two; no
> page size, or one outside 1 to 1000, however large; half a time range, or one reversed; a cursor issued for
> other filters; a statement without its account, asset, `from` or `to`; a statement period over its bound; an
> `as_of` the book has not reached. A value that parses but names nothing the caller may read keeps the
> condition for that (`ERR-UnknownAccount`, `ERR-UnknownAsset`, `ERR-UnknownOperation`, `ERR-NotAuthorised`).
> One read keeps a condition of its own: a disclosure to verify that is missing, or is not hex, is
> `ERR-DisclosureUnsupported`, because aledgent cannot make the assertion. This paragraph is about reads. It
> changes nothing for commands. *Proposed; waits for the owner (amendment 0032).*

### §10, the `ERR-MalformedCommand` row

"missing" is qualified, so that §10 does not contradict the paragraph above. The row of `ERR-QueryUnbounded`
does not change.

| | Text |
|---|---|
| Before | The request does not have a shape aledgent accepts: a field unknown, missing or of the wrong type, a value that does not parse, or a body over its bound (`REQ-ER-3`) |
| After | The request does not have a shape aledgent accepts: a field unknown or of the wrong type, a field a command requires and lacks, a value that does not parse, or a body over its bound (`REQ-ER-3`) |

The row is the description a refusal carries, so it changes in the HTTP document and in the catalogue as
well.

### The mapping, as a table

| What the read sent | Condition | Status |
|---|---|---|
| A parameter the route does not know, or a repeated one | `ERR-MalformedCommand` | 400 |
| A value that is not made of digits where a count is expected (`limit=ten`, `limit=-1`, `limit=`), or does not parse into its type (`from=abc`, `amount=12x`) | `ERR-MalformedCommand` | 400 |
| A `direction` that is neither `debit` nor `credit` | `ERR-MalformedCommand` | 400 |
| An account, identity or operation identifier that does not parse (parameter or path) | `ERR-MalformedCommand` | 400 |
| An `asset` that is not 2 to 12 ASCII letters and digits | `ERR-MalformedCommand` | 400 |
| An alias `source` that cannot be one | `ERR-MalformedCommand` | 400 |
| A `cursor` that is not hex | `ERR-MalformedCommand` | 400 |
| No subject, or both `account` and `identity` | `ERR-QueryUnbounded` | 422 |
| No `limit`, or a `limit` of 0 or above 1000, however many digits | `ERR-QueryUnbounded` | 422 |
| Only one of `from` and `to`, or `from` after `to` | `ERR-QueryUnbounded` | 422 |
| A hex `cursor` issued for other filters or another subject | `ERR-QueryUnbounded` | 422 |
| A statement without `account`, `asset`, `from` or `to` | `ERR-QueryUnbounded` | 422 |
| A statement period over its bound, or an `as_of` not yet reached | `ERR-QueryUnbounded` | 422 |
| A well-formed account the caller does not reach, or an `on_account` outside the subject's | `ERR-UnknownAccount` | 404 |
| A well-formed asset symbol that is not in the registry | `ERR-UnknownAsset` | 404 |
| `disclosures/verify` with a missing or non-hex `disclosure` | `ERR-DisclosureUnsupported` | 422 |

## What does not change

- **Commands.** This amendment covers reads only. `REQ-ER-3`'s rule for commands, its exceptions and
  every status stay as they are. It does **not** make one condition for the same fault on every surface:
  the command validators still answer the condition of the value for some values that do not parse. A
  malformed account in a command body is `ERR-UnknownAccount`, an asset `ERR-UnknownAsset`, an operation
  `ERR-UnknownOperation`, and an `amount` such as `12x` is `ERR-PrecisionExceeded`. `REQ-ER-3` already
  calls those shape faults, so the commands are the part that is out of line with it. Aligning them is a
  separate, client-visible change, and needs its own follow-up.
- **`disclosures/verify`, a deliberate exception.** A missing `disclosure`, and one that is not hex, keep
  `ERR-DisclosureUnsupported` (§10: "aledgent cannot make the assertion requested"). It is a condition
  about the assertion, not about the request's shape. An unknown or repeated parameter on that route is
  still `ERR-MalformedCommand`.
- **`REQ-CS-5`.** An unbounded query is still refused with `ERR-QueryUnbounded`. What counts as unbounded
  does not change, with one reading made explicit: any value made of digits is a count, and a count
  outside the page bound is unbounded however many digits it has, so `limit=99999999999999999999` is
  `ERR-QueryUnbounded`, not a value that fails to parse.
- **What the caller may read (`INV-16`).** A well-formed identifier that names nothing the caller reaches
  is still `ERR-UnknownAccount`, `ERR-UnknownAsset`, `ERR-UnknownOperation` or `ERR-NotAuthorised`, as
  before. A malformed one is now told apart from it, which discloses nothing: whether an identifier or a
  symbol parses depends on its text alone.
- A shape refusal names no field and quotes nothing the caller sent (`REQ-AU-8`).
- No identifier is withdrawn, renumbered or reused.

## What it costs

- **A change a client can see.** A client that matches on `ERR-NotAuthorised` for a misspelt parameter, on
  `ERR-UnknownAccount` or `ERR-UnknownAsset` for a malformed identifier or symbol in a read, or on
  `ERR-QueryUnbounded` for a cursor that is not hex, now gets `ERR-MalformedCommand`. The statuses move
  from 403, 404 and 422 to 400. The HTTP document and the guide to reading the book say so.
- **Reads and commands differ for now.** Until the commands are aligned, the same malformed account is 400
  on a read and 404 on a command. The text above says so rather than hiding it.

## Plan

- `F-009` gets one task, `T-009-28`: every read route takes the mapping above, in one place, before the
  core is asked. The tests pin each row over HTTP, and the HTTP document and the guide are updated.
- A follow-up, not in this amendment: align the commands' validators with `REQ-ER-3` (`command.rs`).
- The conformance suite works on the ledger's own entry points, not on HTTP, and encodes none of these
  statuses. No conformance case pins the read clause of `REQ-ER-3` yet.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `T-009-28` |
| Withdrawn | None |
| Changed | `REQ-ER-3` (extended to reads), the `ERR-MalformedCommand` row of §10 ("missing" qualified) |
