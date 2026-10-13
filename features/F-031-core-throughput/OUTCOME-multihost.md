<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Outcome: the cross-host relay (experiment of 2026-10-04)

**Verdict: adopt the relay's shape** for `NFR-21` (a), ruled by the owner on 2026-10-05
(`mh-relay-shape`) pending the fleet run. The run passed correctness. The shape is a coordinator host
for the principals Agent and the assembler, Noise with per-host static keys, and the transport key
held by the host process. The code stays merged behind the `net` feature and the core host's `fleet`
command. It scales the book across machines with every check holding. Three 16-vCPU hosts decide
1 373 transfers a second in a closed loop and 1 426 a second under 1 500 offered, so **1 500 a
second sustained is not reached**: 5 % short. The proposed latency bounds do not hold, on one host
or three. 1 500 a second is a target, not a gate (MVP freeze of 2026-10-04); nothing further was
optimised.

## What was built

The book's shard Agents are spread over several hosts (`NFR-21` (a)). Host 0 holds the principals
Agent, the router and the anchor's assembler. Every exactly-once stream travels between hosts over
Noise IK sessions with static keys pinned per host: credits, trims, the principals feed, audit
records and shares, orders and outcomes. On the fleet the sessions run over the cloud provider's private
network. Each stream is exactly once by its own numbering, and a lost session is resumed from what
its receiver says it holds. Each shard is driven on its own. A host that restarts rebuilds its relay
from its Agents. The coordinator anchors at the greatest consistent cut over every host's audit
records (amendment 0015). The shard Agents' interfaces did not change.

## Numbers (`notes/MEASUREMENTS-F031.md` §12; runs 37257923201, 37259648332)

| Hosts (16 vCPU) | Shard Agents | Closed loop, decided/s | Paced, decided during the load | Trial balance, exactly once, live anchors |
|---:|---:|---:|---|---|
| 1 | 15 | **693** | — | hold |
| 2 | 30 | **1 072** | 916/s at 964 offered | hold |
| 3 | 45 | **1 373** | 1 242/s at 1 300 offered; **1 426/s at 1 500 offered** | hold |

- **The burst (owner ruling G-03) on 3 hosts: absorbed and drained within the 30 s bound.** The
  backlog peaked at 460 commands (under a second of load) in the burst minute (50 × the reference minute), and was
  down to 3 within 10 s of the minute's end. About 55 s later a shard Agent hit the Local Agent's
  image limit and halted the run, so its final checks did not run.
- **Proposed bounds, every run:** ack p99 3.0–7.7 s (`NFR-16`: 1 s), credit lag p99 2.5–8.5 s
  (`NFR-19`: 2 s), revocation lag 1.5–2.7 s (`NFR-20`: 1 s).
- **Correctness natively** (`crates/shards/tests/multihost.rs`): exactly once, the trial balance,
  zero divergence and live anchors, across 3 hosts in threads, with sessions dropped every 40 ms, and
  as 3 processes with one killed and restarted mid-load.

## Where the scaling goes

Not the network: 2–3 MB per phase, and 0.6 ms round trips. **Rounds fragment instead.** Each shard
takes a round as soon as a credit or a trim arrives. On 3 hosts rounds averaged 4–5 commands, against
22 on one host, and each round pays the runtime's fixed cost. Crossing transfers rise from 83 % to
88 % of all transfers, each costing two postings. The shards are 76–79 % busy.

## What would reach 1 500 a second

- **Fill the rounds:** a round wait in the per-shard driver, as F-034 needed. No extra machines.
- **Or a fourth 16-vCPU machine** (about +250 a second, extrapolated). That is **64 vCPU in the fleet, a
  project limit of at least 90 vCPU** with the about 26 used outside the runner autoscaler, against 80 today.

Either way, a run longer than about 30 s at that rate needs external state (`DEP-11`). A Local
Agent's image fails near 4.4 MB, about 1 000 transfers a shard when nearly every transfer crosses
shards. Neither lever brings the latency bounds within reach while the PVM is on the critical path.

## Adopted (owner ruling `mh-relay-shape`, 2026-10-05)

The relay's shape is adopted for `NFR-21` (a), pending the fleet run's correctness. Runs 37257923201
and 37259648332 passed it on 1, 2 and 3 16-vCPU hosts over the private network. Every phase that
completed held the trial balance with credits in transit, exactly once (read from the Agents' heads)
and a consistent cut at every live anchor, one a second. The one halt (the burst run) was the Local
Agent's image limit, not a divergence or a lost credit. The measured numbers are the ones above.

Under the experiment rule, the adoption goes to the coordinator for consolidation into one amendment
and one DEC. Nothing here edits `SPEC.md` or `PLAN.md`.

## The owner's rulings of 2026-10-05

- **`mh-q01`: no more scaling on CI.** Stress testing moves to the deployment's own infrastructure, before
  its first production window, with the same pass criteria (1 500 a second, `NFR-16`, `NFR-19`,
  `NFR-20`, the burst gate) and the fleet bench run on the deployment's machines instead of the
  self-hosted fleet. It is a release step, not a CI job.
- **`mh-q02`: the bounds under native decide, measured** (run 37267168415, same machines, phases and
  checks; `notes/MEASUREMENTS-F031.md` §12). On 3 hosts at 1 500 a second offered, 1 490 a second
  were answered, with the trial balance and exactly once held.
  - `NFR-16` holds (ack p99 0.25 s).
  - `NFR-20` holds (revocation lag 52 ms).
  - `NFR-19` does not hold (credit lag p99 5.2 s): a credit waits for the replay's confirmation.
  - The burst was absorbed and drained, then stalled at the same book size as the verified run.

  Native decide is still not in the MVP (F-034: "adapt, not adopt").
- **`mh-relay-shape`: adopted** (above).

**The many-small-nodes run.** The small pool, now 2-vCPU machines, gave 14 of 16 machines (run
37265315722). The other two failed at the cloud provider with `Primary IP limit exceeded`: a public IPv4 quota,
not the vCPU limit. The run was cancelled before measuring, and no more scaling runs were made
(`mh-q01`). A 16-node group needs the project's primary-IP limit raised by at least 2, or the pool's
servers created without a public IPv4.

The small pool was not used (its machine type is deprecated in its region; changing it is the maintainers' call).
Defaults taken: the experiment's decision record (kept with the maintainers).
