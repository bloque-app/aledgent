<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: replace a node

`NFR-21` (b), `NFR-4`, `NFR-5`, `INV-25` · `DEC-10`, `DEC-22` (b) · **Pages:** the platform's node health
alarm, `AledgentSignalsDown`, `AledgentReceivedOverDecisionBound`, `AledgentTickerStale` ·
**Owner:** platform.

**The rule that does not bend:** one core identity never runs on two live machines (`INV-25`;
[restore](../restore.md)). Fence the old node before you start the new one.

## Preconditions

- [ ] You know which kind of node it is: the **execution node** (core and ingress), or the **adapter's
  container task** (the scheduler replaces that by itself; see §D).
- [ ] The old node is stopped or terminated, and confirmed so from the platform's own console or API. Its
  core container cannot come back.
- [ ] The secrets for the new node are in Vault. The node's identity is unchanged, so the signer's Vault
  role still binds it, and on the new node only the signer's user can reach the node's credentials.
- [ ] The database server is healthy. Ingress's received-log is what makes RPO 0 for acknowledged
  commands.

## Steps

### A. Single-voter profile: the execution node, with its volumes intact

1. **Fence.** Stop the old node. Confirm the core and audit volumes are detached.
2. **Launch** a replacement from the same image and configuration, **in the same zone** as the volumes,
   if the platform's volumes are zonal. A node group of size one can do this and the next step by itself.
3. **Attach** the core volume and the audit volume, and mount them where the deployment configuration
   expects them (the core's `--state` and `audit_store.dir`).
4. **Start the core**, then ingress. The core replays `archive.bin` and rebuilds its replica; the
   startup time grows with the book. The forwarder then decides every row the log holds undecided.
5. Register the new node with the load balancer. If the log stream is named after the node, the new
   node's logs appear under a new name.

### B. Single-voter profile: the volume or the zone is lost

This is a restore: follow [total-loss-recovery](total-loss-recovery.md).

### C. After `F-021`: a replica of any Agent

1. Remove the lost voter with `ChangeReplicas`, signed by the Space's authority. Two of three keep
   committing in the meantime.
2. Start a fresh node with **no local state**: a container task or node with no persistent volume, in the
   zone that lost one.
3. Admit it with `ChangeReplicas`. It installs the latest quorum-certified checkpoint from the archive
   sink and replays the log's tail. No disk is copied or re-attached.

### D. The adapter's task

The container scheduler starts a new task on its own. The lease passes after `lease_ttl`, and the new task
resumes from its store and its light client's sync. Nothing else is needed unless it does not reach
`serving` ([settlement-adapter-outage](settlement-adapter-outage.md)).

## Verification

- [ ] Exactly one core process is running (the host report, `ps`), and the old node is terminated.
- [ ] `aledgent_oldest_undecided_row_age_ms` falls back to normal. Rows that were `Received` are decided.
- [ ] `NFR-4`: every acknowledged operation is present. Compare the received-log's count of acknowledged
  rows with decided rows plus `Received` rows.
- [ ] Reconciliation on demand: axes 1 and 2 at zero.
- [ ] The next anchor goes final.
- [ ] For C: admission time recorded, against the design value of 15 min at `NFR-2`'s volume, together
  with the decision rate during admission.

## Rollback

- **A:** if the new node fails its checks, stop it and fall back to B ([total-loss-recovery](total-loss-recovery.md)).
  Never start the old node once the new one has served.
- **C:** remove the new replica with `ChangeReplicas`. The remaining quorum continues.

Nothing here edits a store or moves value outside the command surface (`REQ-AT-3`).

## Limit of the single-voter profile, stated plainly

Under A, ingress shares the host, so the book is **unavailable** from the old node's loss until the new
core serves. Commands are refused at the edge during that time, not buffered. `NFR-21` (b) is not met by
the single-voter profile (`PLAN.md`, `DEC-22`).

## Exercised

The stress workflow's fault-injection scenario d kills and restarts the core host and ingress under load,
and the restore drill (`ops/tests/agent_drill.rs`) covers path B. Replacing a node on a real platform is
not exercised automatically yet; it is exercised by hand in a staging deployment.
