<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: restore, and the restore drill

`F-018` T-018-02, T-018-03, T-018-08 · `NFR-4`, `NFR-10`, `REQ-RT-3`, `INV-5`, `INV-6`, `AC-K5.1`

**Pages that lead here:** `AledgentCoreBackupLate`, `AledgentDrillFailed`, `AledgentTickerStale`,
`AledgentReceivedOverDecisionBound`, `AledgentOldestNonTerminalOverServiceLevel` when the core is lost.
**Owner:** platform on-call. **Rule that does not bend:** a restore writes only into an empty deployment,
through `F-014`'s import; nothing here edits a store in place (`REQ-AT-3`). A correction to the book is an
operation through `CTR-OPS`.

**The core Agent** (`F-005`) has no backup job: its host streams every decision to the off-node
archive, and its restore is `opsctl restore core --from <archive>` then `opsctl restore core-tail`
(`release/total-loss-recovery.md`, path B; `release/core-archive.md`). The
steps below are the in-process core's; steps 1, 2 and 5 apply to both.

Both run from the core image, which carries `opsctl` beside `aledgent-core-host` and the Agent's programs
(`--entrypoint /usr/local/bin/opsctl`; `deploy/docker/core.Dockerfile`).

**A chainspec book** (`F-036`): `opsctl restore core --from` reads the chainspec from the archive by the
digest the genesis commits (`--chainspec <file>` overrides it), and with `--core-host` passes `--archive`
on: `aledgent-core-host restore --archive <spec>` lays out the chainspec and the
book's `verify-genesis` report from the archive, where `install-verified --archive` kept them (`T-036-25`).
A restored book whose report was never archived takes no command (`ERR-InitialStateNotSealed`) until it is
installed again: `aledgent-core-host install-verified --config <file> --state <dir> --report <the window's
report>`. Reads are answered meanwhile.

## Before you start

