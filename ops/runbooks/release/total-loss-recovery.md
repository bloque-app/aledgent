<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: total-loss recovery from the checkpoint archive

`NFR-4`, `NFR-10`, `NFR-21`, `INV-5`, `INV-6`, `INV-19`, `REQ-AT-3` · `DEC-22` (b), `DEP-11`, `F-021`,
`F-005` (`T-005-16`) · **Owner:** platform, with the owner. **Pages:** `AledgentSignalsDown`,
`AledgentReceivedOverDecisionBound`, the platform's node health alarm, `AledgentCoreArchiveStale`.

**When:** the deployment has lost every live copy of a writer's state. That means a quorum of voters
after `F-021`, or, in the single-voter profile, the one execution node's core volume or its zone. Losing
one node is not this runbook ([replace-node](replace-node.md)).

**The rules that do not bend:**

- A restore writes only into **empty** stores, through the tools below. Nothing edits a store in place
  (`REQ-AT-3`).
- One core identity never runs on two machines (`INV-25`).

## The archive

**An S3 sink first, a stateful archiver later**, behind the same sink and source interface (`DEC-22` (b)).

- **Single-voter profile (built: `T-005-15`).** The core host streams every decision of its log
  (`archive.bin`, `replies.jsonl`) and checkpoints of the Agent's runtime state to the bucket
  ([core-archive](core-archive.md)). That is path B.
- **With `F-021`** (`DEP-11`): the voters publish each quorum-certified checkpoint and the log segments
  after it, through VOS's checkpoint-archive sink and source, over the same bucket. That is path A.

## Preconditions

- [ ] **Declared** by the owner as a total loss. Every old node is terminated and fenced.
- [ ] **PostgreSQL survives.** A replicated database server, or a point-in-time restore of
  `aledgent_ingress` and `aledgent_settle` (`opsctl restore pg-script` from the base backup and the WAL
  archive). The received-log is the source of every acknowledged command.
- [ ] **Keys** available to the signer from Vault: the key-ring root (without it the book cannot be imported
  or signed; [key-loss](../key-loss.md)), the backups' sealing key, the clerk key and the Agent keys.
- [ ] The **audit backup** is verified (`opsctl backup verify`).
- [ ] An isolated host is ready to run the drill's checks before anything serves.

## Steps

### Path A: from the archive sink (with `F-021`)

1. **Pick the checkpoint.** List the bucket. Take the latest checkpoint whose quorum certificate verifies
   against the Agent's committee, and every log segment after it, contiguous. A gap in the segments
   means stopping at the gap: the received-log covers the rest (step 4).
2. **Fresh nodes**, three per Agent, with no local state. Create the Space's nodes under the same Agent
   identities. Do not create new Agents: that would be a re-genesis, which `NFR-17` forbids.
3. **Install** the checkpoint on each voter and replay the segments. The Agents resume at the last
   certified commit.
4. **Ingress** starts against the surviving received-log. The forwarder re-submits every row the log
   holds undecided. Rows decided after the last certified commit come back with **the reply the log
   recorded**; an exact retry answers from the retained result (`INV-18`). A different reply is a
   divergence: **stop** ([divergence](divergence.md)).
5. **The adapter** starts on its database. It resumes from the head it last accepted (the layer is the
   truth for settlement), reads the events since and folds them to check them against the head Kreivo
   holds; behind what a full node keeps, it catches up from the history endpoint. A difference raises
   `AledgentSettlementChainMismatch` ([gap-filling](../gap-filling.md)).
6. **The audit store** is restored into an empty directory, on media apart from the book
   (`opsctl restore audit`).
   - Restore it from a backup taken **at or before** the core's restore point. The core ships from the
     store's head onward, re-deriving every record after it from its archive (`T-013-13`).
   - A store **ahead** of the restored core holds records of decisions the restored book no longer has.
     The core's records at those numbers differ, the store refuses them, and the core holds every input
     ([audit-shipping](audit-shipping.md), "Refused").
   - Keep such a store aside, read-only, as evidence of what was lost. Ship the restored book to a store
     restored from the earlier backup. Never cut a store back by hand.

### Path B: single-voter profile, from the core host's archive

