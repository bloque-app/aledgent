<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-020 — Owned custody with holder-held kernel keys

| | |
|---|---|
| Milestone | Later (after V0) |
| Track | D |
| Status | Not planned in detail. This page records intent and blockers only |

## Goal
In V0 the core holds every kernel account key and enforces owned custody in the service (`DEC-3`). This feature lets a holder in owned custody sign kernel transfers with their own key, so the kernel's own record, not only the service's, shows that the holder authorised the movement. It would move `INV-20` and parts of `INV-16` from service-enforced towards kernel-enforced in the assurance level.

## Spec traces
`REQ-AU-6`, `REQ-KER-9`, `DEC-3`, `REQ-AR-6`

## Why it is later
Credentials are a per-identity set (`REQ-AU-20`), so the kernel's one-key-per-account model has to be reconciled with several holder credentials. Needs a decision on how holders hold Ristretto keys, and a review of recovery (`REQ-AU-15`) against keys the operator never sees.

## Before this is planned
Write a full `plan.md` and `tasks.md` in the same format as the V0 features, once its blocker is resolved.
