<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-033 — notes: the runtime built and measured

`DEC-23`, amendment 0014. Tasks `T-033-02` to `T-033-05`.

## What was built

- **The engine, on VOS** (fork branch `aledgent/composite-runtime`, rebased onto `perf/agent-stack` `c54fc1ff`):
  - `b0ff6513` (first `ef03ab88`, on `d6a13f18`), composite invocations behind the feature `runtime-composite`:
    - `vos/src/agent/composite.rs`: the wire format and the reply.
    - `vos/src/agent/wire/composite_engine.rs`: the steps.
    - `standard.rs`: stream heads in the control image (section `SST1`, absent while empty), a lane-only step commit, and the composite's retained reply.
  - `c7bb037a` (first `bf229847`): `apply_standard_runtime_input_committed_unless_direct`.
  - `188bf162`: a test-only fix, so that VOS's lib tests build (`c54fc1ff` fixed the other one the stack carried).
  - The hosted runs below used the first branch (`bf229847`, on `d6a13f18`). `c54fc1ff` adds lockfiles and a test fix to `d6a13f18`, so the code those runs measured is unchanged. The fleet run used `188bf162`.
  - VOS's standard runtime does not enable the feature, and its images encode as before.
- **The program,** `agents/aledgent-runtime`: VOS's standard entry, with its rules set first (128 steps, 16 for an atomic composite, and every step ordered or numbered) and `direct-public-io` as an option.
- **The core host:**
  - `F033_COMPOSITE=isolated|atomic` sends a batch of inputs as one composite, one input per step, all under the core's one order key. The replies are mapped back to the actor's own batch reply.
  - `BatchLimits::max_packs` keeps one caller blob free for the composite.
  - All of this sits behind the core host's `composite` feature, so the host and its production image stay on `perf/agent-stack` (`c54fc1ff`). The bench turns the feature on when the VOS it builds against has the engine.
