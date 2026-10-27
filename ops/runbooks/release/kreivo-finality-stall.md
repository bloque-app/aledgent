<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: Kreivo finality stall

`REQ-IN-7`, `INV-10`, `REQ-OP-6`, `REQ-OP-9`, `NFR-15` (amendment 0018) · **Pages:**
`AledgentFinalityStalled` (the finalised head more than 20 blocks behind the best, for 2 minutes),
`AledgentBlockProductionStalled` (finalised height flat for over 120 s), `AledgentAnchorStuck`,
`AledgentOldestNonTerminalOverServiceLevel` · **Owner:** settlement-operations.

**What it means.** While Kreivo's finalised height does not move:

- nothing is final, so nothing posts against the issuance counterparty (`INV-10`);
- deadlines keep running;
- operations move to `Needs-Review`, never to a refund (`REQ-OP-9`; absence of evidence is not
  failure, `REQ-OP-6`).

Since amendment 0018, a withdrawal **completes at inclusion** (`Observed`). So a stall of finality alone
does not breach `NFR-15` for legs already included. A stall of block production does, because nothing is
included.

## Preconditions

- [ ] Tell the two cases apart. The adapter reports both heights (`aledgent_best_height`,
  `aledgent_finalized_height`):
  - **Blocks stalled.** The best height is flat as well. `AledgentBlockProductionStalled` pages alone.
    Kreivo's collators are not producing: coretime, or the collators themselves.
  - **Finality stalled.** The best height moves and the finalised height does not, so
    `aledgent_finality_lag_blocks` grows. `AledgentFinalityStalled` pages. Kreivo's finality follows its
    relay chain, Kusama.
- [ ] Confirm the adapter itself is serving ([settlement-adapter-outage](settlement-adapter-outage.md) if
  not). A blind light client looks like a stall from inside.
- [ ] Check from outside through a second source: a Kreivo node the operator runs, or a public endpoint.

## Steps

1. **Blocks stalled.** Run the block-production procedure ([crank-outage](../crank-outage.md),
   "Blocks stalled"). The crank account's key is not the adapter's. Escalate to Kreivo's operators.
2. **Finality stalled.** Nothing on aledgent's side changes it. Escalate to Kreivo's operators, and watch
   Kusama's finality.
3. **Do not act on operations.**
   - Do not release a reservation.
   - Do not resolve a leg without evidence.
   - Do not resubmit a withdrawal: the adapter signs each operation at most once, ever.
   - If the contract's admin must stop new withdrawals, clear the asset's bound. In-flight legs still
     resolve on their own evidence.
4. **Communicate** if the stall exceeds 15 minutes, or `NFR-15`'s ceiling for new external legs, under
   the operator's incident process. Internal transfers are unaffected.

## Verification

- [ ] The finalised height moves. `aledgent_finalized_stall_ms` returns to about the block time.
- [ ] The adapter's cursor catches up and it accepts the layer's head with no chain mismatch (a stall past
  what a full node keeps of events needs a node with history: [gap-filling](../gap-filling.md)).
- [ ] The anchor in flight goes final, and the anchor lag falls.
- [ ] Operations that reached `Needs-Review` are resolved one by one, with the layer's records as evidence
  (`POST /v1/ops/operations/{reference}/resolve`, justified). Record how many, and how long each waited.
  Nothing is resolved outside the command surface (`REQ-AT-3`).
- [ ] Time to finality is reported with the release's measurements (`NFR-15`, amendment 0018).

## Rollback

Nothing here changes aledgent's state. If a bound was cleared in step 3, the contract's admin restores it.
Then check that one withdrawal goes final.

## Exercised

On every change: `ops/tests/alerts.rs` checks that `AledgentFinalityStalled` and
`AledgentBlockProductionStalled` page the test route. A stall of the chain itself is not exercised
automatically yet; it is exercised by hand in a staging deployment, against a local network whose
collators are stopped.
