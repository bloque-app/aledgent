<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: the restore drill

`NFR-10`, `NFR-4`, `INV-5`, `INV-6`, `INV-19`, `REQ-AT-3` · `F-018`, `F-005` (`T-005-17`) · **Pages:**
`AledgentDrillFailed`, `AledgentDrillOverdue` (warning) · **Owner:** platform.

The procedure is [restore](../restore.md). This page sets the cadence and what counts as a pass for
production. A drill restores only into empty stores on an isolated host; nothing in it edits a store or
moves value outside the command surface (`REQ-AT-3`).

## Automated

The drill of the Agent core runs without anyone:

| Where | What | When |
|---|---|---|
| CI, job `restore drill (NFR-10, Agent core)` (`ops/tests/agent_drill.rs`) | Over the native Agent: a deployment streams every decision to its archive; its node is destroyed; a fresh node is restored from the archive; the received-log's tail is replayed. Passes only if, up to the archive's point, the restored logs are the lost node's byte for byte and every reply the received-log recorded is restored (no acknowledged operation lost), and if, after the tail, the restored node is the lost one (zero divergence: the same logs, input number, log head and roots). Also refuses an archive the anchors or its roots contradict | Every pull request and push to `main` |
| Stress workflow, `drill=true` (`e2e/stress/run.sh`, `drill`) | The same over VOS, through `aledgent-core-host`, `opsctl restore core --from` and `opsctl restore core-tail`, after a load. The summary records the restore time and whether the restored logs equal the lost node's | The first of every month (schedule), after every VOS or CipherClerk pin change, and on demand |
| CI, job `core archive on S3 (Object Lock, KMS)` | The sink against an S3 API with Object Lock and a KMS key: written once, encrypted and locked | Every pull request and push to `main` |

The production-class drill below still runs by hand, on the deployment's own archive.

## Cadence

| When | Which parts |
|---|---|
| Before production use (an `M-3` gate) | The book and the keys |
| Monthly in production | The book |
| After every change of the VOS or CipherClerk pin, or of the root format (compact SMT) | The book |
| After every key change | The keys |

## Preconditions

- [ ] An **isolated host** with no network path to production, of the production instance class.
- [ ] The latest verified backups: the core's archive (`opsctl archive verify --from <spec> --config
  <deployment config> --anchors-rpc <archive node URL>`), audit, ingress and adapter (base backup and WAL, `opsctl
  backup verify`).
- [ ] A read-only role on the archive's bucket, and `kms:Decrypt` on its key.
- [ ] The Kreivo node for `--anchors-rpc` is an **archive node**: `INV-19` reads each anchor's state at
  the block it was anchored in, and a node that prunes state (the public RPC endpoints do) answers "State
  already discarded". The drill then fails `INV-19` for the older anchors although the book is intact.
- [ ] The keys for the drill come from Vault's own backup into the drill's signer only, never an
  environment.

## Steps

1. **The book.** Restore into empty stores ([restore](../restore.md)): the audit store, then PostgreSQL to
   the latest archived point, then the core, then the adapter.
   - **Agent core:** from its off-node archive, as [total-loss-recovery](total-loss-recovery.md) path B
     does: `opsctl restore core --from <spec> … --report <drills dir> --core-host aledgent-core-host …`,
     run from the core image (`--entrypoint /usr/local/bin/opsctl`), then the core host on the restored
     directory, then `opsctl restore core-tail`. The report is the drill's record
     (`aledgent_drill_passed`, `aledgent_drill_last_age_ms`).
   - Record the archive's point, the replayed tail and the restore time, and the archive's read time
     (`read and checked … segments in … ms`).
2. **The keys.** Restore the signer's Vault secrets (the key-ring root, the clerk key, the Agent's keys;
   `F-035`) from Vault's backup into a staging Vault, and the backups' sealing key, and start the signer
   on them. A restore that uses them succeeds. Do this at least once before the deployment's first
   production use.

## Verification (a pass)

- [ ] The drill's five checks: `INV-6`, `INV-5`, composite root, `INV-19`, `NFR-4` ([restore](../restore.md),
  "Checks").
- [ ] Recovery time and the data-loss window recorded, against the stated RTO and an RPO of 0 for
  acknowledged operations: the archive's point, and the received-log's tail replayed after it with every
  reply as recorded. For the Agent core, the archive's lag at the "failure" is the window that would be
  lost if the received-log were lost too.
- [ ] Results recorded (pass or fail, times, digests) with aggregates only, never personal data. The
  exporter reads the latest record: `aledgent_drill_passed`, `aledgent_drill_last_age_ms`.

## Rollback

A drill changes nothing in production. A failed drill before production use holds the deployment back
until it passes. In production, a failed drill is an incident for platform: find why before the next
backup cycle relies on the same path. Destroy the isolated host and its keys after the drill.

## Exercised

The jobs in "Automated" above: `ops/tests/agent_drill.rs` on every change, and the stress workflow's
drill monthly and on demand. The keys part is not exercised automatically yet; it is exercised by hand in
a staging deployment.
