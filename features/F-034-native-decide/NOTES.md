<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-034 — native decide, VOS verify: measurement notes

These are observations of an experiment (the owner rule of 2026-10-04). They are not a
requirement. The design note, the draft amendment and the outcome page are on the branch
`f-034-native-decide-design`, not merged, with the defaults and the open questions.

## What was measured

`aledgent-core-host shards --runs verified,native,native@1500,native@v90` (`stress.yml`,
`core=shards-recompiler`, `runner=zombienet`) runs on one fleet machine: AMD EPYC-Genoa, 16 vCPU.
The principals Agent is pinned to core 0 and shard Agent *i* to core *i* + 1. The host's native
shards, its replay workers and its relay are not pinned.

The VOS was `c54fc1ff`. Profile `jam=` is the standard runtime with no feature: the curve
precompiles run as runtime guest code, so **no host precompile** (the ruling of 2026-10-04). The
workload is the one of `MEASUREMENTS-F031.md` §7:

- 256 accounts and a fee pool;
- 8 agent principals, a revocation halfway through;
- rounds of up to 64 commands;
- 750 transfers per shard in the closed loops.

| Run | What it is |
|---|---|
| `verified` | Today's path: each round decided by the Agent (PVM), the native replica checking it. Closed loop |
| `native` | Native decide, closed loop. Rounds committed natively and replayed through the Agent, two rounds per replay invocation, a round waiting up to 200 ms to fill to 64 |
| `native@1500` | Native decide at an offered 1 500 transfers a second for 20 s |
| `native@v90` | Native decide at 90 % of the same count's `verified` rate, for 20 s |

How to read the columns:

- **Decided/s** is the rate at which commands were answered.
- **Confirmed/s** is the commands the replay confirmed per second, from the load's start to the
  last confirmation.
- **Ack** is the time from submission to the answer.
- **Replay lag** is the time from a round's native commit to its confirmation.
- **The replay keeps up** when everything is confirmed within 5 s of the last decision and the
  replay lag's p99 is under 5 s.

## Run 37221732022, profile `jam=` (pure JAM)

| Shards | Verified decided/s | Native decided/s (closed loop) | Native confirmed/s | Verified ack p50 / p99 | Native ack p50 / p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 102 | 936 | 112 | 610 / 660 ms | 54 / 233 ms |
| 2 | 155 | 1 884 | 163 | 808 / 1 733 ms | 75 / 285 ms |
| 4 | 248 | 3 629 | 259 | 940 / 2 906 ms | 79 / 342 ms |
| 8 | 427 | 6 876 | 423 | 1 013 / 4 520 ms | 84 / 375 ms |
| 15 | 653 | 4 939 | 624 | 1 255 / 5 076 ms | 239 / 916 ms |

At an offered rate:

| Shards | `native@v90`: offered → decided/s | Replay lag p50 / p99 | Credit lag p99 | Keeps up | `native@1500`: decided/s | Replay lag p99 | Keeps up |
|---:|---:|---:|---:|---|---:|---:|---|
| 1 | 91 → 90 | 0.74 / 2.74 s | | yes | 1 436 | 34.5 s | no |
| 2 | 139 → 138 | 0.64 / 2.41 s | 2.40 s | yes | 1 481 | 28.5 s | no |
| 4 | 223 → 221 | 0.42 / 1.87 s | 2.05 s | yes | 1 481 | 23.1 s | no |
| 8 | 384 → 380 | 0.38 / 2.50 s | 2.43 s | yes | 1 483 | 17.8 s | no |
| 15 | 587 → 582 | 0.25 / 1.16 s | 1.40 s | yes | 1 480 | 16.2 s | no |

The revocation lag (`NFR-20`) was 4–85 ms under native decide, against 0.57–1.24 s verified: a
revocation fences a shard as soon as its native round applies the feed.

`native@1500`'s confirmed rate is not a clean figure, here or in run 37223335529. From four
shards up, replays failed (a runtime panic, `ResultCapacity`) and the halted shards stopped
confirming, so those rates are lower bounds. The table at the end says why.

Run 37223335529, the same job on another fleet machine (credits capped at 64 a round), was
slower throughout:

| Shards | Verified decided/s | Native decided/s | Native confirmed/s | `native@v90` keeps up | `native@1500` decided/s |
|---:|---:|---:|---:|---|---:|
| 1 | 67 | 738 | 77 | yes (lag p99 1.9 s) | 1 256 |
| 2 | 106 | 1 502 | 114 | yes (1.5 s) | 1 478 |
| 4 | 171 | 2 504 | 173 | yes (1.2 s) | 1 474 |
| 8 | 307 | 5 581 | 291 | yes (1.7 s) | 1 479 |
| 15 | 521 | 4 074 | 431 | yes (1.6 s) | 1 480 |

## What the figures say

- **Native decide answers 1 500 transfers a second on this machine from two shards on**
  (1 481–1 483 decided/s offered 1 500). Its answer takes tens of milliseconds, against 0.6–1.3 s
  at p50 with the PVM on the critical path. In a closed loop it decides 936–6 876 a second, 9 to
  16 times the verified rate. At 15 shards the host's own threads compete with 15 pinned Agents
  for 16 vCPU, and the native rate falls to 4 939.
- **The replay confirms at the verified rate, not faster.** In the closed loop, confirmed/s is
  within 5 % of verified decided/s at every count (112/102, 163/155, 259/248, 423/427,
  624/653). A round costs the PVM the same whether it decides it or replays it.
