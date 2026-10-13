<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0005 — plain-language edition (editorial)

**Status:** applied to `SPEC.md` v0.3.3
**Target:** `SPEC.md` v0.3.2
**Date:** 2026-09-22
**Rationale in one line:** the owner asked for the documentation to be simpler to read before sign-off;
many readers use English as a second language.

## What changed

- Every section rewritten in plain English: shorter sentences (average about 22 words before, about 15
  after), everyday words, active voice, lists instead of long comma chains.
- Nothing normative changed. Verified mechanically against v0.3.2: the same 464 defined identifiers, no
  dangling references, no duplicates, and identical counts of MUST (244), MUST NOT (93), MAY (22) and
  SHOULD / SHOULD NOT (2). Code blocks, diagrams and every table row are unchanged.

## Defects corrected on the way

These were errors in v0.3.2, found during the rewrite. None changes intent.

| Where | Was | Now |
|---|---|---|
| §0 | Pointed at "companion documents listed at the end of this section", removed in 0001 | Says those topics are outside this specification |
| §3 | "Four kinds of party" over a table of five (the recovery authority was added in 0003) | "Five kinds of party" |
| §4.2 row, `REQ-LA-2`, `REQ-LA-4`, `close_account` | Still said "layer", which 0002 retired from normative text | "balance kind" |
| `US-J3` | — | Reworded for clarity; same story |

The plan and the overview were rewritten to the same style. In the plan, `DEC-11` to `DEC-14` gained the
*Alternatives* part they were missing, `DEC-13` gained *Reversibility*, and the count of deployable units
now says all four are ours.
