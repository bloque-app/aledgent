<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0030 — Withdrawn and closed identifiers keep a definition (editorial)

**Status:** applied to `SPEC.md` **v0.4.24** (editorial: nothing normative changes). Done on the owner's go
of 2026-10-13 to fix the specification's cross-references, so the documentation site can check them
strictly.
**Target:** `SPEC.md` v0.4.23 as on `main`.
**Date:** 2026-10-13
**Rationale in one line:** a reference to an identifier should always lead somewhere. Five identifiers that
`SPEC.md` and `PLAN.md` still cite were withdrawn or closed, and the specification no longer said so where
the identifier stood.

---

## The problem

§0 says a withdrawn identifier is never reused and that a gap in a sequence means a withdrawal recorded in
an amendment. Where an identifier was withdrawn after its section was written, `SPEC.md` keeps a note in
its place. Those notes mentioned the identifier but did not stand where its definition would, so a reader
(or a tool) following a reference to it found nothing:

- `REQ-AS-5` and `US-F2`, withdrawn by amendment 0012, had a note in italics where each used to be.
- `REQ-MG-1` to `REQ-MG-6`, withdrawn by amendment 0001, had one sentence after `REQ-MG-8` that did not
  name the amendment.
- `OQ-61` and `OQ-62` were raised and closed by amendments 0007 and 0006 on 2026-09-29, and their rows were
  removed from §14. Every question closed since then keeps its row with a *Closed by* note (`OQ-6`, `OQ-17`,
  `OQ-63` to `OQ-67`), and §14 says that an identifier missing from its table was withdrawn before the
  first freeze, which is not true of these two. `PLAN.md` still cites `OQ-61`.

## The change

| Where | Was | Now |
|---|---|---|
| `REQ-AS-5` (§5.3) | *`REQ-AS-5` was withdrawn by amendment 0012 …* | **`REQ-AS-5`** *Withdrawn by amendment 0012 …* The same text, written as the identifier's entry, and it says the identifier is not reused |
| `US-F2` (§6, Epic F) | *`US-F2` was withdrawn by amendment 0012 …* | **`US-F2`** — *Withdrawn by amendment 0012 …* The same, as above |
| After `REQ-MG-8` (§5.12) | "`REQ-MG-1` to `REQ-MG-6` were withdrawn and are not reused." | The same sentence, followed by a table with one row per identifier: withdrawn by amendment 0001, and what survives of it (`REQ-RC-4` from `REQ-MG-1` and `REQ-MG-3`, `REQ-RC-3` from `REQ-MG-5`, as amendment 0001 A5 records) |
| §14 | No row for `OQ-61` or `OQ-62` | A *Closed by* row for each, with the ruling as amendments 0007 and 0006 record it |

## What does not change

- No requirement, story, acceptance criterion, invariant, error or open question changes in substance. No
  identifier is added, renumbered or reused.
- `AC-F2.1` to `AC-F2.3`, withdrawn with `US-F2`, are cited nowhere and keep no entry.

## Plan

`PLAN.md` changes in form only, in the same pass:

- The *Build order inside each milestone* table names `M-0` to `M-3` as plain references. The milestones
  are defined once, by the table above it.
- The task ids `PLAN.md` cites from `features/F-009-ingress-process/tasks.md`,
  `features/F-032-stress/tasks.md` and `features/F-033-aledgent-runtime/tasks.md` link to those files,
  where they are defined.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | None |
| Withdrawn | None |
| Given an entry where they stood | `REQ-AS-5`, `US-F2` (withdrawn by 0012); `REQ-MG-1` to `REQ-MG-6` (withdrawn by 0001) |
| Given back their §14 row | `OQ-61` (closed by 0007), `OQ-62` (closed by 0006) |
