<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: a reconciliation failure

`F-018` T-018-08 · `NFR-9`, `INV-5`, `INV-6`, `REQ-PV-4`, `REQ-AT-3`

**Pages that lead here:** `AledgentReconciliationDifference`, `AledgentReconciliationNotRun`.
**Owner:** finance-operations, with platform. A failed reconciliation pages a human and never opens a
ticket (`NFR-9`). **Rule that does not bend:** the reconciliation only reads, and so does this runbook.
No storage repair, no support override (`REQ-AT-3`, guardrail 7): a correction is an operation through
`CTR-OPS` — an issue, a redemption, a reversal under two-person approval — with its justification, on the
audit trail.

## Read the record

Each run is a JSON record (`ops::reconcile::Run`) in the reconciliation's records directory. Each finding
names its axis and where, never a balance:

| Finding | Axis | Means |
|---|---|---|
| `commitment` | 1(a) | an account leaf's commitment on a layer is not the sum of its entries' |
| `rebuild` | 1(b) | the leaf store rebuilt from the delta log differs from the stored one |
| `external` | 1(c) | the external source (shadow and cutover) disagrees on a pair, or one side lacks it |
| `unbalanced` | 2 | a currency's commitments do not sum to the identity, over the leaves or the entries |
| `trial-balance` | 2 | an asset's trial balance on openings is not zero |
| `settlement` | 3 | the book's position and the settlement layer's differ at a cut, by the difference named: an aggregate per asset (`F-017`) |
| `stale-cut` | 3 | the adapter's latest positions report is more than a day old: see `anchoring.md` |

`AledgentReconciliationNotRun` on axis 3 means the daily comparison could not run: no positions report
from the adapter yet (`ERR-EvidenceInsufficient`), or ingress refused or did not answer the job. Each run is
also on the audit trail: the `compare_settlement` operator record whose justification is the run's summary.

The run also carries the `Suspense` report (`REQ-LA-2`): each `Suspense` (account, asset) not at zero, and
since when. `AledgentSuspenseNotReconciled` pages once one has not been zero for a day: its value is placed
where it belongs by an operation through `CTR-OPS`, never by an edit.

## Procedure

1. **Freeze the evidence.** Take a core backup and an audit backup now (`restore.md`, "Collect the
   backups"). Keep the run's record.
2. **Axis 1(a) or 1(b) or 2 on the stored leaves.** The store and the log disagree, or the book does not
   balance: this is storage corruption or a kernel defect, never a customer's error. Restore on an
   isolated host from the last backup before the first failing run (`restore.md`) and run the drill's
   checks there. If the restored book is clean, the live store diverged after the backup: page platform to
   replace the core from the restore; the book is re-derived from its entries (`INV-6`). If the restored
   book fails too, stop writes (`resume` nothing, freeze nothing by hand) and escalate to the owner: this
   is an `INV-5`/`INV-6` breach.
3. **Axis 1(c).** The book and the legacy book disagree on named pairs. The comparison ran inside the
   core as a privileged read, recorded with its justification. Decide per pair from each source's
   records; a correction to the book is an operation through `CTR-OPS`, never an edit. During cutover, a
   difference blocks the cutover (`DEC-14`).
4. **Run again** once resolved: `aledgent_reconciliation_differences` returns to zero on every axis.

## Exercised

In CI: `ops/tests/reconcile.rs` — one minor unit of drift in a copied leaf fails axis 1, an unbalanced
copy fails axis 2 and only axis 2, one minor unit sent to the position outside the pallet's calls fails
axis 3, the external source's mismatch at one minor unit is found through a recorded privileged read, a
`Suspense` balance is reported with its age, and each pages. The staging run is deferred.
