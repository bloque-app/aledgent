<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-021 — Replicated (Shared) core Agent

| | |
|---|---|
| Milestone | Later (after V0) |
| Track | D |
| Status | Not planned in detail. This page records intent and blockers only |

## Goal
V0 runs the core on one Local Agent with backups (`DEC-10`). This feature moves it to a replicated Shared Agent, so the loss of one node does not need a restore.

## Spec traces
`NFR-5`, `NFR-4`, `DEC-10`, `REQ-RC-3`

## Why it is later
Blocked on `DEP-3`: Shared Agents are not production-qualified on VOS `saga/agents` and run under a host-wide mutex. The locality of an Agent may not be widened later, so this may require a re-genesis through export and re-import (`REQ-RC-3`, `F-014`).

## Before this is planned
Write a full `plan.md` and `tasks.md` in the same format as the V0 features, once its blocker is resolved.
