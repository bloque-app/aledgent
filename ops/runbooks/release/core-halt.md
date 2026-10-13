<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: the core stalls or halts

`REQ-CS-7`, `REQ-RT-13`, `REQ-AT-3`, `NFR-11`, `NFR-15`, `NFR-16` · `F-032` finding F18, `T-032-17`,
`T-018-10` · **Pages:** `AledgentCoreHalted` (at once, every 15 minutes), then
`AledgentReceivedOverDecisionBound` (oldest undecided row over 60 s), `AledgentTickerStale`,
`AledgentCompletionOverCeiling`, `AledgentOldestNonTerminalOverServiceLevel` · **Owner:** platform.

**The behaviour, as built** (`T-032-17`).

1. An input the Agent fails **alone and definitively** (refused, panicked, out of gas) goes alone first
   on the next try.
2. After **two such failures in a row** (`FAILED_ATTEMPTS = 2`), the core host **halts** on it:
   - an `aledgent-core: ALERT` line on standard error names the input;
   - the host's report shows `halted=1`;
   - every later input answers `Unavailable` without reaching the Agent or replaying the archive;
   - **reads are still answered**, at what the Agent committed;
   - **inputs stay `Received`, in order.**
3. Nothing skips or forces the input (`REQ-AT-3`). The only way on is **a build whose Agent applies it,
   and a restart**.

Before the fix, every retry replayed the whole archive (about 33 s at 16,000 inputs), and the core stalled
for ever. Ticks are now bounded to 8 deadlines each, and the unbounded reads of F18's class are fixed
(`T-032-18` to `T-032-20`).

**The halt has its own alert** (`T-018-10`):

- `AledgentCoreHalted` pages on `aledgent_core_halted == 1`, at the next scrape. It is routed at once,
  and again every 15 minutes. The core host reports it in `/v1/ops/signals` (`core_halted`). It is also
  1 when the core stopped on an incident (a divergence, an archive or a reply log that does not write).
  Then the `ALERT` line or the core's last error says which.
- `AledgentReceivedOverDecisionBound` follows within about 2 minutes.
- The `ALERT` line is in the core container's log stream.
- If the core's reads stop as well, `AledgentSignalsDown` pages instead: the core is not answering.
- A halt can also follow an audit store that refuses records: the outbox reaches its bound and the core
  holds every input. That is not a halt (`core_halted` stays 0). It is `AledgentAuditBackpressure`; see
  [audit-shipping](audit-shipping.md).

## Preconditions

- [ ] Tell a **halt** from a **slow core**:
  - **Halt:** `halted=1` in the host's `core-diag` report, an `ALERT` line, and every new decision
    `Unavailable`.
  - **Slow:** decisions still commit (`decisions` rising) but behind the load. Go to
    [scale-out-in](scale-out-in.md), and compare the load with the deployment's own capacity plan.
- [ ] Capture the evidence: the `ALERT` line, the input it names (an operation reference, or a tick),
  the core host's last reports, and the image digests.

## Steps

1. **Contain.**
   - Ingress keeps acknowledging transfers and reservations (`REQ-AR-7`).
   - A decided command gets `ERR-DecisionUnavailable` after its wait, which is a named refusal.
   - Tell integrators that decisions are delayed, under the operator's incident process.
   - Do not restart in a loop: a restart without a fix halts again on the same input.
2. **Reproduce off production.**
   - Take the input from the received-log (its canonical form) and the core's state (a snapshot of the
     core volume).
   - On an isolated host, run the same image. `aledgent-core-host measure` can replay a workload.
   - Find why the Agent fails where the native replica succeeds: heap (256 KiB guest heap), gas, the
     4 MiB image, or a defect.
3. **Fix forward.** Build an image whose Agent applies the input. The fix goes through the normal pull
   request, review and release gate. If the fix changes the Agent's program, check the runtime and
   package procedure for this deployment's pin.
4. **Deploy by digest.** Restart the core with the new image, then ingress if it was restarted too. The
   core replays its archive, then the forwarder decides the waiting rows in order.

## Verification

- [ ] `halted=0`. The named input is decided, with the outcome the replica computes.
- [ ] The backlog drains: `aledgent_oldest_undecided_row_age_ms` falls under 60 s. Record the drain time.
- [ ] `NFR-4`: no acknowledged operation is missing. Nothing is still `Received` after the drain.
- [ ] Trial balance zero; reconciliation on demand, axes 1 and 2.
- [ ] A regression test naming the input's class is merged with the fix (the F18 pattern:
  `req_rt_1_a_tick_stays_within_the_heap_however_many_operations_are_open`).

## Rollback

If the new image misbehaves, redeploy the previous digest. The core halts again on the same input, which
is safe: every input waits `Received`. Never edit the received-log or the archive to remove the input.

## Exercised

On every change: `ops/tests/alerts.rs` checks that `AledgentCoreHalted` and
`AledgentReceivedOverDecisionBound` page the test route. The stress workflow's fault-injection scenario d
kills and restarts the core host under load. A halt on a real input is not exercised automatically yet; it
is exercised by hand in a staging deployment.