1. **Isolate.** A drill runs on a host with no network path to production. A restore runs on the
   replacement host, and the failed core must be down for good: one core identity never runs on two live
   machines (VOS's source). Stop the old `aledgent-ingress` process if it is still up.
2. **Collect the backups** (`ops/backup/schedule.toml`): the latest verified core backup, the latest audit
   backup, the ingress and adapter base backups with their WAL archives. Verify each:

   ```sh
   opsctl backup verify <root>/core/<id>
   opsctl backup verify <root>/audit/<id>
   opsctl backup verify <root>/ingress/<id>
   opsctl backup verify <root>/adapter/<id>
   ```

   A backup that does not verify is not used. Take the next older one.
3. **Secrets** come from Vault (the backups' sealing key from KMS), into the environment, never onto a
   command line:
   `ALEDGENT_OPS_SEALING_KEY` (opens the openings), `ALEDGENT_INGRESS_DATABASE_URL` and
   `ALEDGENT_INGRESS_SEALING_KEY`, `ALEDGENT_AUDIT_WRITER_CREDENTIAL`, and for `restore core-tail`
   `ALEDGENT_CORE_LINK_TOKEN`. The core's keys are the signer's (`F-035`): with `core_keys` in the
   deployment file, `opsctl` asks the node's signer; only the in-process core's development setup reads
   `ALEDGENT_CORE_ROOT` from the environment.
   The keys themselves are never in these backups; if they are lost, see `key-loss.md`.

## Procedure

1. **Audit store.** Restore it into an empty directory on media apart from the book's:
   `opsctl restore audit <root>/audit/<id> <audit-dir>`. It re-verifies every frame and the chain.
2. **Ingress's and the adapter's PostgreSQL**, to the latest archived point: on the new database host,
   `opsctl restore pg-script --pgdata <empty dir> --base base.tar.gz --wal wal.tar.gz --wal-dir <dir>
   --start "<how the server starts>"` prints the script; run it as the database's system user. The server
   replays every archived segment and promotes at the end of the archive. The data-loss window is the
   archive's `archive_timeout` (ingress's synchronous standby is what keeps acknowledged rows when only the
   primary is lost).
3. **The core**, from its backup and the restored received-log:

   ```sh
   opsctl restore core --config <ingress deployment config> --backup <root>/core/<id> \
     --journal-out <new core journal> --audit-dir <audit-dir> --book-dir <book dir> \
     --leaf-dir <new leaf store dir> --principal <ops identity> --credential <its credential> \
     --report <drill results dir>
   ```

   It imports the export (every digest, both roots, the openings against every commitment, the audit head,
   the principal's `import_book` operator kind), replays every input the received-log holds after the
   backup point in input order — each must come back with the reply the log recorded — rebuilds the leaf
   store from the delta log, runs the drill's checks, and writes the core's journal rebuilt from the log.
   It exits non-zero if any check fails: **stop, do not serve.**
4. **Serve.** Point `core_journal` in ingress's configuration at the rebuilt journal and start
   `aledgent-ingress serve`. The core replays the journal; the forwarder decides every row the log holds
   undecided.
5. **The adapter.** Start `aledgent-settle` on the restored adapter database. It reads the settlement
   layer from its cursor — the layer is the truth for settlement — and its re-scan compares the inbox with
   what it stored and what the core ingested (`REQ-IN-5`). Any difference is an incident: see
   `gap-filling.md`.
6. **Take a fresh core backup** and a fresh audit backup at once. The restored core's audit trail
   continues from the export's head with the import's own record.

## Checks (the drill's five)

`INV-6` (every balance equals its entries), `INV-5` (the trial balance is zero per asset), the composite
root (the rebuilt leaf store's root is the book's; in a drill, both roots equal the source's last
committed ones), `INV-19` (every `Final` anchor on the settlement layer with its commitment, and recomputed from rebuilt
state wherever the restored log holds its sequence: the anchors before the backup point are checked
against the layer only; `--anchors-rpc` names the node to read), and `NFR-4`
(every acknowledged operation present). Recovery time and the data-loss window are recorded with the
result; the exporter reads the latest drill record (`aledgent_drill_passed`, `aledgent_drill_last_age_ms`).

**`--anchors-rpc` must be an archive node.** `INV-19` reads the anchors' state at the block each was
anchored in, which can be long ago. A node that prunes state (the public Kreivo RPC endpoints do)
answers "State already discarded" for those blocks. Since `T-026-12` that is no longer reported as a
fault of the book: the check is **`Unknown`** (below), says "needs an archive node", and the drill has
not passed. It is the node, not the book: name a node that keeps every block's state and run the check
again.

**`Unknown` is not a pass and not a failure** (`T-026-12`). A check is `Unknown` when nothing it read
contradicts the book and some anchor it needed is in state the node no longer keeps. `INV-19` is the check
that can be: an anchor that is no longer its anchor sequence's latest, and, after a governance reset of the
settlement contract (amendment 0039), the previous book's anchors, of which only the last is kept in
current state. An `Unknown` check never counts towards a pass and blocks one: the drill's record is not
`passed` (so `aledgent_drill_passed` is 0 and `AledgentDrillFailed` fires), and `opsctl restore core --from`
and `opsctl archive verify` refuse, with a message that says "not a pass" and names the remedy, never "a
check failed". An `Unknown` never hides a failure: a checkpoint the layer contradicts fails the check even
when others are unknown. A venue that was reset is read **per book**: the checkpoint's commitment names
which anchor sequence is the book's (the way the verifier names it); numbers the old book also used are
not read as this book's. The detail says which sequences the anchors were found in: the venue's current
one, or "a book re-founded since" (what a restore of an older book's backup finds).

A failing check's detail in the report (and on the console) is the count of what it found and at most 20
examples, not every item: the count says how widespread it is, the examples say what kind.

## Cadence (accepted proposal)

Before production (a gate of `M-3`), then monthly, and after every change of the VOS or CipherClerk pin.

## Exercised

In CI at small size: `ops/tests/restore.rs` (in memory) and `ops/tests/postgres.rs` (PostgreSQL restored
in a new container from `pg_basebackup` and the WAL archive). The staging run at production-like size is
deferred (`features/F-018-operations/NOTES.md`).
