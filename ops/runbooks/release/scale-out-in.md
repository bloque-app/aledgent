<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: scale out and in

`NFR-21` (a), `NFR-18`, `NFR-17`, `REQ-RT-9`, `INV-25` · `DEC-22`, `DEC-23` · **Owner:** platform.

**The rule** (`NFR-17`, `NFR-21` (a)): capacity is added online, with no change to the book, to any
identifier or to any operation. A change that cannot be made that way is **not made** once the deployment
is in production. Nothing here moves value or edits a store outside the command surface (`REQ-AT-3`).

## What can be scaled in this release

| Lever | Online in this release? | Depends on |
|---|---|---|
| Adapter lanes (more PASS device keys signing in parallel) | **Yes** | A field of the adapter signer's Vault secret (or an opted-in KMS key), the contract's key set |
| Ingress replicas | **No benefit yet**: ingress must share the core's host while the core link is unauthenticated | An authenticated core link |
| The execution node's machine size (vertical) | **No.** A size change is a stop and start of the node, minutes with the volumes attached, and it stops ingress too. Take it ahead of need, in a maintenance window, on the deployment's own growth triggers: memory first, then CPU | Ingress off-host, or `F-021` |
| The core and audit volumes | **Yes**, where the platform grows volumes online; then grow the filesystem | Nothing: page at 70 % full, grow by half again |
| Shard count | **Fixed at genesis**, until online splits exist | `T-033-12` (online split), ingress routing to shards |
| Shards across hosts | At genesis only, and only under outcome A of the multi-host relay experiment | The multi-host relay experiment |
| Replicas per Agent | No | `F-021`, `DEP-11` (membership change) |

## Preconditions

- [ ] The signal says capacity is short: `aledgent_oldest_undecided_row_age_ms` is rising,
  `AledgentReceivedOverDecisionBound`, or the stress suite's sustained rate is approaching the measured
  ceiling for the operator's reference load.
- [ ] The reconciliation is green on every axis. Never scale a book that is diverging.
- [ ] The latest backups are verified.

## Steps

### A. Add an adapter lane (online, in this release)

1. Write a new secp256k1 secret as a field of the adapter signer's Vault secret (`lane-2`), add it to the
   adapter's signer as a key granted to `settle`, and `kill -HUP` the signer. For a lane opted into KMS:
   create an `ECC_SECG_P256K1` key and grant `kms:Sign` on it to the adapter's task role instead.
2. Register its public key as a device of a community PASS account (`Pass.register` for a new account,
   `add_device` for an existing one). A new PASS account also needs a membership, funding, and the
   contract admin's `set_keys` with the full key list.
3. Add the lane to `settle.json` (`adapter.keys`) as a signer key (`{"signer": {"key": "lane-2"}}`).
   Restart the adapter task. The lease hands over, and the adapter resumes from its store.

### B. Add shards (after `T-033-12`; recorded here as `DEC-23` specifies it)

1. Create an empty shard Agent on the target execution node, pinned to aledgent's runtime.
2. The principals Agent records the move at directory version v+1, `moving`.
3. The source shard applies it from `feed`, fences the range (its debits are refused with a retryable
   error), and streams the range's subtrees as `handoff/<n>`.
4. The target applies the handoff exactly once and reports its roots.
5. The principals Agent flips the range at v+2, and ingress routes to the target. The source forwards
   credits for moved partitions until every sender's trim has passed the cut.

One range at a time. The fence is the only unavailability, and only for that range.

### C. Add replicas (after `F-021`)

1. Start a node with no local state.
2. Admit it with `ChangeReplicas`, signed by the Space's authority.
3. It installs the latest quorum-certified checkpoint from the **archive sink**, then replays the log's
   tail ([replace-node](replace-node.md) §C).

### Scale in

- **Lanes:** remove the lane from `settle.json` and restart. Its in-flight extrinsics are settled by the
  adapter's next reads. Then remove the device from the PASS account, or the account from the contract's
  keys.
- **Replicas** (after `F-021`): `ChangeReplicas` to remove a voter, never below three voters.
- **Shards:** `DEC-23` specifies no merge. Shards are not scaled in.

## Verification

- [ ] For each step: every balance unchanged. Reconciliation axes 1 and 2 at zero, run on demand.
- [ ] `NFR-1`, `NFR-16` and `NFR-15` held through the change, read from ingress's metrics.
- [ ] For B: no credit lost or doubled; the in-transit positions match the outbox (`REQ-RT-11`); the next
  anchor is consistent (`REQ-RT-14`).
- [ ] For C: admission time and the rate during admission recorded (`NFR-21` measurements).
- [ ] Record the new decision rate with the mix and principals it was measured at (`NFR-18`).

## Rollback

- **A:** remove the lane (scale in, above).
- **B:** run the same split procedure back toward the source shard. Never edit a store.
- **C:** remove the replica with `ChangeReplicas`.

## Exercised

Not exercised automatically yet. Adding and removing an adapter lane is exercised by hand in a staging
deployment; shards and replicas wait for `T-033-12` and `F-021`.
