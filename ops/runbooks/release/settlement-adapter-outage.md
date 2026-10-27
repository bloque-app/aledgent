<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: settlement adapter outage

`REQ-SA-5`, `REQ-SA-6`, `REQ-SA-7`, `REQ-OP-6`, `REQ-OP-9`, `INV-10`, `INV-11` · `F-016`, `F-027` ·
**Pages:** `AledgentAdapterNotServing` (not serving for 5 minutes), `AledgentAdapterRuntimeUnconfirmed`
(a `runtime:<spec_version>` alert open), `AledgentAdapterSignalsStale`,
`AledgentSignalsDown`, `AledgentObservationLag`,
`AledgentAnchorLag`, `AledgentLaneUnderFeeFloor`, the platform's task-not-running alarm · **Owner:**
settlement-operations.

**What an outage means.** The adapter is the only thing that touches Kreivo. While it is down:

- **Internal transfers, reservations and queries carry on.** They never touch the settlement layer
  (guardrail 9).
- **New external legs are refused** with `ERR-SettlementLayerUnavailable` once its status lease lapses.
- **In-flight batches** land or expire on chain, and are decided when the adapter returns.
- **Nothing is released on a timer** (`REQ-OP-9`). An operation past its deadline goes to `Needs-Review`.

## Preconditions

- [ ] Confirm the state: `aledgent_adapter_serving` is 0, or the signals file is stale. Read the adapter's
  alerts (`aledgent_adapter_alerts{kind}`): `unreachable`, `dry-run`, `batch:<lane>`, `gap:*`,
  `review:<op>`, `runtime:<spec_version>`.
- [ ] Rule out a chain-side cause first: `aledgent_finalized_stall_ms` and `aledgent_finality_lag_blocks`.
  If blocks or finality have stalled, follow [kreivo-finality-stall](kreivo-finality-stall.md) instead.

## Steps

1. **The task is not running.** The container scheduler restarts it. If it crash-loops, read the
   adapter's log stream.
   - A smoldot panic aborts the process by design, and the restart resumes it from its store.
   - Repeated panics on one chain spec point to the boot nodes (step 3).
2. **It runs but does not serve** (blind light client). It restarts itself after **3 minutes** blind.
   Between restarts:
   - check the egress path to the boot nodes (wss 443, tcp 30333);
   - check that the Kusama checkpoint in the image is recent enough for warp sync. A stale one makes a
     cold start take longer than 3 minutes, and the fix is a new pin and image.
3. **Boot nodes.** Check the chain spec's boot nodes from outside the deployment's network. If entries
   are dead, deploy a chain spec with live ones (`deploy/templates/chain-specs/`).
4. **Dry runs fail** (a `dry-run` alert, nothing signed). The adapter backs off for a minute. If dry runs
   keep failing over the light client, set the optional `dry_run_endpoint` to a Kreivo node the operator
   runs. That node is then trusted for batch sizing only: it can make a batch fail whole, but it never
   moves value. Record it as a finding.
