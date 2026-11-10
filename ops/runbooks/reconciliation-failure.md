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

## The daily job

`opsctl reconcile --exporter-config <exporter.toml> --from <archive> --config <deployment config>
(--anchors-rpc <URL> | --no-anchors) (--token-file <file> | --token-env <VAR>) [--compare-settlement]` (`T-018-12`) is the daily run against a
running deployment. Axis 1 is `opsctl archive verify` on the off-node archive; axis 2 is `GET
/v1/trial-balance` with the operator's token (`--token-file` or `--token-env`, never an argument; the exporter's
`read_signals` token is refused the trial balance, and the opt-in also needs the `compare_settlement` kind);
axis 3 is `F-017`'s comparison. It writes one run record into the exporter configuration's
`[records] reconciliation` and exits non-zero unless every axis ran and found nothing.

- Axis 3 with **nothing bound** has no baseline: the record says `no baseline (nothing bound)`, the axis
  counts as run with no difference, and nothing is written (owner's ruling).
- Axis 3 with an asset **bound** needs `--compare-settlement`, because the comparison writes an operator
  record on the audit trail and runs only with the owner's go. Without the flag it is recorded as not run
  and pages as not run. No other flag implies it.
- An axis 1 with a `Deferred` check (the anchor checks under `--no-anchors`) or an undecided one is
  recorded as not run, with the checks named. An empty trial balance (no accounts or no assets) is axis 2
  not run. An asset whose binding is not `book_only`, `binding` or `bound` makes axis 3 not run. A bound
  asset with no row in the comparison is held by neither side; the note says so.

What the job does not catch, and what does:

- **Posted, but the record was not written.** If the comparison was posted and writing the run record
  then failed, the audit trail has a `compare_settlement` record with no run record, and the command
  exits non-zero. Re-run; do not treat the audit record as a run.
- **Token or configuration errors** (unreadable token, no `[records] reconciliation`, bad arguments) fail
  the command before any record is written. Nothing on the record says so; the exporter pages
  `AledgentReconciliationNotRun` after 24 hours without a run.
- **Axis 3 not run** (a bound asset without `--compare-settlement`, or a comparison that failed) pages
  only 24 hours after the last run that did run that axis, not at once. The command's exit status shows
  it at once.
- **A stalled archive still verifies clean.** Axis 1 checks what the archive holds, not that it is
  current. Only the archive-lag alert catches an archiver that stopped.
- **Suspense.** This command does not make the report. The exporter keeps publishing the last run that
  did (with its age growing), so `AledgentSuspenseNotReconciled` still fires from it. If no run has ever
  made it, there is no signal and no page.
- The command does not make the `Suspense` report; its records say `suspense_checked: false` and the
  exporter publishes no `Suspense` signal from them.

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
