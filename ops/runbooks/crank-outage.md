<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: crank outage and liquidity shortfall

`F-018` T-018-08 · `REQ-SA-7`, `REQ-SN-2`, Kreivo's runtime

**Pages that lead here:** `AledgentBlockProductionStalled`, `AledgentLiquidityShortfall`,
`AledgentLaneUnderFeeFloor`. **Owner:** settlement-operations (blocks), treasury (liquidity).
**Rule that does not bend:** nothing is released on a timer and nothing is refunded while the layer is
stalled (`REQ-OP-9`); parked withdrawals resume by themselves when the shortfall clears (`REQ-SN-2`).

## Blocks stalled

The settlement layer produces blocks only while its coretime is cranked; an idle chain can stop
(Kreivo's runtime). While the finalized height does not move, nothing is observed, nothing is final, and
deadlines keep running: operations move to `Needs-Review`, never to a refund.

1. Confirm on the dashboard: `aledgent_finalized_height` flat, `aledgent_finalized_stall_ms` rising, the
   adapter still `serving`.
2. Crank: submit the chain's own block-production trigger from the crank account (the chain team's
   procedure; its key is not the adapter's).
3. Verify the height moves, then that the adapter's cursor catches up (`aledgent_observation_cursor_lag`
   back to zero) and the re-scan finds no difference (`gap-filling.md` if it does).
4. Operations that reached `Needs-Review` while stalled are resolved with the layer's records as evidence
   through `CTR-OPS` (`resolve_operation`), one by one. None is released without evidence.

## Liquidity or fees short

The adapter parks an instruction whose position (less what it has signed and not settled) or whose lane's
fees cannot cover it, raises one alert per position, and resumes it when the shortfall clears.

1. Read which: `aledgent_liquidity_alerts{kind}`, `aledgent_position{asset}`, `aledgent_lane_native`.
2. Treasury tops up the instance's position, or the lane's native balance, on the layer (the pallet's
   calls from treasury's own keys).
3. Verify the alert clears and parked withdrawals resume. Do not resubmit anything: the adapter signs each
   operation at most once, ever.

## Exercised

In CI: `ops/tests/alerts.rs` (a stalled height, a position shortfall and a lane under its floor each
page); `services/settle/tests/flow.rs` (twenty parked withdrawals resume; fee parking). The crank itself
needs the live chain: deferred to staging.