5. **Runtime mismatch.** A Kreivo upgrade that changes only the `spec_version` does not stop the adapter
   (since `T-027-12`); `spec_versions` in its configuration gates nothing. It refuses to serve on a
   runtime upgrade for one of two reasons, which the `refusing` alert names:
   - **`unknown_surface`**: what a transaction must carry changed (the `transaction_version`, the
     transaction extensions, or the types of their data or of `Contracts::call`), and the adapter has
     no format for it. Deploy the adapter build that pins the new format (`services/settle/kreivo-pin*`).
     If the change is known not to touch what the adapter signs (an asset location type that gained a
     variant, for instance), the operator can instead name a known format for that `spec_version` in
     `adapter.formats` (`{ "<spec_version>": "kreivo-133" }`) and restart: the layer still has to
     confirm it before anything is broadcast. Record either as a finding.
   - **`unconfirmed`** (with a `runtime:<spec_version>` alert): the surface matched a format, but the
     runtime refused a transaction signed in it (`BadProof`, `BadSigner`, `UnknownOrigin`, an
     extension's `Custom` error, bytes it cannot decode). The format changed in a way the metadata
     does not show, such as the order of the PASS device's message. Nothing was broadcast. Deploy the
     build with the right format, or name it in `adapter.formats`; a restart asks the layer again. A
     restart alone with nothing changed is refused again. An upgrade enacted while a step was signing
     does not cause it: the transaction is validated by the runtime it was signed for, and a refusal
     from any other runtime is not taken as one (the step signs again). So `unconfirmed` right after
     an upgrade is still a real format change, not a race to restart away, **with one exception**:
     the light client failing to run the validation at all. smoldot reports a virtual machine that
     could not start the function, or a runtime that called a host function a light client does not
     provide, under the same "execution" error as a runtime that cannot decode the transaction, and
     the adapter cannot tell them apart without matching message text (F-027 NOTES, decision 53).
     Read the alert's detail, or the `error` log "did not confirm the transaction format on its
     runtime": a real format change reads "Error during the execution of the runtime: Runtime has
     trapped: …" (or "Runtime has aborted: …"); this case reads "Error during the execution of the
     runtime: " followed by "Function to start was not found.", "Symbol to start is not a
     function.", "Function to start uses unsupported signature.", "The types of the provided
     parameters don't match the signature.", "DataSizeOverflow" or "ForbiddenHostFunction". **A
     restart clears it**: the refusal is held in memory and the restarted adapter asks the layer
     again. If the same answer comes back after a restart, no restart will fix it: record a
     finding, and consider the explicit RPC fallback (`"connection": "rpc"`, the adapter's README).
   - **Neither** (still serving, the `warn` log "neither confirmed nor refused", withdrawals
     acknowledged but not signed): the layer found the first transaction signed on a new runtime
     invalid *for now* (a nonce, fees, the era, the block's resources), which neither confirms the
     runtime nor refuses it. Nothing goes out on that runtime until it confirms; each step asks
     again. After `adapter.not_now_alert_after` such answers in a row (10 by default, about 20 s at
     the template's 2 s `interval`), the adapter raises `runtime:<spec_version>`, whose detail names
     the last answer (`Invalid(Payment)`, `Invalid(Future)`, …), and
     `AledgentAdapterRuntimeUnconfirmed` pages. Check the lane's fees and nonce (step 8) before
     anything else. The alert clears when the runtime confirms.
     **This is urgent, and no other page will say so** (F-027 NOTES, decision 52). A withdrawal is
     instructed with an expiry 150 heights above the finalized head (`expiry_window` in the
     settlement configuration, `deploy/templates/settlement.json`). At Kreivo's 2.0 s blocks and its
     ~16-block finality lag, it can wait unsigned for about **4.5 minutes**; then no era fits, the
     adapter records it as never signed, and about 5 minutes after it was instructed the core
     releases it: the operation ends `Released` (terminal; the payer's withdrawal does not happen,
     and it does not go to review). The oldest-non-terminal page (over 1 h, for 5 min) therefore
     never fires for these withdrawals, `AledgentAdapterNotServing` does not fire (the adapter still
     serves), and the anchor lag pages only after 2 h. Each withdrawal instructed while it lasts
     ends released when its expiry passes; nothing can hold it past that (`REQ-SL-4`). Payers can
     retry once the runtime is confirmed.
6. **Database.** If the adapter cannot reach `aledgent_settle`, follow the database server's own failover.
   The adapter resumes from the head it last accepted.
7. **Keys.** If signing fails, read the signer's log: a signer that cannot log in to Vault or read its
   secret does not start (check its Vault role's binding to the task's identity); for a lane opted into
   KMS, check KMS reachability and the task role's `kms:Sign` grant. Do not fall back to a key in the
   environment.
8. **Liquidity.** A lane or anchor key under its fee floor parks instructions. Treasury tops up
   ([crank-outage](../crank-outage.md), "Liquidity or fees short").

**To pause on purpose:** scale the service to zero (nothing is signed). If the keys are in doubt, the
contract's admin clears a bound or empties the key set.

## Verification

- [ ] `aledgent_adapter_serving == 1`, and the signals are fresh.
- [ ] `aledgent_observation_cursor_lag` returns to about 0 and the adapter accepts the layer's head. If it
  raises `AledgentSettlementChainMismatch`, or cannot read the range it missed, follow
  [gap-filling](../gap-filling.md).
- [ ] Parked withdrawals resume by themselves; nothing is resubmitted by hand. The oldest non-terminal
  age falls.
- [ ] Anchors resume, and the anchor lag falls under the interval.
- [ ] Operations that reached `Needs-Review` during the outage are resolved one by one, with Kreivo's
  records as evidence (`POST /v1/ops/operations/{reference}/resolve`). None is released without evidence,
  and none outside the command surface (`REQ-AT-3`).

## Rollback

- A bad adapter release: redeploy the previous image digest, and it resumes from its store. First check
  whether the release ran a store migration (`services/settle/migrations`). If it did, the previous
  build must accept the migrated schema.
- A dry-run node set in step 4: remove `dry_run_endpoint` once the light client's dry runs work again.

## Exercised

On every change: `ops/tests/alerts.rs` checks that `AledgentAdapterNotServing` and the adapter's other
signals page the test route. Stopping and restarting the adapter against a local network is exercised by
hand in a staging deployment; it is not exercised automatically yet.
