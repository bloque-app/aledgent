<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: the core's off-node archive

`NFR-4`, `NFR-10`, `NFR-21` (c), `INV-25`, `REQ-AT-3` · `F-005` (`T-005-15` to `T-005-17`), `DEC-22` (b),
`DEP-11` · **Owner:** platform. **Pages:** `AledgentCoreArchiveLag`, `AledgentCoreArchiveStale`,
`AledgentCoreArchiveConflict` (security). **Warns:** `AledgentCoreCheckpointLate`.

## What it is

In this release the book's writer is one core Agent on one execution node (`DEC-22`, `F-021`). The archive
is what bounds the loss of that node's storage until VOS has membership change.

- **What streams.** `aledgent-core-host serve --archive <spec>` ships every decision's bytes of
  `archive.bin` (every committed input, hash-chained) and `replies.jsonl` (every reply) to the archive,
  at most `--archive-interval-ms` (default 1 s) after the decision. A checkpoint of `agent.state` (the
  Agent's runtime state) follows every `--checkpoint-every` inputs (default 50,000) or 15 minutes.
- **Never on the critical path.** Nothing the core answers waits for the archive. A slow or absent sink
  only grows the lag, and the lag pages.
- **Not shipped:** the replica's leaf store (`replica/`). It is a projection of `archive.bin`, which a
  restore rebuilds and checks root by root, as every restart of the core host already does (`DEC-4`).
- **The sink** is S3 with **Object Lock** (compliance mode) and **KMS** encryption on every object, or a
  directory for tests and the drill. A writer refuses to start on S3 without both. The format and the
  checks are in `crates/core-archive`.

```text
--archive 's3://<bucket>/<prefix>?region=<region>&kms-key-id=<archive key ARN>'
--archive fs:<directory>
```

Every object is locked in compliance mode for **400 days** by default. The spec's `lock-mode` and
`retain-days` override that, and only together. The upload is **asynchronous**, at a 1 s interval.

Credentials come from the instance role or the environment. They are never an argument.

## The recovery point

| What is lost | What is lost with it |
|---|---|
| The core's node and its volume, ingress's PostgreSQL intact | **Nothing acknowledged.** The archive restores to its last segment, and `opsctl restore core-tail` re-decides the window after it from the received-log. Each reply must match the one recorded |
| The core's node, and ingress's received-log with it | The decisions committed after the archive's last segment: **the archive's lag at the failure.** About 1 s plus one upload while the sink is healthy. `AledgentCoreArchiveLag` pages at 5 minutes. This is by design: the archive stays asynchronous, and no decision waits for the sink |
| The archive's newest segments (a damaged archive) | The restore refuses. `--accept-damaged-archive` restores to the point before the damage, and the received-log's tail must cover the rest |

`NFR-21` (c) asks that losing one node's storage loses **nothing** acknowledged. The single voter meets
that only while the received-log survives. The archive bounds the double loss and does not close it.
`F-021` closes it (`PLAN.md`, `DEC-22`).

## Setting it up (the operator provisions the sink; nothing here creates it)

- [ ] **A bucket** of the operator's naming. Object Lock is enabled at creation, with default retention
  **COMPLIANCE, 400 days**. Versioning is on, and **no** lifecycle rule expires current versions.
- [ ] **A KMS key** used only for the bucket's encryption (SSE-KMS). The core host's role may
  `kms:GenerateDataKey` and `kms:Decrypt`; the restore role may `kms:Decrypt` only.
- [ ] **The core host's role** may `s3:PutObject`, `s3:PutObjectRetention`, `s3:GetObject` and
  `s3:ListBucket` on `core/`. It never gets `s3:DeleteObject` or `s3:BypassGovernanceRetention`.
- [ ] **The restore role** (the drill's isolated host, total-loss recovery) may `s3:GetObject`,
  `s3:GetObjectRetention` and `s3:ListBucket` only.
- [ ] The exporter's `[records] core_archive` names `<state>/archive.status.json`. The core's backup age
  is then the archive's (`aledgent_backup_last_age_ms{store="core"}`).
- [ ] `opsctl archive verify --from <spec> --config <deployment config> --anchors-rpc <archive node URL>` passes.

## Alerts

### `AledgentCoreArchiveLag` (page)

Decisions have waited more than 5 minutes for the sink. Losing the node now loses them unless ingress's
received-log survives.

1. Read the status: `opsctl archive status --state <state dir>`. Check `failures`, `last_error` and
   `pending_bytes`.
2. `Unavailable` from S3 means the network path, the role or the KMS key. Fix the cause. The archiver
   retries with backoff, up to 30 s apart, and catches up by itself; a backlog over 16 MiB goes up in
   parts.
3. Do not restart the core host to "unstick" it. A restart is safe (the archiver resumes where the sink
   stands), but it fixes nothing the sink refuses.

### `AledgentCoreArchiveStale` (page)

The archiver has not written its status for 2 minutes. Either the core host is down
([core-halt](core-halt.md), [replace-node](replace-node.md)) or the exporter cannot read the file.

### `AledgentCoreArchiveConflict` (page, security)

The core host refuses to write, because the archive's history is not its logs'. Either two hosts run one
core identity (`INV-25`), or a node was brought up from an older copy (a snapshot) while the archive went
further.

1. **Do not delete anything in the bucket.** Object Lock forbids it anyway, and the archive is the record.
2. Find the second writer and fence it. If the node was restored from a snapshot, the archive is the
   newer copy: restore from it ([total-loss-recovery](total-loss-recovery.md), path B) instead.
3. Escalate to the owner if the archive and a live node disagree on a decision both answered. That is a
   divergence ([divergence](divergence.md)).

### `AledgentCoreCheckpointLate` (warning)

No checkpoint for 2 hours while inputs commit. Nothing is lost. A restore re-sends every input since the
last checkpoint through the Agent, which takes longer. Check `last_error` in the status.

## Exercised

- On every change: the CI job `restore drill (NFR-10, Agent core)` (`ops/tests/agent_drill.rs`). It backs
  up, destroys, restores and replays the tail over the native Agent, and asserts the restored node is the
  lost one byte for byte. The job `core archive on S3 (Object Lock, KMS)` writes, reads and lists against
  MinIO with Object Lock and a KMS key.
- Monthly, and with `drill=true` on demand: the stress workflow's drill over VOS
  ([restore-drill](restore-drill.md)).

Nothing in this runbook changes a store outside the command surface (`REQ-AT-3`): a restore writes only
into an empty directory, and every check that fails refuses before anything is laid out.
