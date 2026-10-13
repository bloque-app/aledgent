<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0021 — `ERR-MalformedCommand` for every shape fault

**Status:** ruled by the owner on 2026-10-05, on the decision form: `a0020-q1` ("add
`ERR-MalformedCommand` for all shape refusals, not only pass-through"). The same day, the owner answered
`a0020-q2` (keep the default: credits notify the account's opening principal) and `a0020-q3` (keep 10 legs
until measured on the production core). Neither of those changes the spec; §"Carried to the plan" records
them. Applied to `SPEC.md` v0.4.14.
**Target:** `SPEC.md` v0.4.12 as on `main` (amendment 0020 applied). Version v0.4.13 is amendment 0019's
(#185), so this one takes the spec to **v0.4.14**, whichever of the two lands first.
**Date:** 2026-10-05
**Rationale in one line:** a request aledgent cannot read is the caller's mistake, not a lack of authority,
so it gets a condition of its own instead of failing closed as `ERR-NotAuthorised`.

---

## The problem

SPEC §10 has no condition for a malformed request. Ingress has refused every shape fault with
`ERR-NotAuthorised` (`F-009` `NOTES.md`, decision 4), the condition for a principal without authority.
Amendment 0020 wrote the same into `REQ-PO-5` for over-size pass-through data. A caller then cannot tell
"you may not do this" from "this is not a request I can read". An integrating party maps the first to a
configuration fault and pages, and maps the second to a bug in its own code. Both arrive as 403.

## The ruling

Add `ERR-MalformedCommand`, and use it for every shape refusal: a pass-through value over 1 KiB or not JSON,
unknown fields, wrong types, and an oversized body where the size is a shape issue.

## §10 — one row added

> | `ERR-MalformedCommand` | The request does not have a shape aledgent accepts: a field unknown, missing or of the wrong type, a value that does not parse, or a body over its bound (`REQ-ER-3`) |

The list stays closed (`REQ-ER-1`), now with 39 conditions.

## `REQ-ER-3` (new)

> **`REQ-ER-3`** A command whose shape aledgent does not accept MUST be refused with `ERR-MalformedCommand`,
> and nothing is taken in. That covers every shape fault of a command, on the command surface, the operator
> surface and the settlement adapter's submissions: a field unknown or missing, a field of the wrong type, a
> body that is not the JSON the command takes, an identifier, encoding or enumerated value that does not
> parse, a list that must not be empty and is, a header that does not parse, a client key outside its
> bounds, pass-through data over its bound (`REQ-PO-5`), and a body over the bound the deployment takes for
> that command. Three things are not shape faults, and keep their own conditions:
>
> - **A value that parses but is not acceptable.** Excess precision is `ERR-PrecisionExceeded`, a reference
>   to an account or operation aledgent does not hold is `ERR-UnknownAccount` or `ERR-UnknownOperation`,
>   and a multi-leg submission over the published maximum is `ERR-BatchTooLarge` (`REQ-CS-10`).
> - **A proof of possession that does not verify, or does not decode,** is `ERR-CredentialInvalid`. So is
>   anything sent to the routes that authenticate by what their body carries: they deny by default
>   (`NFR-7`).
> - **A query's parameters** keep the conditions of the query surface (`ERR-QueryUnbounded` and the
>   conditions about what the caller may read).
>
> A shape refusal names no field and quotes nothing the caller sent (`REQ-AU-8`).

## `REQ-PO-5` and `AC-M8.3` (revised)

In `REQ-PO-5`, "It fails closed, as any command whose shape is not one aledgent accepts, with
`ERR-NotAuthorised`" becomes "… with `ERR-MalformedCommand` (`REQ-ER-3`)".

`AC-M8.3`: "Then the command is refused with `ERR-MalformedCommand` and nothing is taken in (`REQ-PO-5`,
`REQ-ER-3`)".

## Behaviour change for existing clients

Every shape fault that ingress answered `403 ERR-NotAuthorised` is now **`400 ERR-MalformedCommand`**. A
caller that treated a 403 on a malformed body as "not authorised" will now see a 400 with its own code.

- An integrating client's draft aledgent client (`errors.ts`) lists the closed set of 38 codes and maps
  statuses. It must add `ERR-MalformedCommand` (400): a refusal, final, not retryable, an integration bug.
  Until it does, it would classify the new code as a protocol failure ("outcome unknown").
- Value refusals keep their codes (`ERR-PrecisionExceeded`, `ERR-UnknownAccount`, `ERR-UnknownOperation`,
  `ERR-BatchTooLarge`). The answering setting's wait outside its bounds stays `ERR-NotAuthorised`
  (`AC-M6.4`): that is a value, not a shape.

## No invariant is affected

The refusal changes which condition is named, not what is refused or what is taken in. Nothing malformed
was logged before, and nothing malformed is logged now.

## Carried to the plan

- **`F-009`, built in this PR (`T-009-24`).** Every shape refusal in ingress returns
  `ERR-MalformedCommand` with status 400. That covers JSON bodies, path parameters, headers, the client key,
  identifiers, hex values, enumerations, empty lists that must not be, and a non-transfer body over 16 KiB.
  The conformance case `ERR-MalformedCommand` runs through ingress.
- **`a0020-q2`.** Credits notify the account's opening principal (`REQ-EV-9`, unchanged). Who opens
  Bloque's customer accounts is the integrating party's plan: mediums does, so mediums is their serving
  principal.
- **`a0020-q3`.** Production keeps `max_legs` at 10 until it is measured on the production core
  (`T-008-09`).
