<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-022 — Cross-deployment settlement

| | |
|---|---|
| Milestone | Later (after V0) |
| Track | D |
| Status | Not planned in detail. This page records intent and blockers only |

## Goal
Deployments on the same settlement layer settle each cross-deployment transfer as its own external leg in V0. This feature adds the netted variant: signed vouchers between deployments and a per-window net settlement, bounded and reconciled to zero at each close. VOS's `clerk-bridge` and `clerk-settle` actors are the prior art.

## Spec traces
`REQ-SL-6`, `DEF-1`

## Why it is later
Needs the exposure bound and window (`REQ-SL-6`), and the answers to `OQ-32` and `OQ-33`. The prior-art bridge hard-codes one currency and keeps a viewing-key secret in replicated state (see `notes/FIT-VOS-CIPHERCLERK.md`).

## Before this is planned
Write a full `plan.md` and `tasks.md` in the same format as the V0 features, once its blocker is resolved.
