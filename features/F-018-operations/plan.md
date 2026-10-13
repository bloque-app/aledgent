<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-018 — Operations

| | |
|---|---|
| Milestone | M-3 — Production V0 |
| Track | B |
| Home | `ops/` |
| Depends on | F-005, F-006, F-009, F-011, F-013, F-014, F-016, F-017 |
| Unblocks | M-3 cutover (DEC-14 phases 1–3) |
| Realises | DEC-10 (backups and drill), DEC-14 (the reconciliation it gates on) |

## Goal
Operators can show three things at any time. First, a backup restores to a book that re-derives every
balance from its entries and has lost no acknowledged operation. Second, the six `NFR-11` signals are
visible, and they page someone when they cross a line. Third, every day the book is reconciled on three
axes at one minor unit with zero tolerance, and any difference pages a human.

## Spec traces
- `NFR-4`, `NFR-10`, `AC-K5.1` — no acknowledged operation lost; backups restorable; restore re-derives
  every balance; a drill before production and at a stated cadence.
- `NFR-11`, `REQ-OP-8`, `AC-C4.2` — six signals, including the age of the oldest non-terminal operation.
- `NFR-9`, `INV-5`, `INV-6`, `REQ-PR-5`, `AC-J3.1` — three axes daily, one minor unit, zero tolerance,
  paging.
- `REQ-OP-6`, `AC-C4.1` — the `Needs-Review` queue on a dashboard with an owner and a service level.
- `REQ-CS-7`, `REQ-EV-6`, `REQ-SA-7`, `REQ-IN-5`, `REQ-SN-2` — the sources of the signals.
- `REQ-LA-2` — `Suspense` reconciled to zero on a cadence.
- `REQ-PV-4`, `REQ-PV-5`, `REQ-AT-1`, `REQ-AT-3` — reads are attributed. Any privileged read goes through
  one counted mechanism. No repair path outside the command surface.

## Scope
**In:** backup jobs for every durable store; the restore procedure and the drill with its checks; the
signal exporter; alert rules and paging; dashboards; the daily reconciliation job; runbooks.
**Out:** rebuilding the leaf store from the delta log (`F-006`); export and re-import (`F-014`); producing
each signal inside its unit (`F-007`, `F-009`, `F-016`, `F-017`); the settlement comparator (`F-017`); the
legacy-book source for shadow and cutover (DEC-14 Phase 1 work). This feature gives that source a place to
plug in.

## Design

**What must survive.**

| Unit | Durable state | Backup | Restored from |
|---|---|---|---|
| Core Agent | roots, hot records, committed delta log, outbox, operation records | `F-014` export on a cadence; VOS space backup when qualified | backup, then replay of the input log |
| Leaf store | leaves, history, indexes | none | rebuilt from the delta log (`DEC-4`, `F-006`) |
| Ingress | received-log, input order, delivery attempts, recipients | PostgreSQL base backup and WAL archive | itself, to the latest point |
| Adapter | instructions, extrinsics, cursors, gaps | PostgreSQL base backup and WAL archive | itself, then an inbox re-scan |
| Audit trail | append-only records | its own retention store (`F-013`, `NFR-6`) | itself |
| Keys | signer, clerk key, core root | the KMS or HSM's own backup | never in these backups |

VOS on `main` backs up only with the daemon stopped. On `saga`, native backup and restore after
bootstrap is still an open release gate. **Proposal:** the primary core backup is
the `F-014` export, on a cadence. A VOS space backup runs in a short stop window that ingress absorbs
(`DEC-5`, `NFR-5`), once `saga` qualifies it.

**Restore** (`ops/opsctl restore`, and a runbook).
1. Restore the core from the latest backup. Note its last input number `s_b`.
2. Restore the ingress store to the latest point.
3. Replay every ingress input with `core_input_seq > s_b`, in order. Every core input passes through
   ingress (`F-009`): commands, ticks, observations and delivery records. So this replay is complete.
4. Rebuild the leaf store from the delta log (`F-006`).
5. Restore the adapter store and re-scan the inbox from its cursor. The chain is the truth for settlement.

**Drill** (`NFR-10`). The same procedure, on an isolated host with no network path to production. One VOS
identity must never run on two live machines. It checks:
- `INV-6`: every stored balance equals the sum of its entries;
- `INV-5`: the trial balance is zero per asset;
- the rebuilt composite root equals the last committed root, and every `Final` anchor's commitment
  recomputes from rebuilt state at its sequence. This check is external: it is against Kreivo;
- `NFR-4`: every operation reference ingress acknowledged exists in the restored core, in the same or a
  later state;
- recovery time and data loss window, recorded with the result.
**Proposal — cadence:** before production (a gate of `M-3`), then monthly, and after every change of the
VOS or CipherClerk pin. *Alternative:* quarterly.

**Signals** (`NFR-11`).

