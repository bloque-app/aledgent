<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0003 — the recovery model for owned custody

**Status:** applied to `SPEC.md` v0.3.1
**Target:** `SPEC.md` v0.3.0
**Date:** 2026-09-21
**Rationale in one line:** amendment 0002 put owned custody and the custody switch into V0, which made
`OQ-14` (recovery) V0-blocking; the owner has ruled on it.

Owner rulings: the recovery authority is a **different principal from the operator**; the model is a
**dedicated recovery authority** combined with a **time-locked successor**, with a **holder-held recovery
credential** as an optional first line; **guardian thresholds** are not offered (impractical);
**"no recovery, declared"** is not offered.

## A1 — Recovery exists, and the operator cannot do it alone

`REQ-AU-9` revised: recovery MUST exist wherever owned custody is offered, MUST be one of the mechanisms
below, and the holder MUST be shown which apply before choosing owned custody.

New: `REQ-AU-15` (recovery authority distinct from the operator; neither alone can complete),
`REQ-AU-16` (delay, cancellation by the existing credential, visibility, attributed completion),
`REQ-AU-17` (successor), `REQ-AU-18` (holder-held recovery credential, no delay, never sole),
`REQ-AU-19` (guardian thresholds not offered). New party **recovery authority** in §3 and the glossary;
new story `US-A8` with `AC-A8.1`–`AC-A8.6`; commands `initiate_recovery`, `cancel_recovery`,
`complete_recovery`, `name_successor`, `claim_succession`; error `ERR-RecoveryNotElapsed`; `AC-A5.3`
reworded; §7.D operator surface extended.

## What this costs, stated plainly

A holder who loses their credential waits out a delay, and depends on a party that must verify them out
of band. That party's identity and its verification process are deployment facts the specification does
not fix; in a federation each deployment names its own. A recovery authority is also a new thing that
can be compromised, which is why it can neither move value nor act outside a recovery.

## Closed

`OQ-14`. `OQ-13` (the unit of owned custody) remains open and V0-blocking.

## Carried to the plan

Who the recovery authority is for the first production deployment, how it verifies a holder, the length of the
delays, and where its keys live.
