<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-018 — implementation notes

Written while implementing `F-018` (issue #18), 2026-09-23, against the in-process core of `F-009`, and
completed after `F-017` (anchoring, disclosure, the settlement comparison) merged.
The plan's proposals were followed as accepted: the `F-014` export as the primary core backup, the drill
cadence (before production, monthly, after every VOS or CipherClerk pin change), Prometheus with
Alertmanager and Grafana, commitment-based axes 1 and 2. Where the plan was silent, the more conservative
behaviour was chosen (refuse rather than allow, page rather than stay silent); each such choice is listed
here for review.

## Scope of this pull request

Every task, `T-018-01` to `T-018-08`. `T-018-07` was finished after `F-017` merged: reconciliation axis 3
through `F-017`'s settlement comparison, the `Suspense` report, recording each run, and `F-017`'s
`TODO(F-018)` items (paging on anchoring's incidents and on a stuck anchor, the daily comparison, the
instance's attestation key in a runbook), with the drill's anchor check and the anchor-lag signal. What
depends on `F-005` or on a staging environment stays deferred (below).

## F-005 and F-017

- **`F-005` (the core as a VOS Agent) is blocked on issue #38.** Nothing here is VOS-specific. The core
  is `ingress::inprocess::InProcessCore`, reached only through `F-009`'s boundary and a lock-held hook for
  backups and the reconciliation. The VOS space backup in a stop window is `TODO(F-005)`: VOS on `saga`
  has not qualified it. So is the core's host running the core backup and the daily
  reconciliation on their schedule: in process they are library jobs (`backup::core::take`,
  `reconcile::daily`) the host calls.
- **`F-017`** gives the anchor store, the settlement comparison and the clerk key. `F-017` built no
  settlement-position account: the book's position at a cut is derived from the settling postings behind
  the final records below the cut (`F-017` decision 24), and axis 3 is that comparison. `F-014`'s import
  still checks a book root at a posting sequence against `Checkpoints`, the in-process stand-in: `F-017`'s
  anchors commit to the kernel root at an entry sequence, and re-pointing the import is `F-017`'s open
  point for the owner. The drill's `INV-19` check is the one against the settlement layer's anchors.

## What landed

| Where | What |
|---|---|
| `ops/` (new workspace member) | the `ops` crate and `opsctl`: backups, restore, drill, signals, exporter, alert evaluation, reconciliation |
| `ops/src/backup/` | `mod.rs` (manifests with SHA-256 per artifact, `verify`, `latest`), `core.rs` (export + sidecar + sealed openings), `pg.rs` (`pg_basebackup`, `pg_verifybackup`, WAL archive, restore script), `audit.rs` (verified copy), `schedule.rs` (T-018-01) |
| `ops/backup/schedule.toml` | the cadence and retention of every store in the plan's table; the leaf store and the keys excluded |
| `ops/src/seal.rs` | ChaCha20-Poly1305 sealing of the openings under the KMS's sealing key |
| `ops/src/restore.rs` | the procedure (T-018-02): import, log checks, ordered replay, journal rebuild, leaf-store rebuild, adapter re-scan |
| `ops/src/drill.rs`, `ops/src/kit.rs` | the drill's checks and record (T-018-03); an in-process deployment generic over both stores, and `run_drill` |
| `ops/src/signals.rs`, `ops/src/exporter.rs` | the signals as Prometheus metrics, and the exporter (T-018-04) |
| `ops/prometheus/`, `ops/alertmanager/`, `ops/grafana/` | rules, routes, scrape configuration, the operations dashboard (T-018-05) |
| `ops/src/alerts.rs` | the evaluation of those very files in the tests |
| `ops/src/reconcile.rs` | axes 1 and 2 (T-018-06); axis 3, the `Suspense` report, the daily job and its records (T-018-07) |
| `ops/runbooks/` | restore, anchoring (with the attestation key), gap filling, key loss, crank outage, reconciliation failure (T-018-08) |
| `crates/ledger-core/src/ledger/operator/signals.rs` | `Ledger::signals`: counts and ages under the `ReadSignals` operator kind |
| `crates/ledger-core/src/ledger/operator/external.rs` | `Ledger::reconcile_external`: axis 1(c) as a privileged read, mismatches only |
| `crates/conformance/src/cases/f018.rs` | `AC-K5.1`, `AC-C4.2`, `AC-C4.1`, `AC-J3.1`, `AC-J3.2`, run on both in-process targets |

Shared files, all additive: `ledger-core` — `OperatorOpKind::ReadSignals` (code 17, after `F-017`'s three),
anchoring's counts and ages in `Signals`, `ReadKind::Reconciliation`
(code 3), `Openings::from_rows`, the two modules above, and `tests/privileged_reads.rs` (the new place, and
`ops/` walked too); `ingress` — `InProcessCore::checkpoint`, `InProcessCore::restored`, `Sidecar`,
`Query::Signals`, `QueryReply::Signals`, `SignalsBody` (with anchoring's fields), `GET /v1/ops/signals`,
`read_signals` on the wire, the clerk key carried into a restored core,
`openapi.json` regenerated; `settle` — `Adapter::lanes`, `settle::signals`, `ProcessConfig::signals_file`
(optional) and its writing after each step; `conformance` — `Feature::F018`, the case table; the workspace
`Cargo.toml`; CI's no-float check over `ops/`. `ops` depends on `crates/verify` (feature `rpc`) for the
drill's read of the layer's anchors.

## Decisions taken during implementation

### Backups (T-018-01)

1. **A core backup is three artifacts at one input number**, taken under the in-process core's lock
   (`InProcessCore::checkpoint`): `export.bin` (the `F-014` export), `sidecar.json` (what the in-process
   core holds beside the book: the input number `s_b`, the operator's founding identifiers, the delivery
   records of `F-009` decision 15, and the replies of the last 64 inputs) and `openings.sealed`. The export
   is `F-014`'s privileged read: the backup principal holds the `PrivilegedRead` operator kind and gives a
   justification.
2. **The job ships the core's audit outbox before exporting.** The export refuses while the audit store is
   behind (`F-014` decision 8), and the in-process core ships nothing on its own (`F-009` decision 17).
   Trade-off, stated plainly: the in-process core replays its journal from genesis on a restart, and the
   replay does not contain earlier backups' privileged-read records, which the audit store holds. After such
   a restart the next backup's shipment is refused by the audit store (its chain differs), so the backup
   fails closed and `AledgentCoreBackupLate` pages. The way back is the restore procedure, which continues
   the audit chain from the export's head. `TODO(F-005)`: the core's host ships the outbox continuously and
   restarts from its own durable state.
3. **The openings are sealed with ChaCha20-Poly1305** under a sealing key the deployment's KMS provides
   through `ALEDGENT_OPS_SEALING_KEY` (never an argument, never logged), bound to the backup's identity. The
   export holds no blinding and no key (a test scans every artifact for every blinding and for the key root).
   `F-006` decision 16 and `F-014` decision 11 left the sealed form open; whether this sealed file is an
   "exported record" under `REQ-RT-6` stays the owner's.
4. **The key root and the lanes' keys are in no backup**: the schedule refuses a job for them and names them
   as excluded.
5. **PostgreSQL** is backed up with its own tools: `pg_basebackup` in plain format with a SHA-256 backup
   manifest, `pg_verifybackup`, then a tarball with its own SHA-256; the WAL archive by the server's
   `archive_command`, checked by forcing a segment switch and waiting for the archiver (a failing archiver
   fails the job). No password is ever on a command line. In the drill, ingress's and the adapter's
   databases share one cluster; a deployment gives each its own (`F-016`: "a database and role apart").
6. **The audit store's backup is a verified copy** of its files, re-read as a store (every frame re-hashed,
   the chain walked) with a head at least the source's.
7. **Retention**: `schedule.toml` carries `retain` per store; `opsctl backup prune` removes the verified
   backups past it, and never one that does not verify (it is not a backup; an operator looks at it).

### Restore (T-018-02)

8. **The received-log and the core backup must be of one history**: the log's decided rows are checked
   gapless from input 1, not behind `s_b`, and equal to every reply the sidecar kept. Anything else refuses
   the restore before the import (conservative: a log behind the backup would lose acknowledged inputs).
9. **Every replayed input must come back with the input number and reply the log recorded.** The core is
   deterministic (`REQ-RT-3`: time in the input, blindings and nonces derived from the key root and the
   posting); a different reply fails the restore — never a new truth.
10. **The restored core's journal is rebuilt from the log** for inputs `1..=s_b`, then receives the replay,
    so a later start replays it from genesis like any other in-process core (tested: same root after a
    restart). `opsctl restore core` writes it as the file the ingress binary's `core_journal` names.
11. **The first start of a restored core takes the imported book as its base** (`InProcessCore::restored`);
    the delivery records and the reply tail come from the sidecar.
12. **The leaf store is rebuilt into `F-006`'s `BookStore`** (redb, with its archive) from the restored
    book's committed delta log, every commit's root checked, then `F-006`'s `INV-6` and `INV-5` store checks.
13. **The adapter re-scan** runs the restored adapter from its cursor until it has read the finalized inbox,
    then re-scans the whole inbox against its store and the core; any difference fails the drill's
    `REQ-IN-5` check.

### Drill (T-018-03)

14. **Checks recorded:** `INV-6`, `INV-5`, the composite root (and the book root), `INV-19` against the settlement
    layer (decision 33), `NFR-4`, `REQ-IN-5`; recovery time (wall time to a restored core serving) and the
    data-loss window (inputs lost, and the WAL archive's `archive_timeout`). A drill passes only if nothing
    failed and nothing was lost; *deferred* never counts as passed.
15. **`NFR-4`'s acknowledgements are the received-log's command rows**, less the settlement adapter's
    reports: those are observations at coordinates, not operations, and the re-scan checks them. A transfer
    acknowledged at `Received` while the core was down is checked after the restored forwarder decides it.

### Signals (T-018-04)

16. **A new operator kind, `ReadSignals`** (`read_signals` on the wire, code 14), for the plan's
    "read-signals authority". The signal query needs no justification: it is not a privileged read
    (`REQ-PV-4`) — it bypasses no authority over any account and returns no ledger fact, only counts, ages
    and times. It is refused to everyone else, the operator's organisation included until granted.
    Attribution (`REQ-PV-5`) is ingress's access log: the principal and the request id.
17. **Undelivered notifications are counted by the in-process core**, where the delivery records live
    (`F-009` decision 15); the oldest undecided row by ingress's surface, which adds it to the core's reply.
18. **The exporter never touches the settlement layer** (guardrail 8): the adapter writes its signals —
    cursor lag, gaps, open alerts by kind, lanes' native balances against the fee floor, positions, the
    finalized height — to a file after each step (`ProcessConfig::signals_file`), and the exporter reads it.
    Block production is the finalized height and how long it has not moved, tracked across scrapes.
19. **Every value is an integer** (counts, milliseconds, the layer's integer units). An axis or a backup
    never recorded reports its age since the epoch, which pages as "no run" or "late". The review queue's
    owner is a label from the exporter's configuration; the core's owner identity is not exported (an
    identifier). A source that does not answer within the timeout is `aledgent_signals_up 0`, which pages.

### Alerts, routes, dashboards (T-018-05)

20. **The rules are a subset of PromQL**: `metric{label="value"} op integer`, evaluated by `ops::alerts` over
    the committed files in the tests; anything else is refused by the parser. Thresholds are the V0
    proposals written in the rules file (the oldest non-terminal operation over the review service level of
    one hour; `Received` over the one-minute decision bound; undelivered over fifteen minutes; ticker over
    thirty seconds; the finalized height stalled two minutes; reconciliation not run in 24 h; backups two
    cadences late; anchor lag over 2 × 1 h, a placeholder for `OQ-11`). *Amendment 0007
    (2026-09-29):* there was no latency rule, and no 50 ms threshold to correct. Two rules are added:
    **completion over its 30 s ceiling at p99** (`AledgentCompletionOverCeiling` on
    `aledgent_completion_p99_ms`, `NFR-15`, the signal `completion`, ingress's measurement carried in
    the core's signals); the 19 s target is on the dashboard ("Completion (NFR-15)") and pages no
    one. And an **internal fault** (`AledgentInternalFault`, `REQ-ER-2`, amendment 0006 item 8):
    any `internal_fault` incident in the last hour pages.
21. **Severity `page` reaches the `pager` receiver**, whose URL is read from a file the deployment provisions
    (`url_file`): no endpoint and no secret is in the repository, and the parser refuses an inline URL. A
    failed reconciliation pages and never opens a ticket.
22. **An axis not run pages**: `NFR-9` requires every axis daily, and its absence is not a state to get
    used to. Only the drill being overdue is a warning rather than a page.
23. **The Needs-Review queue panel** shows counts, the most overdue entry and the service level per owner;
    the operations themselves stay on `CTR-OPS`, behind an operator's authority.

### Reconciliation (T-018-06)

24. **Axes 1(a), 1(b) and 2 run over a snapshot**: the delta log's history and the account leaves the store
    holds. The fault-injection tests change a copy of it, never the store: one minor unit (`+1·G`) on a
    copied leaf fails axis 1; a forged one-sided entry with its account's leaf made consistent passes axis 1
    and fails axis 2 alone.
25. **Axis 1(c) returns mismatches only** — which (account, asset, balance kind) differ, or which side lacks
    a pair — never a figure (`REQ-PV-4`: the operator learns where to look, not the book). It is a new place
    of the privileged-read mechanism, `ReadKind::Reconciliation`, added to the committed count; the count
    test now walks `ops/` too.
26. **Runs are recorded as JSON files and metrics, and on the audit trail** (decision 30).

### Axis 3, `Suspense` and the run's record (T-018-07)

28. **Axis 3 is `F-017`'s `compare_settlement`, through ingress**, as the operations principal with the
    `compare_settlement` operator kind, at the latest cut the adapter reported. Each asset whose
    difference is not zero is a finding with the asset, the cut and the difference (an aggregate per
    asset, no account's figure). A cut the adapter reported more than a day before is itself a finding
    (`StaleCut`): the adapter reports positions whenever a final anchor wants its comparison, so an old cut
    means anchoring or the adapter stopped. With no report yet (`ERR-EvidenceInsufficient`), or ingress
    refusing the job, axis 3 is recorded as not run, and pages as not run after 24 h.
29. **The daily job** (`reconcile::daily`) runs axes 1 and 2 and the `Suspense` report under the core's
    lock, then axis 3 through ingress, then writes the run's JSON record. `TODO(F-005)`: the host schedules
    it; this is `F-017`'s "daily comparison".
30. **Each run is recorded on the audit trail by the axis-3 command itself**: `compare_settlement`'s
    justification is the run's summary — each axis's count of differences, the number of non-zero
    `Suspense` pairs, and the SHA-256 of axes 1 and 2's findings and the `Suspense` report — so one
    audited operator record, journaled as a core input (deterministic on replay, unlike a write outside
    the journal), carries both the comparison and the run. No new command or operator kind. Stated
    plainly: a run whose axis 3 is refused leaves the core's audited refusal, which does not carry the
    summary; its JSON record does.
31. **The `Suspense` report** lists each `Suspense` (account, asset) with a non-zero balance of any kind and
    when it last left zero, from the journal: no figure. Its cadence is a day: `AledgentSuspenseNotReconciled`
    pages once one has not been zero for longer (a V0 proposal; `REQ-LA-2` asks for "a stated cadence").
32. **Anchoring's signals** come from the core's signal query: the anchor lag (the newest `Final`
    checkpoint's age; before the first, the oldest checkpoint's), the age of the checkpoint in flight, the
    count of anchoring's incidents by kind, and the number of assets whose latest comparison differs.
    Pages: the lag over twice the interval, a checkpoint in flight for an hour, any commitment mismatch or
    unknown anchor (these never clear: the record stays, and anchoring stays stopped until an operator
    acts), and any differing asset.
33. **The drill's `INV-19` check** reads the settlement layer's anchors (`verify::AnchorSource`: the fake
    layer in the tests, `RpcAnchors` over a node in `opsctl restore core --anchors-rpc`). For every `Final`
    checkpoint: its commitment is on the layer and is the one its fields produce; and wherever the restored
    delta log holds a committed state at its entry sequence, the kernel root of those rebuilt leaves is the
    one it committed to. Stated plainly: the import commits the export's leaves in chunks, so the states
    before the backup point are not in the restored log, and anchors before it are checked against the
    layer only; the record says how many of each (in the PostgreSQL run: 4 recomputed, 2 layer-only).
    Without a read of the layer, the check is recorded as deferred, never as passed.
34. **Export format 3** (`F-017`'s six anchoring sections) crosses the backup unchanged: the backup stores
    the export's bytes, and the restore imports them. A test checks the version, the six sections filled,
    and the restored anchor store equal to the source's after the replay. The restored core takes the
    deployment's clerk key (`restore_core`'s `clerk`), so a replay reissues the same disclosures.
35. **The kit anchors**: the adapter has an anchor key, the instance's attestation key is the test clerk's,
    and the position is funded by a deposit through the layer, so the book's position and the layer's agree
    from the first comparison (a position funded outside the pallet would be a difference from the start,
    which is what `AC-J3.2` injects).

### Runbooks (T-018-08)

27. **Six runbooks**, each saying what pages lead to it, who owns it, its steps, and how it is exercised; a
    test refuses any storage write in them (SQL writes, edits of a store's files). Gap filling from an
    archive node is only partly possible today: the adapter can re-scan a range against an archive node,
    but reporting a skipped range's records to the core is not built (`F-016` Deferred), so a missing
    withdrawal is resolved with evidence through `CTR-OPS` and a missing deposit is escalated.

## Small-size run, recorded (the exit criterion's staging run is deferred)

`ops/tests/postgres.rs`, 2026-09-23, on the developer laptop (Apple M2 Pro, Docker Desktop, PostgreSQL
16-alpine, debug build): five rounds of traffic (a transfer, a captured and a released reservation, a
withdrawal executed on the fake layer, a deposit observed, per round), a core backup after the second, a
transfer acknowledged at `Received` while the core was down, the cluster destroyed and restored in a new
container from its base backup and WAL archive.

| | |
|---|---|
| Book restored | 5 rounds of traffic with an anchoring interval after each: 334 inputs at the failure (figures of one run; the ticker's rows vary a little) |
| Core backup | at input 124, after two anchoring intervals; export format 3 with its anchoring sections |
| PostgreSQL backup | about 9.3 MB (base backup and WAL archive, compressed) |
| Replay | 210 inputs after the backup point, each with its recorded reply, to input 334 = the source's last; 1 row (the transfer acknowledged at `Received`) decided after the restore |
| Recovery time | 298 ms from the start of the restore to a replica serving the restored core (debug build) |
| Checks | `INV-6`, `INV-5`, composite and book roots equal to the last committed, `INV-19` (6 `Final` anchors: 4 recomputed from rebuilt state, 2 before the backup point checked against the layer only), `NFR-4`, `REQ-IN-5` (inbox 0..17, no difference): all passed |
| Data lost | 0 inputs; WAL window 1 s (`archive_timeout`) |
| Wall time of the test | 14–19 s, both container starts included; generated data a few MB, deleted with the test's directory |

The run is repeated by CI on every change (`cargo test --workspace`, Docker on the runner). **Deferred:**
the drill in staging at production-like size (no staging environment), and each runbook exercised once in
staging.

## Tests

| Test | Covers |
|---|---|
| `ops/tests/restore.rs` | `T-018-01` (a core backup verifies, holds no secret in the clear, opens only under its key; the audit backup), `AC-K5.1` / `T-018-02` / `T-018-03` in memory (restore to an older backup plus replay gives the pre-failure roots; every check including `INV-19` against the layer, which fails against a layer without the anchors; export format 3's anchoring sections across the backup and the restored anchor store equal to the source's; the outage's deposit issued; a restart from the rebuilt journal; the record), a log behind the backup refused, the journal rebuilt from the log alone |
| `ops/tests/postgres.rs` | `NFR-10`, `AC-K5.1`, `INV-19`: the drill over PostgreSQL restored from `pg_basebackup` and the WAL archive, with anchors before and after the backup point |
| `ops/tests/signals.rs` | `AC-C4.2`, `NFR-11`: every signal scraped (the anchor lag included), no ledger fact; `REQ-PV-5`: the query refused without `read_signals`; a core down is a source down |
| `ops/tests/alerts.rs` | `NFR-11`: a healthy deployment pages and warns no one; every paging rule pushed over its line pages the test route with its signal and owner (the anchoring and `Suspense` rules included); `AC-C4.1` end to end; the dashboards show every signal |
| `ops/tests/reconcile.rs` | `AC-J3.1`: the daily run reconciles three axes and is recorded as JSON and on the audit trail; an axis that cannot run pages as not run; `AC-J3.2`: one minor unit on axis 1 (a copied leaf), axis 2 (an unbalanced copy) and axis 3 (sent to the position outside the pallet's calls), each paging; `REQ-PV-4`: the external source's audited privileged read; `REQ-LA-2`: a `Suspense` balance with its age, paging after a day |
| `ops/tests/runbooks.rs`, `ops/tests/opsctl.rs` | `T-018-08`, `REQ-AT-3`; the binary |
| `ops` unit tests | the schedule, manifests and pruning, sealing, the rules and routes' parser, the exporter's example configuration |
| `crates/ledger-core/tests/privileged_reads.rs`, `api_surface.rs` | `REQ-PV-4`: the new place counted; `reconcile_external` on the reviewed write surface |
| conformance `f018.rs` | `AC-K5.1`, `AC-C4.2`, `AC-C4.1`, `AC-J3.1`, `AC-J3.2`, both in-process targets |

## Deferred

- `TODO(F-005)`: the VOS space backup in a stop window; the core's host scheduling the core backup and the
  daily reconciliation and shipping the audit outbox continuously (in process they are library jobs the host
  calls, and `opsctl` for PostgreSQL, the audit store, pruning and the restore).
- Staging: the drill at production-like size, each signal pushed over its line against a real pager route,
  each runbook exercised once (setting the attestation key, filling a gap from an archive node, a crank, a
  KMS restore).
- `TODO(F-016)`, another feature's: reporting a skipped range's records to the core from an archive node
  (`gap-filling.md` resolves the affected operations through `CTR-OPS` meanwhile).
- The owner: plan open point 4 (whether the commitments and the rebuilt store are enough "independent
  sources" outside migration); the sealed openings' status under `REQ-RT-6`; the V0 alert thresholds and
  the `Suspense` cadence of a day; re-pointing `F-014`'s import at a final checkpoint's kernel root
  (`F-017`'s open point).

## T-018-10: the MVP release's alerts (2026-10-04)

A group of its own in `ops/prometheus/rules/aledgent.yml`, `aledgent-release`, each rule linking its
runbook (`ops/runbooks/release/`). The lines are proposals, as the V0 ones are:

| Alert | Line | Source |
|---|---|---|
| `AledgentCoreHalted` | `aledgent_core_halted == 1`, at once | The core host (`CoreStats::halted`, `stopped`) through `/v1/ops/signals` |
| `AledgentAuditNotShipped`, `…ShipLag`, `…StoreDown`, `…StoreRefused`, `…Backpressure` | not configured for 10 min; oldest unshipped over 5 min; store down 2 min; refused; back-pressure | The core's shipper (`T-013-13`) |
| `AledgentVosAuthorityBacklog` (page), `…Unacknowledged` (warning) | over 191 (three quarters of 256); any for 10 min | The core host's driver: receipts issued whose Acknowledge has not succeeded |
| `AledgentCreditLag`, `AledgentRevocationLag`, `AledgentStreamLag`, `AledgentReplayFenced` | `NFR-19`'s 2 s and `NFR-20`'s 1 s at p99, for 5 min; a fence | Deferred: the sharded profile's host and native decide; none exists with one core Agent |
| `AledgentAdapterNotServing` | not serving for 5 min (it restarts itself after 3 min blind) | The adapter's signals |
| `AledgentFinalityStalled` | finalized head more than 20 blocks behind the best, for 2 min | The adapter's signals, which now carry the best height |

Alertmanager pages the halt and the audit trail's alerts through routes of their own: no grouping
wait, and every 15 minutes while they last. A rule over a deferred metric is evaluated in the tests by
pushing a raw sample, and listed in `ops::signals::DEFERRED`; it pages the day its metric appears.
Archive lag is not here: the Agent core's archive (`T-005-15`) brings its metric and its rules
(`AledgentCoreArchiveLag`, `ops/runbooks/release/core-archive.md`).

## T-018-11: the book's size (2026-10-13)

The owner ruled on 2026-10-13 that an alert fires when a deployment's book reaches three times its
genesis size: that alert is the trigger for `T-036-17`, the disk-backed service store, which ruling
`a0022-q5` requires before a book of about 3× is served. Nothing exported counted the book until now.

**The unit.** Three gauges, all from counts the collections keep (`LeafMap::leaf_count`, a
`BTreeMap`'s length), so the read costs the same at any size and visits no record:

| Metric | What it counts | Why |
|---|---|---|
| `aledgent_book_records` | Identities, ledger accounts, grants (authority, issuance, operator-surface), aliases (the seed's and the ones registered since) and operations, plus the seed's opening rows and credential imports | A chainspec's unit, the one the ruling and `T-036-17`'s "about 1.3 KB a record" are stated in |
| `aledgent_book_leaves` | Every leaf of the service store | What the replica actually holds in memory, and what `T-036-17` moves to disk |
| `aledgent_book_genesis_records` | The chainspec's records, from the header the book keeps; absent for a book founded without one | The denominator, read from the book: a rule can compare against it with no deployment fact |

At genesis `aledgent_book_records` is the chainspec's records plus the rows genesis itself writes: the
operator's identity, the issuance counterparty and an opening position per asset of the opening set
(two plus one per asset of its opening set). After it, each operation, identity, account, grant or alias adds
one; a transition of an existing operation (a capture, a release, an observation) adds none.

*Trade-off, stated plainly.* Records are the ruling's unit, not memory's. A seeded record writes a few
leaves (an identity about six, an alias one); a later operation writes its own number
(postings, a client key, a notification, its record). The two ratios drift apart as the book grows, so
records at 3× is not memory at 3×. `aledgent_book_leaves` is exported so that the drift is visible on a
dashboard; if it matters, the alert moves to leaves with its genesis value read once from the deployment.

**No ledger fact.** Counts only: no identifier, account, amount or reference (`REQ-PV-5`). The size of
the book is operational, as the count of open operations already is.

**The rule is not committed here.** A deployment's rule may name its own genesis as a number (three
times its records, for 30 minutes, a warning), which is a deployment's fact, not the product's: it lives
with that deployment's monitoring. The same line with no deployment fact is
`aledgent_book_records > 3 * aledgent_book_genesis_records`; committing it to
`ops/prometheus/rules/aledgent.yml` (with a dashboard panel, as `T-018-05`'s test requires) is left for
when the owner wants it there.

**The guest is unchanged.** `LeafMap::leaf_count` and what reads it are the host's alone
(`cfg(target_has_atomic = "8")`, the same split `leaves.rs` already makes: the core's guest has no
atomics). The trait is used as `dyn LeafMap` in the guest, so a method the guest compiled would be in
its vtables and change the core package's digest, which a restart checks against the
genesis code *H₀* (`agents/core-host/src/code.rs`) until `F-037` brings upgrades. The library checks
for the guest target (`riscv64em-vos`); the package itself was not rebuilt to compare digests.