- **The replay keeps up at 90 % of the PVM's rate**, with the replay lag at 1.2–2.7 s p99 and the
  credit lag at 1.4–2.4 s p99. That is around `NFR-19`'s proposed 2 s, because a credit now also
  waits for its debit's confirmation. **It does not keep up at 1 500**: the lag grows for as long
  as the load lasts (16–35 s p99 after 20 s).
- **Native rounds must be full.** In the first run (37217934926) a native round formed as soon as
  anything was queued. At 90 % of the verified rate those rounds carried one or two commands, and
  the replay fell behind: 34 confirmed/s against 64 offered on one shard. A round costs a fixed
  part plus a part per command, and small rounds pay the fixed part per command. A 200 ms round
  wait (`Cluster::round_wait`) fixed it, at the price of 120–240 ms of answer latency at p50.
- **Variance between fleet machines is large.** The same `verified` workload gave 72 → 505 (run
  37217934926), 103 → 740 (37219855298) and 102 → 653 (above) on 1 → 15 shards. Compare only
  within a job.

## Against 1 500 transfers a second

The PVM's sustained rate is the bound in both models. Measured, pure JAM, 16 vCPU:

- 653–740 transfers a second on 15 shard Agents (one per vCPU, the principals Agent on the
  sixteenth);
- 44–49 a shard at 15 shards. Most transfers cross shards there (84 %), so each costs nearly two
  postings.

One shard alone decides 102–103, so with a core each and nearly every transfer crossing shards,
the per-shard rate tends to about 102 ÷ 1.93 ≈ 53. The measured 44–49 at 15 shards includes the
host's own threads on the same cores.

**The cores for 1 500 transfers a second sustained, with the PVM confirming every round:**

| Model | Shards (one PVM core each) | vCPU in total | Fleet machines (16 vCPU) |
|---|---:|---:|---:|
| PVM on the critical path | 1 500 ÷ 44–49 ≈ 31–35 | ≈ 33–37 (the shards, the principals Agent, the host's replicas and relay) | 3 |
| Native decide, replay keeping up | the same 31–35 replaying | ≈ 35–39 (+ about 2 cores of native decision: 6 876 a second used 8 shards' worth of native threads) | 3 |
| For reference, `native-curve` (a host precompile, excluded by the ruling; 956–967 on 15 shards) | ≈ 23–24 | ≈ 26 | 2 |

**This machine does not reach 1 500 sustained in either model.** It reaches 653–740 with the PVM
confirming every round. Native decide answers 1 500 here, but its replay falls behind at once,
by about 850 transfers a second. Native decide does not lower the count of PVM cores: every
round still runs once in the PVM.

**The smallest real run that would show 1 500 sustained:**

- *Option 1:* 32–36 shard Agents, one per core, with the PVM confirming every round, on 48 vCPU.
  Either one 48- to 64-vCPU machine running today's single-process harness (no code change), or
  three fleet machines with the relay carried between hosts (not built: the harness is one
  process).
- *Option 2:* the same shard count under native decide, `native@1500`, which also shows the
  replay keeping up at that rate.

Either is a single 30-minute job.

## The prototype's failures, and what fixed them

| Run | Seen | Cause | Fix |
|---|---|---|---|
| 37217934926 | Replays at `native@1500` halted: runtime panic, `ResultCapacity` | Replays of eight full rounds held every round's writes in one overlay: out of the Agent's 16 MiB heap | The replay commits round by round; two rounds per replay by default |
| 37217934926 | The replay fell behind below the verified rate | Native rounds of one or two commands | `round_wait` (200 ms) |
| 37219855298, 37221732022 | Halts at `native@1500` from 4 shards up | Credits released in bursts at confirmation: a round took hundreds | At most 64 credits a round (`max_credits`) |
| 37223335529 | Halts at `native@1500` from 4 shards up, still | See below | Not fixed here: external state on Shared Agents (`DEP-11`) |

**The last row.** Only `native@1500` runs halted, and only from four shards up.

- Those runs carry 30 000 transfers, 2.7 to 40 times the closed loops. Their shard Agents' images
  reached 4.3–4.6 MB, against at most 2.3 MB in every other run.
- A Local Agent carries its whole image through every invocation, in an actor heap of 16 MiB.
  The likeliest cause is that image together with a full round's working set. Shards that see
  credits from three or more peers carry the largest rounds.
- `NOTES-shards.md` already names this limit, and the PVM on the critical path would meet it at
  the same book size. No `verified` run of that length was made to show it.

Every halt was a *failed* replay (the Agent refused, or its runtime panicked), never a
*divergence*. In every run, every round that was confirmed had the native digest.

## Reproduce

    git push origin <branch>:stress-runs/<name>   # with e2e/stress/matrix.json:
    [{ "name": "f034-jam", "core": "shards-recompiler", "runner": "zombienet", "shard_profiles": "jam=",
       "core_features": "native-decide", "shard_counts": "1,2,4,8,15", "shard_batch": "64",
       "shard_args": "--runs verified,native,native@1500,native@v90 --ops-per-shard 750 --paced-secs 20 --drain-cap-secs 60 --replay-batch 2 --round-wait-ms 200" }]

On the laptop: `cargo test --release -p shards --features native-decide --test agents_throughput
-- --ignored --nocapture`. The replay there is the guest's code run natively, so it shows only
the native decision rate: 1 590, 2 349, 4 002 and 5 228 decided/s on 1, 2, 4 and 8 shards, under
a load average of 19–30.
