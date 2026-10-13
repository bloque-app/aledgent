<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: divergence

`NFR-9`, `INV-5`, `INV-6`, `INV-18`, `INV-19`, `REQ-RT-11`, `REQ-AT-3`, guardrail 7 · **Pages:**
`AledgentReconciliationDifference`, `AledgentReconciliationNotRun`, `AledgentSettlementDifference`,
`AledgentSuspenseNotReconciled`, `AledgentAnchorCommitmentMismatch`, `AledgentUnknownAnchor`,
`AledgentRescanDifference` · **Owner:** finance-operations with platform; security for anchors.

**A divergence is an incident, never a ticket** (`NFR-9`). Tolerance is zero, at a resolution of one minor
unit.

**The rules that do not bend:**

- Nothing here edits a store.
- A correction is an **operation** through `CTR-OPS`, with its justification on the audit trail: an
  issue, a redemption, a resolution with evidence, or a reversal under two-person approval
  (`REQ-AT-3`, `REQ-RC-2`).

## Kinds of divergence

| Signal | What diverged | Where to go |
|---|---|---|
| The core host stops with a replica difference (`HostError::Divergence`, or a summary or root mismatch in `serve`) | The Agent's committed result differs from the native replica's, for the same input | Steps below, with platform first. The host refuses to go on by design |
| Axis 1(a), 1(b) or 2 (`commitment`, `rebuild`, `unbalanced`, `trial-balance`) | The book's own consistency: storage corruption or a kernel defect | [reconciliation-failure](../reconciliation-failure.md), step 2 |
| Axis 2 with credits in transit (sharded) | A pair's in-transit positions do not match the credits enqueued and not applied, at a consistent cut (`REQ-RT-14`) | Steps below. Suspect a forged or lost stream message (`R-03`) |
| Axis 3 (`settlement`) | The book's position against Kreivo's contract holding, per asset | [reconciliation-failure](../reconciliation-failure.md); [anchoring](../anchoring.md) (daily comparison) |
| Re-scan difference | The inbox, the adapter's store and what the core ingested disagree | [gap-filling](../gap-filling.md) |
| Anchor commitment mismatch, or an unknown anchor | Someone else wrote under the instance's keys, or the anchors came from another book | [anchoring](../anchoring.md). Page security |
| A replay reply differs from the received-log's recorded reply | Restore or failover produced a different decision for an answered command (`INV-18`) | Stop serving. Steps below |

## Preconditions

- [ ] Page the owners on the alert's `owner` label. Open an incident under the operator's incident process.
- [ ] **Freeze the evidence before anything else:** a core backup and an audit backup now; a snapshot of
  the core volume; the reconciliation run's JSON record; the host's last reports; the image digests.

## Steps

1. **Classify** with the table above.
2. **The book versus itself** (axes 1 and 2, or a replica difference):
   - Restore the last backup from before the first failing run onto an isolated host, and run the drill's
     checks ([restore](../restore.md)).
   - If the restored book is clean, the live store diverged after that backup. Replace the core from the
     restore ([total-loss-recovery](total-loss-recovery.md)); the book is re-derived from its entries
     (`INV-6`).
   - If the restored book fails too, **stop writes**: stop the forwarder and keep ingress acknowledging.
     Escalate to the owner. This is an `INV-5` or `INV-6` breach.
3. **Between shards** (sharded profile): read every shard's heads at the last consistent cut. Find the
   pair with `applied > enqueued` (a credit counted twice) or a credit missing from the source's outbox.
   A message the source never committed means the relay is not trustworthy. **Stop the relay** and page
   security (`R-03`).
4. **Against Kreivo** (axis 3): decide per asset from Kreivo's finalised records. A difference the book
   cannot explain is corrected only by an operation, with the chain's record as evidence.
5. **Fix the cause** (a defect, a lost message, a key compromise) through the normal release path before
   resuming.

## Verification

- [ ] Every axis is back to zero, on a run started after the fix (`aledgent_reconciliation_differences` is
  0).
- [ ] Every correcting operation is on the audit trail with its justification and its approvers.
- [ ] The next anchor goes final and is consistent.
- [ ] The incident record names the cause, the operations that corrected it, and the time from the page to
  zero.

## Rollback

Corrections are operations: a wrong correction is itself corrected by a reversal (two approvers), never
by deleting it. A restore that replaced the core is not rolled back. The old store is kept as evidence.

## Exercised

On every change: `ops/tests/reconcile.rs` (the reconciliation's axes) and `ops/tests/alerts.rs` (each
signal above pages the test route). The end-to-end response is not
exercised automatically yet; it is exercised by hand in a staging deployment.
