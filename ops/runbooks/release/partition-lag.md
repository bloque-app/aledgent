<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: credit, revocation and stream lag (the sharded profile)

`NFR-19`, `NFR-20`, `REQ-RT-11`, `REQ-RT-13`, `REQ-AU-3` (amendment 0012) · `F-031`, `F-033`, `F-034` ·
**Pages:** `AledgentCreditLag` (p99 over 2 s), `AledgentRevocationLag` (p99 over 1 s), `AledgentStreamLag`,
`AledgentReplayFenced` · **Owner:** platform; security for revocation and the fence.

**Status in this release.** None of these lags exists with **one core Agent**, the single-voter profile
this release deploys:

- a credit is applied in its debit's commit;
- a revocation takes effect at its commit. The only lag left is ingress's API-key cache, whose TTL is
  500 ms and refused above `NFR-20`'s 1 s at start (`api_key_cache_ms`);
- there are no streams between Agents and no native decide.

Their metrics come with the sharded profile's host, once ingress routes to shards, and with native decide
(`F-034`) if it is ever turned on. They are deferred in `ops/src/signals.rs`, and the rules are committed
so that they page the day the metrics appear.

## Preconditions

- [ ] Which signal: credit lag (a peer shard's `credit/<n>` stream), revocation lag (the principals
  Agent's `feed`), a stream's lag (the relay), or a shard's replay fence.
- [ ] Whether one shard or every shard: one points at that shard's host; every shard points at the relay
  or the principals Agent.

## Steps

1. **Credit or stream lag.**
   - Check the relay between hosts: its connections (Noise IK), and the receiving shard's decision rate.
   - A shard behind its own load is a capacity question ([scale-out-in](scale-out-in.md)). Rebalance hot
     accounts with ordinary transfers (`REQ-RT-12`).
   - The value in transit is neither payer's nor payee's until the credit lands: never move it by hand.
2. **Revocation lag.**
   - A revoked credential may act until the revocation reaches every shard. Treat a lag over `NFR-20`
     as a security matter.
   - Check the principals Agent's `feed` stream and the slowest shard.
   - If an exposed credential is involved, freeze the accounts it reaches through the command surface
     (`freeze_account`) while the feed catches up.
3. **Replay fenced** (native decide, `F-034`).
   - A shard's replay through the PVM disagreed with its native decision, and the shard fenced itself:
     it decides nothing more.
   - This is a divergence: follow [divergence](divergence.md). Do not unfence by hand.
   - Turn native decide off, then restart the shard from its last committed state.

## Verification

- [ ] The p99 lags are under their bounds for 30 minutes.
- [ ] Reconciliation on demand: axes 1 and 2 at zero, and every partition's in-transit position balances
  against its peers.

## Rollback

Redeploy the previous digest of the shard host or relay. Nothing here edits a store, and nothing moves
value outside the command surface (`REQ-AT-3`).

## Exercised

On every change: `ops/tests/alerts.rs` checks that each rule above pages the test route when its metric
crosses its line. The lags themselves are not exercised yet: their metrics arrive with the sharded
profile's host.