| Signal | Source | Pages when |
|---|---|---|
| Age of the oldest non-terminal operation (`REQ-OP-8`) | core query; oldest `Received` row in ingress | over its service level; `Received` over the `REQ-CS-7` bound |
| Observation completeness | adapter: cursor lag behind the finalized inbox head, open gaps, last re-scan difference | any gap or difference; lag over a limit |
| Undelivered notifications, count and age (`REQ-EV-6`) | core outbox, through the delivery worker | oldest over a limit |
| Anchor lag | core: time since the newest `Final` checkpoint was created | over twice the cadence |
| Reconciliation outcome per axis | reconciliation job | any non-zero result, or no run in 24 h |
| Liquidity of positions that fund settlement costs (`REQ-SN-2`) | adapter: signer native balances; instance positions per asset | under the floor |

Three more signals are added: ticker age (`DEC-6`, `RISK-7`); `Needs-Review` count and oldest
(`REQ-OP-6`, `AC-C4.1`); and Kreivo block production, because blocks need a crank when the chain is idle.
`ops/exporter` polls the core's signal queries through ingress. It uses an operator principal with a
read-signals authority, so every read is attributed (`REQ-PV-5`). The queries return counts and ages,
never ledger facts. VOS itself exports only counters, so each unit exposes its own
metrics. **Proposal — stack:** Prometheus, Alertmanager and Grafana, with a pager service. *Alternative:*
OpenTelemetry to a hosted backend. A failed reconciliation pages. It never opens a ticket (`NFR-9`).

**Daily reconciliation** (`NFR-9`), `ops/reconcile`.
- **Axis 1, per (account, asset).**
  - *(a) Commitment check.* The kernel keeps each account's debits and credits per layer as rolling sums
    of Pedersen commitments. The job sums the entry commitments from history per
    (account, layer, side) and compares the result with the account leaf. Point equality is exact, so one
    minor unit shows. It needs no opening and no privileged read.
  - *(b) Rebuild check.* A leaf store rebuilt from the delta log equals the live store bit for bit
    (`F-006`).
  - *(c) External source.* During shadow and cutover (`DEC-14`), values are compared with the legacy book.
    That needs openings. So it runs inside the core as the one instrumented privileged read, with a
    justification (`REQ-PV-4`), and returns only mismatches.
  Under `DEC-2`, each holding account is its own row.
- **Axis 2, per asset.** The sum over accounts of (debits − credits) per layer is the identity point.
  `trial_balance` also returns exactly zero (`INV-5`).
- **Axis 3.** `F-017`'s comparator at a fresh cut, for each instance and asset (`REQ-PR-5`).
- **Suspense.** A non-zero `Suspense` balance and its age are reported (`REQ-LA-2`, `RISK-8`).
- **Recording.** **Proposal:** each run writes its result as an operator record in the audit trail, and
  as a metric (`AC-J3.1`). *Alternative:* a table in an ops database.
- **No repair.** The job only reads. Any correction is an operation through `CTR-OPS` (`REQ-AT-3`).

## Test plan
- **Drill** in CI at small size and in staging at production-like size: `AC-K5.1`, `INV-5`, `INV-6`,
  `NFR-4`. The result is recorded (`NFR-10`).
- **Replay restore.** Restore to an older backup, then replay. The composite root equals the root before
  the failure (`RISK-5`).
- **Fault injection per axis.** Add one minor unit of drift to a copied leaf (axis 1). Unbalance a copied
  store (axis 2). Send one minor unit to the position outside the pallet's calls (axis 3, `AC-J3.2`). Each
  pages the test route.
- **Signals.** Push each signal over its line in staging, and check that the alert fires (`AC-C4.1`,
  `AC-C4.2`).
- **Privileged read.** The build-time count of privileged-read call sites includes axis 1(c) (`REQ-PV-4`).

## Exit criterion
A restore drill on a production-size staging book re-derives every balance, finds no missing acknowledged
operation, and recomputes the latest `Final` anchor. The daily job pages on an injected difference of one
minor unit on each of the three axes.

## Open points
1. **Proposals:** export-first core backup; drill cadence; the Prometheus stack; commitment-based axes 1
   and 2; results in the audit trail.
2. **VOS backup** on `saga` is not qualified. The fallback, export, needs `REQ-RC-3`
   proven at volume (`F-014`).
3. **Replay needs deterministic private inputs.** Blindings and signing nonces must be re-derivable from a
   secret seed (`F-005`). Logging them is forbidden (`REQ-RT-6`).
4. **"Every independent source"** (`NFR-9` axis 1). Outside migration, the only independent sources are
   the commitments and the rebuilt store. The owner should confirm that is enough.
5. **Operational dependencies.** Kreivo liveness needs a crank. Filling a gap from an archive depends on
   one archive node.
6. `RISK-5`, `RISK-7` and `RISK-8` are mitigated here. `DEP-3` does not affect V0.
