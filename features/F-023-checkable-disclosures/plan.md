<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-023 — Disclosures that can be checked independently

| | |
|---|---|
| Milestone | Later (after V0) |
| Track | D |
| Status | Not planned in detail. This page records intent and blockers only |

## Goal
In V0 every disclosure is aledgent's signed assertion and says so (`DEC-12`). This feature replaces the signature with a construction a verifier can check without trusting aledgent, for example a proof over the kernel's state transition bound to an anchor.

## Spec traces
`DEF-3`, `REQ-PR-6`, `REQ-DS-3`

## Why it is later
Needs a chosen construction and an external review. VOS can prove execution today, but the cost per proof and the verifier's tooling are open (`OQ-23`).

## Before this is planned
Write a full `plan.md` and `tasks.md` in the same format as the V0 features, once its blocker is resolved.