- **The bench:**
  - `compare` takes `!aledgent` (our runtime, built against the variant's VOS checkout) and `~isolated` (composite batches).
  - `conformance` runs the suite through the core Agent in VOS under each variant's runtime.

## Tests

`vos/tests/agent_runtime_composite.rs` runs the composite runtime's PVM guest over assembled actors. It covers:
- the plain path;
- an atomic composite across three actors in one transition, with its exact retry and one acknowledgement;
- an atomic failure that commits nothing and is retained;
- isolated steps deferred behind a failure by their order key;
- deferral for gas, in isolated and atomic composites;
- streams: a duplicate, a gap that waits, an atomic gap that moves no head;
- an actor that is not installed;
- 128 steps with the reply kept within 8 KiB;
- the codec.

All 9 tests pass. VOS's own suites (run 37154416970, at the pre-rebase equivalent of `188bf162`) pass, except for three failures that do not touch this change:
- two tests need the scripted recovery fixture, which the bench does not build;
- `invocation_gas_is_explicitly_bounded` fails on the stack's per-Agent limits. That one is for the VOS perf work.

## Conformance through the Agent in VOS

Hosted 2-vCPU runners, recompiler, VOS `bf229847`, profile `vos-native native-curve`, core built with `conformance-ports`:

| Runtime | Changes per invocation | Result | Run |
|---|---:|---|---|
| VOS's standard | 1 | 193 pass, 0 fail, 11 pending by owner, 72 not runnable on this target | 37153656891 |
| aledgent's | 1 | the same | 37153658589 |
| aledgent's, batches as composites | 8 | the same; 74 composite invocations of 2 to 8 inputs | 37153660389 |

## Measured against the standard runtime

One hosted runner for every variant: AMD EPYC 9V45, 2 vCPU, recompiler, 384 reservations per run. Two runs, each on a single runner: 37153652299, and 37154968020 after the pack fix (`BatchLimits::max_packs`).

Inputs per second at the round's p50; gas in millions per invocation, p50. "Runtime" is the invocation's gas less the actor's. `std` is VOS's runtime, `ald` ours, `aldd` ours with `direct-public-io`, `aldc` ours with the batches as isolated composites.

**Run 37153652299** (EPYC 9V45). Actor gas 8 000 M per invocation; batch 1 also under VOS's standard 1 000 M bounds.

| Variant | Batch | Invoke gas | Actor gas | Runtime gas | Inputs/s |
|---|---:|---:|---:|---:|---:|
| std | 1 (1 000 M) | 219.26 | 108.97 | 110.29 | 18.50 |
| ald | 1 (1 000 M) | 219.26 | 108.97 | 110.29 | 18.54 |
| aldd | 1 (1 000 M) | 218.01 | 108.97 | 109.04 | 19.16 |
| std | 8 | 1 008.48 | 870.17 | 138.32 | 57.02 |
| ald | 8 | 1 008.43 | 870.17 | 138.26 | 54.54 |
| aldd | 8 | 1 006.77 | 870.17 | 136.61 | 52.57 |
| aldc | 8 | 1 168.35 | 880.71 | 287.64 | 26.35 |
| std | 32 | 1 474.40 | 1 314.92 | 159.48 | 67.04 |
| ald | 32 | 1 474.36 | 1 314.92 | 159.45 | 66.00 |
| aldd | 32 | 1 472.10 | 1 314.92 | 157.18 | 60.92 |

**Run 37154968020** (EPYC 7763, after the pack fix), at 8 000 M:

| Variant | Batch | Runtime gas | Inputs/s |
|---|---:|---:|---:|
| std | 1 / 8 / 32 | 110.52 / 138.32 / 159.48 | 12.82 / 30.73 / 34.60 |
| ald | 1 / 8 / 32 | 110.51 / 138.26 / 159.45 | 12.82 / 31.72 / 35.29 |
| aldd | 1 / 8 / 32 | 109.29 / 136.61 / 157.18 | 12.85 / 31.71 / 34.94 |
| aldc | 1 / 8 / 32 | 110.51 / 287.64 / 544.71 | 12.85 / 17.75 / 17.62 |

### The shard Agents on the fleet

**Run 37160680098**, one self-hosted x86-64 runner: AMD EPYC-Genoa, 16 vCPU, one shard Agent per core. This is the same machine type as run 37154004755. The workload is `aledgent-core-host shards` with 2 000 transfers among 256 accounts, rounds of 64, recompiler, VOS `188bf162`, every profile `vos-native native-curve` with lookup. Figures are decided transfers per second.

| Shard Agents | std | ald | aldd (`direct-public-io`) | std at `c54fc1ff` (run 37154004755) | Largest Agent state |
|---:|---:|---:|---:|---:|---:|
| 1 | 108 | 108 | **120** | 105 | 2.92 MB |
| 2 | 182 | 182 | **200** | 179 | 2.08 MB |
| 4 | 325 | 317 | **339** | 296 | 1.34 MB |
| 8 | 654 | 639 | **669** | 642 | 0.74 MB |

In every profile and at every count: the trial balance holds with credits in transit; every credit is applied exactly once, as read back from the Agents; and the actor gas per round is identical (3 088, 3 400, 2 863 and 2 324 M).

- **Our runtime matches the standard one on the shards** (within ±2.5 %, the runner's noise). The `std` column built at `188bf162` matches the `c54fc1ff` baseline, because the composite feature is off in it.
- **`direct-public-io` pays where the image is large:**
  - +11 % at one shard (2.9 MB image: 108 → 120 decided/s);
  - +10 % at two shards;
  - +4 % at four;
  - +2 % at eight (0.74 MB image).

  It removes the two hash passes over the image that only a proof would read (`r-04`).

How to read it:

- **The plain path costs the same.** The runtime's gas per reservation is 110.29 M under both runtimes, a difference of 1.3 k. Inputs per second are equal within the runner's noise. Our runtime adds no fixed gas per invocation.
- **`direct-public-io` saves 1.25 M per invocation on the core,** whose state is 9 KB. It saves more on an Agent whose image is large: two hash passes over the image per invocation. It is off by default (`r-04`).
- **A composite step costs about 21 M of runtime gas** with the 4.34 MB core program. That is the price of starting a machine per step, against about 110 M for an invocation of its own. With near-empty actors, a step costs about 1.3 M (VOS test `measure_runtime_gas_per_step`).
- **The actor's own batch is faster than a composite of the same inputs** (56 against 26 inputs/s at a batch of 8). Composites buy isolation per step, ordering and exactly-once below the actor; they do not buy throughput (`r-02`).

## How it plugs into the shard Agents (#142)

The shard Agents' `aledgent-shard` runs under this runtime unchanged, because the plain path is the standard one. The core host's `--runtime` takes `aledgent_runtime.pvm`.

The next step (`T-033-10`) moves exactly-once into the runtime:
- the relay sends each source's credits and the principals feed as stream steps;
- once a step can carry a range of a stream (`T-033-06`), one source's batch of credits becomes one step;
- the actor's own sequence checks then become a second line of defence.

## Reproduce

    gh workflow run x86-bench.yml --ref f-033-aledgent-runtime -f mode=compare -f vos_ref=<VOS commit> \
      -f vos_picks=none -f batches="1 8 32" -f batch_gas=8000000000 \
      -f "variants=std=<ref>+vos-native,native-curve@lookup ald=<ref>+vos-native,native-curve@lookup!aledgent aldc=<ref>+vos-native,native-curve@lookup!aledgent~isolated"
    gh workflow run x86-bench.yml --ref f-033-aledgent-runtime -f mode=conformance -f vos_ref=<ref> \
      -f vos_picks=none -f "variants=ald=<ref>+vos-native,native-curve@lookup!aledgent"
