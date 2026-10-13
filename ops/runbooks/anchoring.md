<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: anchoring, the attestation key and the daily settlement comparison

`F-018` T-018-07, T-018-08 · `F-017` · `REQ-PR-1`, `REQ-PR-5`, `REQ-PR-8`, `INV-19`, `NFR-9`, `NFR-11`

**Pages that lead here:** `AledgentAnchorLag`, `AledgentAnchorStuck`, `AledgentAnchorCommitmentMismatch`,
`AledgentUnknownAnchor`. **Owner:** settlement-operations for the lag, security for the incidents.
**Rules that do not bend:** an anchor is a number and 32 bytes, nothing else (`INV-17`); it proves
non-revision, never solvency (guardrail 11); nothing here writes a store by hand (`REQ-AT-3`). The operator
acts only through `CTR-OPS`: `POST /v1/ops/anchors` (kind `anchor_checkpoint`) and
`POST /v1/ops/settlement/comparisons` (kind `compare_settlement`), each with a justification.

## Setting the instance's attestation key (once per clerk key)

The core signs disclosures with the deployment's clerk key, through the signer (`F-035`: the signer's
`clerk`, read from Vault; never in the core's process, never on a command line). Recipients check that signature against the key the settlement layer's instance record
publishes (`F-017` decision 22), so the key must be on the record before any disclosure is issued, and again
after any rotation (`key-loss.md`).

1. Read the clerk key's public key from the signer: `aledgent-signer public <signer.json> clerk` (the secret
   never leaves the signer).
2. The instance's admin — not the adapter, whose keys cannot — sets it with the settlement pallet's
   `set_keys` call (`F-019`), naming the instance's key list unchanged and the new attestation key.
3. Verify from outside aledgent: `aledgent-verify` (`crates/verify`, feature `cli`) reads the instance
   record at the finalized head over the node's RPC and prints the attestation key; it must be the public
   key of step 1. Then issue one disclosure to an internal recipient and verify it offline with the same
   tool: it must pass.

## Anchor lag, or a checkpoint stuck in flight

The core makes a checkpoint every anchoring interval (`OQ-11`; one hour by default) and one at a time: the
next waits until the last is final (`F-017` decision 4). A lag over twice the interval, or a checkpoint in
flight for an hour, means the anchor is not reaching finality.

1. Read anchoring as the operator: `GET /v1/ops/anchors` — the checkpoints with their status, the
   incidents, the latest comparison.
2. If the latest checkpoint is `pending` and never `submitted`: the adapter is not taking it. Check that the
   adapter serves (`aledgent_adapter_serving`), that its anchor key is configured and is one of the
   instance's keys (its start-up assertions say so), and that the anchor key's native balance covers fees.
3. If it is `submitted` and not `final`: blocks are not being finalized, or the extrinsic did not land. See
   `crank-outage.md` for a stalled chain. An extrinsic that provably did not execute is re-emitted by the
   core on the adapter's proof (`F-017` decision 10): do nothing by hand.
4. When the layer moves again the checkpoint becomes final and the lag falls; an on-demand checkpoint
   (`POST /v1/ops/anchors`, with a justification) answers the latest if one is still in flight.

## A commitment mismatch, or an unknown anchor

`commitment_mismatch`: the layer recorded the anchor of a number with another commitment. The checkpoint
never becomes final and anchoring stops until an operator acts (`F-017` decision 9): the number is taken on
the layer. `unknown_anchor`: the layer holds an anchor aledgent never made. Either means someone else wrote
under the instance's keys, or the book the anchors were made from is not this one.

1. Page security. Freeze the instance's anchor key on the layer (its admin removes it from the instance's
   keys) until the cause is known.
2. Run the restore drill on an isolated host (`restore.md`): its `INV-19` check recomputes every `Final`
   anchor it can from rebuilt state and compares each with the layer. A drill that passes shows the book is
   the one anchored; the stray record is not the book's.
3. The way forward is the owner's: anchoring resumes only once the number sequence on the layer and the
   book's checkpoints agree again. No store is edited to make them agree.

## The daily settlement comparison (axis 3)

The daily reconciliation (`ops::reconcile::daily`) asks the core to compare the book's position — derived
from the settling postings behind the final records below the cut (`F-017` decision 24) — with the positions
the adapter last reported, and records the run's summary as that command's justification on the audit trail.
A difference of one minor unit pages (`AledgentReconciliationDifference`, `AledgentSettlementDifference`):
follow `reconciliation-failure.md`. A cut older than a day is itself a finding: the adapter has stopped
reporting, usually because anchoring stopped (above).

## Exercised

In CI against the fake layer: the drill's `INV-19` check passes over anchors before and after the backup
point, and fails against a layer that holds none (`ops/tests/restore.rs`, `ops/tests/postgres.rs`); the
daily comparison reconciles, and one minor unit sent to the position outside the pallet's calls fails axis 3
and pages (`ops/tests/reconcile.rs`); each anchoring alert pages (`ops/tests/alerts.rs`). Setting the
attestation key needs the live chain: deferred to staging.