The recovery point is the archive's last segment. With ingress's received-log intact, step 4 re-decides
everything after it, and no acknowledged operation is lost ([core-archive](core-archive.md), "The
recovery point").

1. **Check the archive and lay it out on the fresh node.** On the replacement node (or the drill's
   isolated host), with the node's signer running and reading its keys from Vault (`F-035`: the
   deployment file's `core_keys` names them; no key goes into any environment, and the core host and
   `opsctl` refuse or ignore one there), and a read-only role on the bucket. Run it from the **core
   image** the node ran (by digest): it carries `opsctl` beside `aledgent-core-host` and the Agent's
   programs, which `--core-host` needs on one filesystem. Mount the deployment file, the signer's
   socket directory and the empty state directory as the core's unit does:

   ```sh
   podman run --rm --entrypoint /usr/local/bin/opsctl \
     -v /etc/aledgent/ingress.json:/etc/aledgent/ingress.json:ro \
     -v <signer socket dir>:<signer socket dir> \
     -v /var/lib/aledgent/core:/var/lib/aledgent/core -v /var/lib/aledgent/drills:/var/lib/aledgent/drills \
     <core image>@sha256:<digest> \
     restore core --from 's3://<bucket>/<prefix>?region=<region>' \
     --config /etc/aledgent/ingress.json --state /var/lib/aledgent/core \
     --anchors-rpc <Kreivo archive node URL> --report /var/lib/aledgent/drills \
     --core-host aledgent-core-host \
     --runtime /usr/share/aledgent/agent_runtime.pvm --actor /usr/share/aledgent/core.vos
   ```

   Take the bucket and prefix from the core unit's `--archive`, without `kms-key-id`: the prefix is
   per genesis (`core/g8`, not `core`). A chainspec book (`F-036`) needs its chainspec: `opsctl` reads
   it from the archive (`<prefix>/chainspec/<digest>.bin`, by the digest the deployment's genesis
   commits, and refuses any other), and passes `--archive` to the core host's `restore`, which lays
   out the chainspec and its kept `verify-genesis` report. `--chainspec <file>` overrides the
   archive's copy. Pass `--unnamed-code` and `--genesis-manifest <file>` when the core unit's `serve`
   has them; `opsctl` hands both to the core host.

   It reads every segment and checks each one: its digest, its offsets and the entries' hash chain.
   The segments are read 32 at a time (`--fetch-concurrency <n>`, 1 to 256) and checked strictly in
   order, as one at a time would: a missing or damaged segment ends or fails the read exactly as
   before. One request a segment at 86 ms (the 2026-10-15 drill, with the CLI) is about 90 minutes for
   65k segments one at a time, and **about 3 minutes** 32 at a time; it prints the time it took. It
   re-derives the book natively from genesis, reaching every entry's roots, and runs the drill's checks:
   `INV-6`, `INV-5`, the roots at the archive's point, `INV-19` against the anchors on Kreivo, and
   `NFR-4` against every archived reply. It **refuses** if any check fails. It also refuses if Kreivo
   holds an anchor after the archived book's last checkpoint (`archive-behind-anchor`: the archive does
   not reach the book the layer anchored), or if the archive is damaged.

   Then it lays out the empty `--state` directory. `aledgent-core-host restore` starts the Agent from the
   archive's latest checkpoint and re-sends every archived input after it through VOS, each checked
   against its archived entry. The Agent must end at the archive's point: its input number, log head and
   roots. Without `--core-host` it prints that command, to run on the node in the same image; it then
   records no `roll-forward` check, so check the core host's `restore.done.json` by hand: its `rolled`
   `to` and `kernel_root` are the archive's point.
2. **Start the core host** on the directory, with the same `--archive` (the archiver resumes where the
   bucket stands). Do **not** start ingress yet.
3. **Replay the received-log's tail** through the restored core:

   ```sh
   opsctl restore core-tail --config deployment.json --state /var/lib/aledgent/core
   ```

   It re-sends every input the log shows decided after the archive's point, in order. Each must come back
   with the reply the log recorded. A different reply stops it: that is a divergence, so **stop**
   ([divergence](divergence.md)).
4. **Start ingress.** Its forwarder sends the rows still `Received`.
5. Steps 5 and 6 of path A (the adapter, the audit store).

**If the received-log is lost too**, skip step 3. The decisions after the archive's point are lost: that
is the archive's lag at the failure, which the report records. Restore PostgreSQL to the latest point
first (preconditions) and replay what it holds.

**A volume snapshot**, if the deployment takes them, is a fallback only, for when the owner declares the
archive unreadable. A volume from a snapshot is older than the archive: the core host starts on it and
replays its own `archive.bin`, step 3 replays the received-log's tail, and its archiver refuses to write to
the old archive (`AledgentCoreArchiveConflict`), so it needs a new prefix.

## Verification

Run the drill's five checks on the restored state **before it serves** ([restore](../restore.md)):

- [ ] `INV-6`: every balance equals its entries.
- [ ] `INV-5`: the trial balance is zero per asset, with credits in transit counted (`REQ-RT-11`).
- [ ] The composite root equals the last certified one: the checkpoint's under A, the archive's point
  under B (`archive-point`, and the core host's `restore.done.json`).
- [ ] `INV-19`: every `Final` anchor on Kreivo matches its commitment, recomputed where the restored log
  holds its sequence.
- [ ] `NFR-4`: every operation the received-log shows acknowledged or answered is present.

Then:

- [ ] Reconciliation on demand: axes 1, 2 and 3 at zero.
- [ ] A fresh core backup and audit backup taken at once.
- [ ] Recovery time and data-loss window recorded.

## Rollback

There is nothing to roll back to. The old nodes are gone. If a check fails:

- do not serve;
- try the next older checkpoint (A), or (B) `--from-genesis` (the Agent founded afresh, every archived
  input re-sent), on a new isolated host;
- if every candidate fails, escalate to the owner: it is an `INV-5` or `INV-6` breach.

Never "fix" a restored store to make a check pass.

## Exercised

Path B on every change: the CI job `restore drill (NFR-10, Agent core)` (`ops/tests/agent_drill.rs`), and
monthly over VOS in the stress workflow's drill ([restore-drill](restore-drill.md)). Path A waits for
`F-021` and is not exercised yet.
