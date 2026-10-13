<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 measurements: core throughput

Observations, not requirements: `NFR-18` has no target (ruled 2026-10-03); the stress suite
(`F-032`) and this file report what was measured.

**The reference since 2026-10-04 is §10**, and since 2026-10-05 §11 (v2 roots, the default by `DEC-25`): the JAM-compatible default, with Gray Paper host calls
only. §8.4 and §9 include VOS's native curve calls, which are outside JAM and no longer a default.

## 1. Lever 1: batched invocations (`T-031-05`, 2026-10-03)

Run [37099849992](https://github.com/bloque-app/bloque-ledgent/actions/runs/37099849992) of the manual
x86-64 bench workflow, mode `reservation-timing`, on `f-031-batching` at `7b2608b`.

| | |
|---|---|
| Host | AMD EPYC Genoa, 16 CPUs, 32 GB (the `zombienet` fleet) |
| VOS | aledgent's VOS fork at `3140c3d` (recompiled Refine execution), `de0f12f` picked (sequenced identities); both program limits raised to 4 096 KiB in the checkout |
| Actor | `agents/core`, every command group, layout 2 and 3 linked: package 2 638 337 B, program 2 636 770 B |
| Engine | The recompiler |
| Workload | `aledgent-core-host measure --n 192 --warmup 3 --batch B`: reservations on one account, B staged (`Driver::stage`) and flushed (`Driver::flush`) per round, in as few invocations as `BatchLimits` allow |
| Second set | The same with `MAX_EXECUTION_GAS` raised from 1 000 M to 8 000 M in the checkout (core host and runtime rebuilt), every other bound VOS's own |

A round is its reservations on the replica, their staging (witnesses, packs) and the flush; its time
is wall time. Gas is per invocation. Inputs per second are the round's inputs over the round's time.

| VOS actor gas | Batch | Rounds | Invocations | Round p50 µs | Round p99 µs | Invoke gas p50 | Actor gas p50 | Inputs/s at round p50 | Inputs/s over the run |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 000 M | 1 | 192 | 192 | 292 237 | 330 293 | 2 062 447 859 | 295 463 843 | 3.42 | 3.38 |
| 1 000 M | 8 | 24 | 72 | 1 127 390 | 1 193 270 | 2 666 842 534 | 882 286 560 | 7.10 | 7.05 |
| 1 000 M | 32 | 6 | 66 | 4 017 179 | 4 056 235 | 2 672 200 255 | 884 950 030 | 7.96 | 7.99 |
| 8 000 M | 1 | 192 | 192 | 294 802 | 323 918 | 2 062 443 168 | 295 463 843 | 3.39 | 3.37 |
| 8 000 M | 8 | 24 | 24 | 530 595 | 571 703 | 4 194 568 366 | 2 360 194 696 | 15.08 | 15.01 |
| 8 000 M | 32 | 6 | 12 | 1 624 814 | 1 795 458 | 5 417 593 799 | 3 535 713 757 | 19.69 | 19.43 |

- **The fixed part.** One reservation's invocation is 2 062 M of gas, of which 295 M is the actor's;
  the rest is the runtime, mostly reloading the 2.64 MB program (this run linked layout 3 into the
  guest; since #134 the production guest does not, and is smaller, so this fixed part is an upper
  bound). A batch shares it: at 8 000 M of
  actor gas a 20-input invocation costs about 7.7 G, 0.38 G per reservation.
- **Stock VOS** holds an invocation to 1 000 M of actor gas: three reservations. Batching gives 2.3
  times (3.4 to 8.0 inputs/s).
- **Gas raised**, the packs bind next (four caller blobs of at most 46 KiB; 178 KB of blobs for 20
  reservations), and batching gives 4.4 times at 8 and 5.8 times at 32 (19.7 inputs/s).
- **Against the stress suite** (#127, one input carrying K commands, every per-invocation bound
  raised): 3.6, 13.9 and 13.1 inputs/s at 1, 8 and 32.
- The interpreter, recompiler and lockstep runs of the same job: 227 executions identical on both
  engines; the recompiler's driver round 295 ms p50 and 327 ms p99, the interpreter's 2.93 s p50.

Reproduce: `gh workflow run x86-bench.yml --ref <branch> -f mode=reservation-timing -f batches="1 8 32"
-f vos_ref=3140c3d57169316675c4231f742002d095571b4c` (`batch_gas` defaults to 8 000 M; empty skips the
second set).

## 2. Lever 2: the book in committed maps (`T-031-08`)

Natively (`crates/core-binding/tests/mapped.rs`, 500 transfers): the transition 1 841 → 1 085 µs
p50 (−41 %), the bytes an invocation carries 6 960 → 859 B (−88 %).

On VOS (run [37098709186](https://github.com/bloque-app/bloque-ledgent/actions/runs/37098709186), mode
`mapped-timing`, 100 reservations, the same raised bounds for both layouts):

| Layout | Round p50 | p99 | Invoke gas p50 | Actor gas p50 | Agent state |
|---|---:|---:|---:|---:|---:|
| 2 (witnesses) | 329 ms | 447 ms | 2 064 M | 294.9 M | 11.9 KB |
| 3 (committed maps) | 361 ms | 414 ms | 2 215 M | 219.8 M (−25 %) | 359 → 671 KB |

The actor's work falls by a quarter; on a Local Agent the rows travel as the image, so the
invocation costs 7 % more and grows with the book. External state on a Shared Agent removes that
term (`DEP-1`, `DEP-11`). Details: `features/F-031-core-throughput/NOTES-committed-maps.md`.

## 3. Lever 3: shards of partitions (`T-031-11`)

The prototype (`crates/shards`), natively, release, 4 000 transfers among 256 accounts, batches of
32, on a 12-core laptop under load: 874 ops/s on one shard, 1 637 on two, 2 225 on four, 2 554 on
eight (83 % of transfers between shards, two postings each). Details:
`features/F-031-core-throughput/NOTES-shards.md`.

## 4. The stress suite's findings, fixed (`T-031-15` to `T-031-18`)

GitHub-hosted 2-vCPU runners, against the stress suite's runs:

| Finding | Before | After | Run |
|---|---|---|---|
| F5: notifications without a recipient | 124 490 failed attempts in 10 min at 10 × | 0 (22 132 parked, listed) | [37107506920](https://github.com/bloque-app/bloque-ledgent/actions/runs/37107506920) |
| F2: idle ticks | one a second | 10 in 10 min (60 s heartbeat) | same |
| F13 + lever 1: in-process ceiling | 40 × collapsed to ≈ 3 decisions/s | 40 ×: ≈ 80 decisions/s, not collapsed; 80 × collapses (F3) | same |
| Lever 1 latency on the Agent (T-031-18) | ack at ¼ × 15 s / 46 s (p50 / p99) | 2.3 s / 4.6 s | [37121891951](https://github.com/bloque-app/bloque-ledgent/actions/runs/37121891951) |
| F3: a read storm, in-process (local, 32 readers) | no decision window completed in 3 min | ≈ 170 decisions/s under ≈ 9 000 reads/s (190 alone) | #136 |
| F4: API keys (local) | the core read on every request | cached ≤ 500 ms (`NFR-20`); a removed key stops in 512 ms | #136 |
| F6: memory per command (local, 60 rounds) | 29 422 B in-process, 27 590 B core host | 3 380 B, 2 815 B with the leaves on disk | #136 |
| F6, on the runners: leaves on disk by default | in-process 10 × held, ack p99 6 ms; soak 14 MB → 3.6 GB | 10 × not held (79 % of the offered rate decided, ack p99 2.2 s); soak 14 MB → 965 MB, ack p99 65 ms (`NFR-1` fails) | [37123728717](https://github.com/bloque-app/bloque-ledgent/actions/runs/37123728717), [37123730023](https://github.com/bloque-app/bloque-ledgent/actions/runs/37123730023), [37109753823](https://github.com/bloque-app/bloque-ledgent/actions/runs/37109753823) |

The disk default regressed `NFR-1` on a 2-vCPU runner (redb's write amplification beside
PostgreSQL's fsyncs): #139 makes memory the default again and keeps the disk stores opt-in. Memory
growth in the in-process core stays open (a pruned node table in the kernel's `MemStore` halves the
leaves' share, 32 → 15 KB a command, and is the next step).

**Agent batches did not commit from 47155ab until #143.** `Envelope::Mapped`, gated on the
`mapped` feature, sat before `Batch`; postcard numbers variants by index, the host always has
`mapped` and the production guest does not, so the guest read every batch of two or more inputs as a
malformed message. The core then rebuilt and re-sent each input alone (`crate::core`). §1's figures
(7b2608b) predate the bug and stand. The Agent rows of stress runs 37107506920, 37107740608,
37109753823, 37115136709, 37115138309, 37119831658 and 37121891951 (including §4's "Lever 1
latency on the Agent") measured the unbatched Agent plus one refused invocation per batch: they
are pessimistic and are to be re-run.

## 5. Where the core Agent stands

On stock VOS, an invocation carries three reservations (1 000 M of actor gas) and pays about 1.5 G
of fixed runtime gas (the program hashed and repacked on every invocation): **3.4 inputs/s at one
per invocation, 8.0 batched, 19.7 with the actor gas raised to 8 G**, on a 16-core EPYC; below a
quarter of the reference hour on a 2-vCPU runner. What moves it next is upstream: the
prepared-actor cache (`proposals/vos/0004`), raised invocation bounds, and the curve arithmetic
native on the Agent path (`F-025`). Sharding multiplies whatever one writer reaches.

## 6. The store commit, batched and cut (`T-031-19`, 2026-10-03)

`notes/MEASUREMENTS-F002.md` §4 put two thirds of a native posting in `FileStore`'s commit (about
1.1 ms of 1.7 ms), and SMT hashing at only 0.12 ms of that. This splits the commit by phase
(`FileStore::last_profile`) and cuts it.

`spikes/native/src/bin/commit.rs`, release, on an Apple M-series laptop with 12 cores (load
average 3 to 5): `Book::apply` over `FileStore` and `FileVault`, transfers among 1 000 funded
customers, 300 batches after 30 of warm-up. `unit` commits each single-posting unit, as the core
does today; `batch` stages 32 units and commits them once (`Book::begin_batch`, `Book::end_batch`).
`file-mem` is redb's in-memory backend (no fsync), `file-disk` the SSD with every commit fsynced.
Times are µs per posting; the store's phases are means.

| Store | Mode | Batch | | Apply p50 | Store commit | Rules | Leaves | Paths | Log | Commit (fsync) | Node reads | Node writes |
|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| file-mem | unit | 1 | before | 2 466 | 1 958 | 4 | 13 | 1 112 | 9 | 820 | 512 | 516 |
| file-mem | unit | 1 | after | 1 165 | **660** | 6 | 12 | 367 | 10 | 266 | 60 | 134 |
| file-mem | batch | 32 | before | 2 041 | 2 017 | 4 | 14 | 1 282 | 6 | 711 | 508 | 512 |
| file-mem | batch | 32 | after | **899** | **382** | 3 | 10 | 259 | 2 | 107 | 60 | 114 |
| file-disk | unit | 1 | before | 24 673 | 20 129 | 11 | 23 | 1 528 | 17 | 18 551 | 512 | 516 |
| file-disk | unit | 1 | after | 14 702 | 9 530 | 8 | 19 | 525 | 14 | 8 963 | 60 | 134 |
| file-disk | batch | 32 | before | 5 914 | 5 417 | 5 | 13 | 1 133 | 2 | 4 265 | 508 | 512 |
| file-disk | batch | 32 | after | **1 792** | **1 313** | 3 | 10 | 267 | 2 | 1 031 | 60 | 114 |

**Where the commit went.** Not encoding (the leaf records 12 µs, the log record 9 µs), not the
write rules (4 µs). A transfer writes four leaves (two accounts, the transfer, its external id);
each leaf read its 128 siblings and wrote its 129 path nodes, one redb B-tree operation each:
**512 reads and 516 inserts per posting**, 57 % of the commit (the 0.12 ms of hashing is inside
that). The transaction's commit was the other 42 %: in memory, redb writing out the pages those
inserts dirtied; on disk, that and the fsync.

**What changed.**

1. **Siblings known to be empty are not read** (`smt::set_leaf`, `NodeTable::neighbours`): a table
   that can say how many bits a key shares with its nearest other leaf saves every read below
   that depth. In a tree of `n` random keys a leaf reads about `log2(n)` siblings, not 128.
2. **No node below depth 32 is stored** (`FILE_NODES_DEPTH`, the cut `MemStore` took in
   `T-031-14`): a deeper node is computed from the leaves under it on the rare read. A leaf
   writes 33 nodes, not 129.
3. **Each node is written once per commit**, however many of the commit's leaves share it: the
   paths are kept in memory and flushed at the end. Leaf tables and the node table are opened
   once per commit, not once per leaf.
4. **A batch is one commit** (`Book::begin_batch` / `end_batch`, `Ledger::begin_batch`, and the
   in-process core's `apply_batch`): the units are decided one by one against the book and each
   other, exactly as alone, and their merged delta is one vault commit, one store commit, one
   log record, one root, one fsync.

Together: the store's commit per posting falls **66 %** for a single posting in memory
(1 958 → 660 µs) and **80 %** batched (382 µs); on disk, where the fsync dominates, batching
takes a posting from 20.1 ms to **1.3 ms** (−93 %). The whole `Book::apply` per posting falls from
2.5 ms to 0.9 ms in memory and from 24.7 ms to 1.8 ms on disk. What is left in the paths is
mostly hashing: every leaf still hashes its 128 levels, which CipherClerk's root needs.

In-process prototype of shards (`crates/shards`, `MemStore`): 1 662 → 1 722 ops/s on one shard,
6 452 → 6 690 on eight (4 000 transfers, batches of 32, the same laptop); `MemStore` was not
the bottleneck there (the kernel and signing are).

Reproduce:

    cargo run --release --manifest-path spikes/Cargo.toml --bin commit -- --samples 300 --batches 1,32

## 7. The book in shards, as VOS Agents (`T-031-20`, `T-031-21`, 2026-10-03)

`aledgent-core-host shards` (stress workflow, `core=shards-recompiler`): N book-shard Agents and one
principals Agent, Local Agents of one Space on one node, each on its own host thread, the host
checking every reply against a native replica. Per shard count: 256 accounts opened through the
principals Agent and funded, 8 agent principals granted, then 2 000 transfers (half by a principal,
a tenth to a fee collector held as a pool), one round of up to 64 commands per shard per step,
principal 0 revoked and granted again half way. One GitHub-hosted runner per run (2 vCPU); every
profile of a run on that one machine. VOS's per-invocation bounds raised (the batch curve's
`raise_limits`; on `perf/agent-stack` the shard Agents also take `ExecutionLimits::MAXIMUM`): a
Local Agent's rows are its image, and a shard's book is in its rows.

**On VOS `perf/agent-stack` `6c794311`** (run
[37145822887](https://github.com/bloque-app/bloque-ledgent/actions/runs/37145822887), AMD EPYC 7763,
both profiles on one runner, the host at `vos-perf-stack` merged into `f-031-shard-agents-perf`):

| Shard Agents | JAM-native decided/s | VOS-native decided/s | Between shards | Step p50 (JAM / VOS) | Credit lag p99 (JAM / VOS) | Revocation lag (JAM / VOS) | Actor gas per round | Largest Agent state |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | **48** | **54** | 0 | 1.28 s / 1.14 s | | 1.20 s / 1.14 s | 6 524 M | 2.92 MB |
| 2 | **34** | **36** | 875 | 3.56 s / 3.42 s | 3.76 s / 3.62 s | 3.62 s / 3.50 s | 7 123 M | 2.08 MB |
| 4 | **33** | **34** | 1 365 | 6.58 s / 6.35 s | 7.15 s / 6.95 s | 6.81 s / 6.78 s | 6 260 M | 1.34 MB |
| 8 | **33** | **34** | 1 544 | 8.16 s / 7.79 s | 13.6 s / 13.9 s | 13.1 s / 13.0 s | 5 365 M | 0.74 MB |

JAM-native: the standard runtime with no features. VOS-native: `vos-native,blake2b-precompile`,
the actor's program through `historical_lookup` (`F025_PROGRAM_LOOKUP=1`). Every run: the trial
balance holds with credits in transit, every credit applied exactly once (read back from the
Agents), every reply equal to its replica's, 1 998 / 1 991 / 1 979 / 1 967 transfers accepted (the
rest refused for funds or by the revocation fence).

**On the pinned VOS** (`3140c3d` with `de0f12f`), the same workload:

| Shard Agents | EPYC 7763 ([37142676739](https://github.com/bloque-app/bloque-ledgent/actions/runs/37142676739)) | EPYC 9V45 ([37143913595](https://github.com/bloque-app/bloque-ledgent/actions/runs/37143913595)) |
|---:|---:|---:|
| 1 | 41 | 102 |
| 2 | 28 | 95 |
| 4 | 27 | 89 |
| 8 | 28 | 87 |

**Reading.**

- **The runner's CPU decides more than the VOS version.** The same actor gas (6 528 M a round)
  ran 2.5 times faster on the 9V45 than on the 7763. Comparisons hold only within one job.
- **`perf/agent-stack` gains 17 % (JAM-native) and 32 % (VOS-native) on one shard** against the
  pinned VOS on the same CPU model (41 → 48 and 54 decided/s). Its levers cut the runtime's fixed
  work per invocation; the actor's own gas is unchanged, about **100 M gas a transfer** (signing,
  verifying and committing in the guest), and it is what a round costs.
- **No scaling on 2 vCPU.** The 2 vCPU are one physical core's two threads, and a transfer between
  shards is two postings (the debit, then the credit), so the work per transfer rises with the
  shard count (44 %, 68 % and 77 % of transfers cross at 2, 4 and 8). Natively, with cores to spare,
  the same Agents' transition scales 859 → 3 537 decided/s from 1 to 8 shards (`NOTES-shards.md`); on VOS the gain from shards needs as many cores as shard Agents.
- **The lags are one step.** A credit is applied, and a revocation reaches every shard, in the next
  round of each shard: here 1.1 s to 13.9 s, past `NFR-19`'s proposed 2 s and `NFR-20`'s 1 s once
  several shards share one core (owner question `s-11`).
- **A Local Agent's image grows with its book** (2.9 MB for one shard after 256 accounts and
  2 000 transfers). Every invocation moves it; external state on Shared Agents removes that term
  (`DEP-11`, owner question `s-05`).

Natively (`crates/shards/tests/agents_throughput.rs`, the same transition over committed maps,
replicas checked, 12 cores): 859, 1 235, 2 074 and 3 537 decided/s on 1, 2, 4 and 8 shard Agents.

Reproduce: `gh workflow run stress.yml --ref <branch> -f core=shards-recompiler -f shard_counts=1,2,4,8
-f vos_ref=<VOS commit> -f vos_picks=none -f "shard_profiles=jam= vos=vos-native,blake2b-precompile@lookup"`
(profiles need a VOS with them and a core host that sets `F025_MAX_LIMITS`, as `f-031-shard-agents-perf`).

## 8. VOS's cost per invocation, and the guest's (`F-025`, 2026-10-03)

The core Agent on aledgent's VOS fork, branch `perf/agent-stack` (`saga/agents` `e6f2bb45` with
`proposals/vos/0001`–`0005` merged and `0006`–`0010` committed). Gas is exact and the same on both
engines (the interpreter on an M2 Pro locally, `aledgent-core-host measure`). Time is on
GitHub-hosted `ubuntu-latest` (2 vCPU), recompiler, with every variant of a table measured in **one
job on one machine** (`x86-bench.yml`, mode `compare`): hosted runners differ in CPU from job to
job, by up to 1.7× on this workload (an EPYC 9V45 against an EPYC 7763).

### 8.1 The runtime's fixed cost per Invoke

The core's 2.36 MB program, one reservation per Invoke; the actor's own work, 294.5 M, is
subtracted.

| Runtime | Runtime gas per Invoke | Gas per program byte |
|---|---:|---:|
| `3140c3d` + `de0f12f` (the old pin) | 1 571 M | 667 |
| `perf/agent-stack` before 0006 (0001–0005) | 1 142 M | 485 |
| + validate the decoded work once (0006, part 1) | 793 M | |
| + public I/O by reference (0006, part 2) | 618 M | |
| + the program by its verified `BlobRef` (0006, part 3): one hashing pass | 269 M | 114 |
| measurement-only stub, passes 2 and 3 skipped (never to merge) | 270 M | |
| + the scheduled BLAKE2b (0007): JAM-native profile | 175 M | 74 |
| + precompile 100 for the runtime's hashing (0007, option) | 110 M | |
| VOS-native profile, program by `historical_lookup` (0008) | 90 M | 38 |

On today's `saga/agents` the runtime hashed the program **six** times per Invoke, not the three of
`proposals/vos/0004`. The decoded work was validated three times, each hashing every blob, and the
ProgramId was computed twice. Each pass over 2.36 MB is about 175 M gas.

### 8.2 Inputs per second, the runtime's levers

Run 37145564561, one EPYC 7763: the old guest (`opt-level = "z"`, CipherClerk with 0001 only) on each
runtime. The old pin on the same CPU type (run 37142322150) gave 1.72, 3.84 and 4.09 inputs/s at 1, 8
and 32, and 7.43 and 10.03 at 8 and 32 with 8 G.

| Runtime | 1 | 8 | 32 | 128 | 8 G: 8 | 8 G: 32 | 8 G: 128 |
|---|---:|---:|---:|---:|---:|---:|---:|
| stack before 0006 | 1.68 | 3.68 | 3.86 | 4.10 | 7.60 | 10.15 | 10.62 |
| stub (never to merge) | 5.02 | 8.90 | 9.25 | 9.11 | 12.43 | 13.72 | 14.00 |
| 0006, one pass | 5.22 | 9.05 | 9.40 | 9.47 | 12.74 | 13.96 | 14.35 |
| + 0007, JAM-native | 7.87 | 11.63 | 11.88 | 11.86 | 14.58 | 15.40 | 15.44 |
| + precompile 100 | 9.11 | 12.62 | 12.81 | 12.94 | 14.49 | 15.77 | 15.87 |
| VOS-native | 10.09 | 12.91 | 13.53 | 13.27 | 14.69 | 15.70 | 14.45 |

"8 G" is 8 000 M of actor gas an invocation: through the Agent's execution limits (0009) where the
runtime has them, otherwise with `MAX_EXECUTION_GAS` raised in the tree. With the program hashed
once, the fixed cost no longer binds a batch. The actor's own gas does: 295 M a reservation.

### 8.3 Where the actor's gas goes

One Invoke of 16 reservations (4.96 G) profiled per program counter on the interpreter and resolved
to the core ELF's symbols. That is 4.75 G of actor work (297 M a reservation) against 0.21 G of
runtime. On the hosted runner, guest execution is about 99 % of a batched round, at roughly
3.5–5 G gas a second; the host's preparation, encoding and native decision are about 3 %. **The
Agent is gas-bound, not host-bound.**

| Actor gas | Share |
|---|---:|
| BLAKE2b (CipherClerk's `blake2b_256` on the `blake2` crate: compress, finalize) | 32 % |
| `memcpy` / `memset` (mostly the `digest` wrapper's per-hash buffers, and curve table selects) | 27 % |
| curve25519 arithmetic (field mul, `pow2k`, conditional assign) | 17 % |
| ledger-core and the binding | 4 % |
| other (outlined code, allocation, decode) | 20 % |

8 470 one-block hashes per reservation, about 16 k gas each, of which `merkle::build_empty_chain`
(14 rebuilds of 128 hashes) is 21 %.

### 8.4 The guest's levers, and inputs per second


The guest levers of `proposals/cipherclerk/0003`–`0006` and VOS 0010 were measured one at a time,
in one `compare` job on one hosted runner (run 37149316771, AMD EPYC 7763), on the recompiler, with
384 reservations a set. "VOS-native" is the runtime of 0008, "+ curve" adds the native curve calls
of 0010. Actor gas is per reservation at batch 1; inputs/s are over the whole run.

| Runtime / guest | Actor gas | Runtime gas | 1 G: 1 | 8 | 32 | 128 | 8 G: 8 | 32 | 128 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| VOS-native / today's guest | 301 M | 95 M | 9.36 | 12.98 | 13.26 | 13.08 | 14.84 | 15.51 | 15.52 |
| + static empty chain (0004) | 264 M | 95 M | 10.73 | 14.58 | 14.62 | 13.82 | 16.91 | 17.46 | 17.79 |
| + VOS BLAKE2b only (0003) | 235 M | 95 M | 12.85 | 18.36 | 18.80 | 18.53 | 22.38 | 23.61 | 23.86 |
| + both (0003 + 0004) | 212 M | 95 M | 13.24 | 19.83 | 20.25 | 20.05 | 24.45 | 26.04 | 26.47 |
| + curve, native (0005, VOS 0010) | 162 M | 95 M | 14.38 | 21.85 | 22.49 | 22.57 | 28.25 | 30.36 | 30.68 |
| + `opt-level = 3` (0006) | 109 M | 110 M | 12.16 | 21.19 | 21.79 | 21.52 | 29.48 | 32.80 | 33.42 |
| JAM-native, all levers | 109 M | 264 M | 7.45 | 15.11 | 15.99 | 15.85 | 24.97 | 30.18 | 30.87 |

- **Each lever pays on its own.** Under the Agent's limits at batch 128 the guest goes from
  15.5 to 33.4 inputs/s: BLAKE2b through `vos_protocol` is the largest step (+8.3 on its own),
  the native curve next (+4.2), the static chain +2.3 on its own and +2.6 on top of BLAKE2b.
- **`opt-level = 3` is a trade.** It cuts actor gas by a third (162 → 109 M) but nearly doubles
  the program (2.34 → 4.33 MB), and the larger program costs 15 M more runtime gas an Invoke on
  VOS-native and 4 % to 15 % of wall time where the round is short. It wins only on batches of 8 or
  more under the Agent's limits; at batch 1 and under the stock 1 G it loses. aledgent runs batched,
  so the image keeps it.
- **JAM-native catches up as batches grow.** Its fixed 264 M is paid once an Invoke, so at
  batch 128 it is within 8 % of VOS-native; at batch 1 it is 39 % behind.
- **Against the start of this note**, same runner type: the stock stack did 1.68 / 3.68 / 3.86 /
  4.10 inputs/s at batch 1 / 8 / 32 / 128 under 1 G (§8.2). The full stack does 14.38 / 21.85 /
  22.49 / 22.57 under 1 G with the `opt-level = z` guest, and 33.4 at batch 128 under the Agent's
  8 G limits: 8.1× the stock 1 G figure, and 3.1× the stock stack with its gas raised to 8 G
  (10.62).

**What is left.** At 109 M a reservation, the profile shifts to the SMT path itself. A compact
SMT (`proposals/cipherclerk/0006`, estimate only) cuts path hashes 7× to 15× for 1 k to 1 M
leaves, which should take a reservation to about 77 M. It cannot keep today's roots, so it needs a
versioned root, and that is a decision for CipherClerk and aledgent, not a tuning.

## 9. The shard Agents with a core each (`T-031-21`, 2026-10-03)

The same workload as §7, on one self-hosted fleet machine (`runner=zombienet`, 16 vCPU, AMD EPYC
Genoa). The principals Agent was pinned to core 0 and shard Agent *i* to core *i + 1*. VOS was
`perf/agent-stack` `c54fc1ff`, with the shard guest taking the core's levers: VOS BLAKE2b, the
static empty-subtree chain, curve precompiles served natively, and `opt-level = 3`. Both profiles
ran in one job (run [37154004755](https://github.com/bloque-app/bloque-ledgent/actions/runs/37154004755),
from `f-031-shards-fleet-perf`, which is the `vos-perf-stack` harness merged with #146).

| Shard Agents | JAM-native decided/s | VOS-native decided/s | Speed-up (JAM) | Step p50 | Credit lag p99 | Revocation lag | Actor gas per round |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | **107** | **105** | 1.0× | 587 ms | | 610 ms | 3 088 M |
| 2 | **177** | **179** | 1.7× | 644 ms | 820 ms | 772 ms | 3 400 M |
| 4 | **320** | **296** | 3.0× | 601 ms | 690 ms | 613 ms | 2 863 M |
| 8 | **627** | **642** | 5.9× | 606 ms | 673 ms | 608 ms | 2 324 M |

JAM-native is `native-curve`; VOS-native is `vos-native,native-curve` with programs looked up.
Every run passed three checks: exactly once (read back from the Agents), the trial balance with
credits in transit, and every reply equal to its replica's.

- **Shards scale when each has a core.** Eight shard Agents decide 5.9 times what one decides,
  although 77 % of their transfers cross shards and so cost two postings. The step stays near
  0.6 s, so the lags stay within one step: credit lag p99 under 0.82 s against `NFR-19`'s proposed
  2 s, and revocation lag under 0.78 s against `NFR-20`'s proposed 1 s.
- **The guest levers halve the actor's gas per round** (6 524 M → 3 088 M for 64 transfers, about
  48 M a transfer).
- The 2-vCPU results in §7 were the hardware's: two vCPU are one core.

Reproduce: `gh workflow run stress.yml --ref f-031-shards-fleet-perf -f core=shards-recompiler -f runner=zombienet
-f vos_ref=c54fc1ff13fcd55b86f2cf4edc77b041b62e245f -f vos_picks=none -f "shard_profiles=jam=native-curve vos=vos-native,native-curve@lookup"`.

## 10. The JAM-compatible default: the new reference (2026-10-04)

The owner's ruling of 2026-10-04, agreed with VOS's maintainer, keeps VOS improvements within JAM's
restrictions: the runtime issues only the Gray Paper's host calls. aledgent's defaults follow it
since #156. The runtime has no features, so the JAM-native profile of `proposals/vos/0008` applies:
the program travels in the work and is hashed once in the guest (0006, 0007), and the per-Agent
limits (0009) apply. The core and shard guests use VOS's BLAKE2b and the static empty chain
(`proposals/cipherclerk/0003`, `0004`), do their **curve arithmetic in software**, and are built
at `opt-level = 3`. `aledgent-core-host runtime-pvm` lists the runtime's host calls: **9 to 14
only**, for VOS's standard runtime (`c54fc1ff`) and for aledgent's (`188bf162`, with the composite
engine). The native curve calls (110–114), the BLAKE2b precompile (100) and the program lookup
keyed by ProgramId are outside JAM (`proposals/vos/0012`). §8.4's "JAM-native, all levers" row
(30.87) used the native curve calls, so it was not a JAM-compatible figure.

### 10.1 One core Agent: gas and reservations per second

Run [37215699281](https://github.com/bloque-app/bloque-ledgent/actions/runs/37215699281), one hosted
`ubuntu-latest` job on an **AMD EPYC 7763**, 2 vCPU, recompiler, 384 reservations a set, VOS
`188bf162` (`perf/agent-stack` `c54fc1ff` with the composite engine). Gas is exact and the same on
every machine. "8 G" is 8 000 M of actor gas an invocation through the Agent's execution limits
(0009); "1 G" is VOS's standard bound.

| Runtime / core guest | In JAM | Runtime gas per Invoke (batch 1) | Actor gas per reservation | 1 G: 1 | 8 | 32 | 128 | 8 G: 8 | 32 | 128 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **standard, no features / software curve, opt 3 (the default)** | yes | **265 M** | **138 M** | 7.34 | 14.03 | 14.85 | 14.94 | **22.48** | **26.62** | **27.28** |
| aledgent's runtime, no features / the same guest | yes | 265 M | 138 M | 7.26 | 14.13 | 14.82 | 14.80 | 22.67 | 26.41 | 27.43 |
| standard, no features / software curve, opt z (2.35 MB) | yes | 179 M | 212 M | 9.36 | 16.38 | 17.08 | 17.11 | 22.86 | 25.14 | 25.67 |
| standard, no features / precompiles served in the guest, opt 3 | yes | 253 M | 196 M | 7.02 | 13.06 | 13.66 | 13.82 | 20.09 | 23.16 | 23.41 |
| `native-curve` / precompiles served natively, opt 3 (§8.4's last row) | **no** | 265 M | 109 M | 7.55 | 15.32 | 16.05 | 16.22 | 25.64 | 31.16 | 32.13 |

Inputs/s are over the whole run. The runtime's gas per Invoke grows with the batch, because each
caller blob is hashed: 292, 313 and 348 M at 8, 32 and 128 under 8 G for the default.

- **The JAM-compatible default does 27.3 reservations/s at batch 128 under 8 G, not 30.9.** The
  30.9 of §8.4 included VOS's native curve calls. On this one machine, those calls give 32.1. Staying
  within JAM costs 15 % at batch 128 and 16 % at batch 32. The whole difference is the actor's
  curve arithmetic: 138 M against 109 M a reservation.
- **The fixed cost per invocation is 265 M** for the 4.35 MB `opt-level = 3` core, and 179 M for
  the 2.35 MB `opt-level = z` core. Most of it is the one in-guest pass over the program with the
  scheduled BLAKE2b: the 2.0 MB larger program costs 86 M more, about 43 gas a byte.
- **Software curve beats the precompiles served in the guest**, 138 M against 196 M a reservation.
  Without native service, the runtime answers 110–114 as its own guest code at the interpreted
  price. The guests therefore drop `pvm-precompile` by default (#156).
- **`opt-level = 3` still pays under 8 G** at batches of 32 and 128 (27.3 against 25.7 at 128).
  It costs 86 M more per Invoke and saves 74 M a reservation. In gas, which is what a JAM core
  budgets, it wins from batch 2 on. Under the standard 1 G bound `z` is faster, but aledgent runs
  under its own limits.
- **aledgent's runtime costs nothing extra** on plain batches. It is the same program as the
  standard runtime plus the composite engine, which no host call reaches.
- A second job of the same dispatch ran on an EPYC 9V45 (run 37215697179). It gave 1.9× the
  inputs/s, with the same gas and the same ordering: 51.2 at batch 128 under 8 G for the default,
  against 61.0 with the native curve. Only the 7763 figures are comparable to §8.

### 10.2 The shard Agents on the fleet

Run [37216459517](https://github.com/bloque-app/bloque-ledgent/actions/runs/37216459517), one self-hosted fleet
machine (`runner=zombienet`, 16 vCPU, AMD EPYC Genoa), one shard Agent per core as in §9, the
standard runtime with no features (`shard_profiles=jam=`), and the shard guest with software curve
at `opt-level = 3`, from #156's branch. VOS `c54fc1ff`. The workload is 2 000 transfers, 256
accounts and 64 commands a round.

| Shard Agents | Decided/s | Speed-up | Round p50 | Round p99 | Credit lag p99 | Revocation lag | Actor gas per round | §9 (native curve) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | **80** | 1.0× | 778 ms | 930 ms | | 783 ms | 4 429 M | 107 |
| 2 | **136** | 1.7× | 875 ms | 974 ms | 974 ms | 887 ms | 4 869 M | 177 |
| 4 | **230** | 2.9× | 870 ms | 1 019 ms | 1 020 ms | 887 ms | 4 215 M | 320 |
| 8 | **435** | 5.4× | 862 ms | 1 016 ms | 1 018 ms | 863 ms | 3 543 M | 627 |

Every count passed the trial balance and the exactly-once check, and produced an anchor.

- **Shards still scale with cores**: 5.4× at eight, against 5.9× with the native curve.
- **Within JAM, each shard decides about a quarter to a third less** (80 against 107 at one, 435
  against 627 at eight), because a round's actor gas rises from about 3.1 G to about 4.4 G.
- **The lags stay within about one round** (round p50 0.78–0.88 s): credit lag p99 1.02 s against
  `NFR-19`'s proposed 2 s, and revocation lag at most 0.89 s against `NFR-20`'s proposed 1 s. The
  margin to 1 s is smaller than with the native curve, where the revocation lag was 0.61–0.77 s.

Reproduce:

    gh workflow run x86-bench.yml -f mode=compare -f vos_ref=188bf1628934515e2f163bb8b9acdd05a93ff3ee -f vos_picks=none \
      -f batches="1 8 32 128" -f batch_gas=8000000000 \
      -f guests="gz=1,3,4:vos-blake2b:z g3=1,3,4:vos-blake2b:3 gp3=1,3,4,5:vos-blake2b,pvm-precompile:3" \
      -f variants="jam=<ref>/gz jam=<ref>/g3 jam=<ref>/gp3 ald=<ref>!aledgent/g3 jamc=<ref>+native-curve/gp3"
    gh workflow run stress.yml -f core=shards-recompiler -f runner=zombienet \
      -f vos_ref=c54fc1ff13fcd55b86f2cf4edc77b041b62e245f -f vos_picks=none -f "shard_profiles=jam="

## 11. Compact trees, v2 roots (experiment `D-COMPACT-SMT`, 2026-10-04)

The owner approved prototyping `proposals/cipherclerk/0006` (2026-10-04, "prototype v2 now, adopt
if numbers hold"). `crates/compact-smt` hashes the book's trees as compact sparse Merkle trees:
a lone leaf is its shortcut node, an empty subtree is zero, only branching levels are hashed
(the Diem/Aptos Jellyfish hashing over a binary radix). Leaves and leaf hashes are unchanged.
Measured behind the Cargo feature `smt-v2`, then off by default; host and guests are built with it
together. The guests below are §10's default (VOS's BLAKE2b, the static empty chain, software curve,
`opt-level = 3`); `g3v2` is the same build with `smt-v2`. *Since 2026-10-05 (`DEC-25`) v2 is the
default and v1 the feature `smt-v1`, a supported opt-in that a deployment may choose at genesis (owner
ruling `cs-q04`: v1 is not weaker, only costlier, and these numbers are its cost). To reproduce, build the
v1 guest with the core feature `smt-v1` (`g3v1=1,3,4:vos-blake2b:3:smt-v1`) and the v2 guest with none.*

### 11.1 One core Agent: gas and reservations per second

Run [37240394265](https://github.com/bloque-app/bloque-ledgent/actions/runs/37240394265), one hosted
job on an **AMD EPYC 7763** (§10's machine type), recompiler, 384 reservations a set, VOS
`188bf162`, the standard runtime with no features. Both guests in the same job. The v1 rows
reproduce §10 (27.12 against 27.28 at batch 128 under 8 G).

| Core guest | Program | Actor gas per reservation | Invoke gas (batch 1) | 1 G: 1 | 8 | 32 | 128 | 8 G: 1 | 8 | 32 | 128 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| v1 (`g3`, §10's default) | 4 431 524 B | **138 M** | 407 M | 6.99 | 13.98 | 14.56 | 14.63 | 7.23 | 22.32 | 26.39 | 27.12 |
| v2 (`g3v2`) | 4 429 736 B | **72 M** | 342 M | 7.78 | 17.42 | 18.32 | 18.32 | 8.00 | **31.60** | **39.48** | **40.30** |
| v2 / v1 | | −48 % | −16 % | +11 % | +25 % | +26 % | +25 % | +11 % | **+42 %** | **+50 %** | **+49 %** |

Inputs/s are over the whole run. Gas is exact and the same on every machine.

- **A reservation's actor gas halves: 137.9 M to 72.0 M.** The estimate of `proposals/cipherclerk/0006`
  was 77 M from 105 M with the native curve; with software curve the saving is the same 66 M of
  hashing. The witness's multiproofs were verified, and the roots recomputed after every commit,
  over 128 levels a touched key; v2 hashes the branching levels only.
- **Within JAM, the core does 40.3 reservations/s at batch 128 under 8 G, against 27.1.** That is
  above §8.4's 30.9 with VOS's native curve calls, which are outside JAM. Under VOS's standard
  1 G bound the gain is a quarter (the runtime's fixed 270 M an invocation dominates).
- **The runtime's cost per invocation does not change** (269 M against 270 M at batch 1): the
  program is 1.8 KB smaller.
- **The witness grows by about a quarter**: 10.4 KB a reservation against 8.2 KB at batch 8. Not
  attributed entry by entry; the likely bulk is the service tree's multiproof, whose node entries
  travel opened (two hashes each), and lone-leaf entries, which carry their key. The driver packs an invocation up to its 184 KiB blob bound, so at batches of 32 and
  128 an invocation carries 14.8 and 16.0 reservations instead of 16 and 19.2, and pays the
  runtime's fixed cost more often. The gain at batch 128 is net of that. Before the wire form was
  packed (one 50-byte record an entry, as v1's), the enum's 84 bytes an entry made it 45 % larger
  (runs 37237857306 and 37237859310, EPYC 9V74: +36 %, +32 %, +30 % at 8, 32, 128 under 8 G).
- Three other jobs of the same comparison landed on other machines (EPYC 9V74 twice, Xeon 8573C
  once): the same gas, and v2 ahead by 30–50 % under 8 G on each.

### 11.2 The shard Agents on the fleet

Run [37237876657](https://github.com/bloque-app/bloque-ledgent/actions/runs/37237876657), two jobs on the
self-hosted fleet (`runner=zombienet`, 16 vCPU, AMD EPYC Genoa), §10.2's workload (2 000 transfers, 256
accounts, 64 commands a round), the standard runtime with no features, VOS `c54fc1ff`. v1 and v2
ran on two machines of the same class at once.

| Shard Agents | v1 decided/s | v2 decided/s | Gain | v1 actor gas per round | v2 | v2 credit lag p99 | v2 revocation lag |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 82 | **88** | +7 % | 4 430 M | 4 112 M | | 691 ms |
| 2 | 140 | **148** | +6 % | 4 871 M | 4 528 M | 904 ms | 817 ms |
| 4 | 237 | **252** | +6 % | 4 214 M | 3 908 M | 940 ms | 863 ms |
| 8 | 473 | **492** | +4 % | 3 543 M | 3 277 M | 981 ms | 851 ms |

Every count passed the trial balance and the exactly-once check. The v1 rows are within 9 % of
§10.2's (another machine of the class, another hour).

- **The shards gain 4–7 %, not half.** A shard keeps its book in committed maps (layout 3): it
  never verifies a multiproof, and its tree writes, 128 hashes each in v1, were about 7 % of a
  round's actor gas (what v2 saved); the rest is the kernel's curve arithmetic. The core's
  layout 2 (witnesses) is where the hashing was.
- The lags stay within a round: credit lag p99 at most 981 ms (`NFR-19`'s proposed 2 s),
  revocation lag at most 863 ms (`NFR-20`'s proposed 1 s).

Reproduce:

    gh workflow run x86-bench.yml --ref <branch> -f mode=compare -f vos_ref=188bf1628934515e2f163bb8b9acdd05a93ff3ee \
      -f vos_picks=none -f batches="1 8 32 128" -f batch_gas=8000000000 \
      -f guests="g3=1,3,4:vos-blake2b:3 g3v2=1,3,4:vos-blake2b:3:smt-v2" \
      -f variants="jam=188bf1628934515e2f163bb8b9acdd05a93ff3ee/g3 jam=188bf1628934515e2f163bb8b9acdd05a93ff3ee/g3v2"
    # stress-runs/<name> with e2e/stress/matrix.json entries
    #   { "core": "shards-recompiler", "runner": "zombienet", "vos_ref": "c54fc1ff…", "vos_picks": "none",
    #     "shard_profiles": "jam=" }, and the same with "core_features": "smt-v2"

## 12. The shards across machines: the cross-host relay (an experiment, 2026-10-04)

`T-031-23`, `T-031-24`; `features/F-031-core-throughput/NOTES-multihost.md` and `OUTCOME-multihost.md`.
The relay (`shards::net`, feature `net`) carries every stream between hosts over Noise; the bench is
`aledgent-core-host fleet` (stress workflow `core=shards-fleet`). Hosts: 16-vCPU fleet machines (AMD EPYC
Genoa, 16 vCPU), 15 shard Agents each (the principals Agent beside node 0's), on the cloud provider's private
network: round trip 0.5–0.9 ms, every port of 47100–47199 open both ways
(probe run [37257171192](https://github.com/bloque-app/bloque-ledgent/actions/runs/37257171192)). Profile
`jam=`, VOS `c54fc1ff`, 256 accounts, rounds of up to 64 commands, every shard driven on its own.
Each run is a fresh book. Every phase ends in a quiescence, then the checks: exactly once from the
Agents' heads, the trial balance with credits in transit, every live anchor (one a second) consistent.

**Runs** [37257923201](https://github.com/bloque-app/bloque-ledgent/actions/runs/37257923201) (1, 2, 3 hosts)
and [37259648332](https://github.com/bloque-app/bloque-ledgent/actions/runs/37259648332) (3 hosts, paced and
burst, fresh books):

| Hosts | Shards | Phase | Offered | Decided/s | Decided during the load | Ack p50 / p99 | Credit lag p50 / p99 | Revocation lag | Round p50 / p99 | Shards busy | Trial balance | Exactly once |
|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---|
| 1 | 15 | closed loop | 11 250 | **693** | | 1.59 / 6.49 s | 1.26 / 5.26 s | 1.85 s | 271 / 1 191 ms | 74 % | holds | yes |
| 1 | 15 | paced at 623/s, 15 s | 9 345 | 500 | | 0.93 / 4.08 s | 0.92 / 2.86 s | 2.18 s | 404 / 1 537 ms | 77 % | holds | yes |
| 2 | 30 | closed loop | 15 000 | **1 072** | | 1.64 / 7.50 s | 1.16 / 8.54 s | 1.89 s | 158 / 1 141 ms | 63 % | holds | yes |
| 2 | 30 | paced at 964/s, 20 s | 19 280 | 807 | 916/s (18 312 in 20 s) | 0.64 / 2.95 s | 0.63 / 2.47 s | 1.52 s | 258 / 950 ms | 78 % | holds | yes |
| 3 | 45 | closed loop | 18 000 | **1 373** | | 1.54 / 7.73 s | 1.13 / 7.99 s | 1.99 s | 129 / 1 159 ms | 60 % | holds | yes |
| 3 | 45 | paced at 1 300/s, 25 s | 32 500 | 1 073 | 1 242/s (24 846 in 20 s) | 0.36 / 4.78 s | 0.35 / 2.89 s | 2.68 s | 131 / 813 ms | 79 % | holds | yes |
| 3 | 45 | paced at 1 500/s, 20 s | 30 000 | 1 159 | **1 426/s** (28 525 in 20 s) | 0.35 / 5.00 s | 0.35 / 2.64 s | 1.64 s | 133 / 844 ms | 76 % | holds | yes |

*Decided/s* is every decision over the phase, from the first offer to the last answer, so it includes
the drain after the load (3.7–8.8 s). *Decided during the load* is what was answered by the end of
the offered window. The first 3-host 1 500/s phase, in run 37257923201, ran on the closed loop's book.
It halted after about 20 s, when shard 2's Agent panicked in the runtime (below), with 27 500 answered
by then.

**The burst on the sharded capacity** (owner ruling G-03; F-032's gate 2: 50 × the reference minute for a
minute, between minutes at 1 ×, drained within 30 s), 3 hosts, run 37259648332, a fresh book:

- **Absorbed and drained within the bound.** The coordinator's progress lines show the backlog at
  most 460 commands during the burst minute (under one second of load). At the 10 s mark after the
  minute ended (130 s) only 3 were outstanding, inside the 30 s bound.
- **Then the book halted.** At about 175 s, in the closing minute at 1 × the reference minute, shard 31's Agent
  panicked in the runtime (`runtime exit Panic at pc 0`). Its 46 commands were never answered, and
  the phase was cut off by the stall guard, so the run reports no table and no final checks. Every
  phase before it on these machines held both checks.

**The halts are the Local Agent's image limit.** Both came at about 970–1 020 transfers a shard
(43 620 or about 46 000 over 45 shards). In three hosts nearly every transfer crosses shards, so a
shard also applies about as many credits. Images were already 3.0–3.3 MB at 670–720 transfers a shard
(the paced books), and F-034 places the failure near 4.4 MB. A 3-host book therefore holds about
1 000 transfers a shard, about 45 000 in all, about 30 s at 1 500 a second. External state on Shared
Agents (`DEP-11`) removes the limit; a fleet run cannot go longer without it.

**Where the cross-host scaling goes** (693 → 1 072 → 1 373 closed-loop: ×1.55 and ×1.98 for twice
and three times the machines):

- **Not the network.** The relay sent 2–3 MB per phase over the private network, and each host's
  CPU peaked at 10–12 of 16 cores.
- **Rounds fragment.** A shard driven on its own takes a round as soon as anything arrives: a few
  credits, a trim or a feed entry. The 1-host closed loop averaged 22 commands a round (520 rounds,
  2 254 M actor gas each). The 3-host paced runs averaged 4–5 (7 324 rounds for 32 500 transfers,
  466 M a round). Every round pays the runtime's fixed cost per invocation (`MEASUREMENTS` §10.1:
  265 M), so more of each shard's time goes to it.
- **More postings per transfer.** 83 % of transfers crossed shards on 1 host and 88 % on 3. Each
  crossing transfer is two postings.
- **Shards are not saturated** (76–79 % busy in the paced runs). The rest is waiting for work that
  the next small round will carry.

F-034 found the same lever: rounds must be full (`Cluster::round_wait`). The per-shard driver has no
round wait yet. It was not added: MVP freeze, 2026-10-04.

**Correctness, and the owner's ruling.** Every phase that completed held the trial balance and
exactly once, and every live anchor was a consistent cut. The relay's shape is adopted for `NFR-21`
(a) (owner ruling `mh-relay-shape`, 2026-10-05, pending this run).

**Bounds.** None of the proposed latency bounds holds in any run, one host included. Ack p99 is
3.0–7.7 s, against `NFR-16`'s 1 s. Credit lag p99 is 2.5–8.5 s, against `NFR-19`'s proposed 2 s.
Revocation lag is 1.5–2.7 s, against `NFR-20`'s proposed 1 s. With the PVM on the critical path,
a command waits for its shard's queue and then for a round of up to about 1.2 s at p99, and a credit
waits for two such rounds.

**The latency bounds under native decide** (owner ruling `mh-q02`, 2026-10-05; run
[37267168415](https://github.com/bloque-app/bloque-ledgent/actions/runs/37267168415)). The machines, network and
phases are as above. Each round is decided and committed on the native shard, and the callers' answers
leave at once. The round is replayed through its Agent (`F-034`, two rounds per replay, a 200 ms round
wait). Its credits, trims and audit record leave only when the replay confirms it. The revocation fence
holds from the native application.

| Hosts | Phase | Decided during the load | Ack p50 / p99 (`NFR-16`: 1 s) | Credit lag p50 / p99 (`NFR-19`: 2 s) | Revocation lag (`NFR-20`: 1 s) | Replay lag p50 / p99 | Trial balance | Exactly once |
|---:|---|---:|---:|---:|---:|---:|---|---|
| 1 | closed loop, 6 000 | 9 469/s | 155 / 392 ms | 2.18 / 3.96 s | 172 ms | 1.67 / 3.91 s | holds | yes |
| 1 | paced at 600/s, 15 s | 592/s | 139 / **270 ms** | 0.72 / **3.41 s** | **23 ms** | 0.49 / 3.51 s | holds | yes |
| 3 | closed loop, 12 000 | 23 399/s | 154 / 437 ms | 2.76 / 6.80 s | 202 ms | 0.82 / 4.22 s | holds | yes |
| 3 | paced at 1 300/s, 15 s | 1 288/s | 129 / **247 ms** | 0.75 / **5.05 s** | **38 ms** | 0.22 / 3.61 s | holds | yes |
| 3 | paced at 1 500/s, 20 s | **1 490/s** | 129 / **248 ms** | 0.63 / **5.22 s** | **52 ms** | 0.20 / 3.85 s | holds | yes |

Next to the PVM on the critical path (the table above), on the same machines:

- **`NFR-16` holds under native decide**: ack p99 0.25–0.27 s paced, against 3.0–5.0 s.
- **`NFR-20` holds**: revocation lag 23–52 ms paced, against 1.5–2.7 s.
- **`NFR-19` does not hold**: credit lag p99 3.4–5.2 s paced, against 2.5–2.9 s. A credit now waits for
  its debit's confirmation, so the replay's lag (p99 3.5–3.9 s) is added to it.
- **1 500 a second is answered**: 1 490/s during the load, with the backlog at most 333 and drained
  0.24 s after the load. The replay kept up: every round was confirmed before the phase's quiescence,
  and its lag stayed bounded at about 4 s p99 over 20 s. On one host, F-034 had measured that lag
  growing for as long as the load lasted.
- **The burst** (3 hosts, a fresh book) was absorbed. The backlog stayed under 100 during the burst minute
  (50 × the reference minute) and drained within 10 s. The run then stalled with 47 commands unanswered, at about
  43 500 transfers (about 970 a shard). That is the book size at which the verified run's shard halted
  at the Local Agent's image limit. No halt was reported, so the cause in native mode is not established.

**Contention.** All three 16-vCPU machines machines came up within 2–3 minutes in both runs. In an earlier
attempt (37241857027) one machine waited 11 minutes behind another session's job. The quota (80 vCPU,
about 26 used outside the runner autoscaler) fits three 16-vCPU machines and no fourth.

**What 1 500 a second needs.** Three hosts decide 1 426 a second while 1 500 is offered, and the
backlog grows by about 74 a second, so they fall 5 % short. Two levers would close the gap:

- **Fuller rounds.** A round wait, as F-034's, gives no extra cost in machines.
- **A fourth 16-vCPU machine.** 64 vCPU in the fleet, so a project limit of at least **90** with the 26 used
  outside the runner autoscaler. Extrapolated from +379 and +301 for the second and third hosts, a fourth adds about
  +250, to about 1 600 closed-loop.

Neither brings the latency bounds within reach. A sustained run longer than about 30 s needs
`DEP-11`.

Reproduce: `git push origin <branch>:stress-runs/<name>`, with `e2e/stress/matrix.json`:

    [{ "name": "mh62", "core": "shards-fleet", "fleet_runner": "zombienet", "fleet_nodes": "3",
       "fleet_counts": "1/closed:11250,paced:v90:15 2/closed:15000,paced:v90:20 3/closed:18000 3/paced:1500:20",
       "shard_profiles": "jam=", "shard_batch": "64", "fleet_wait_secs": "2400",
       "fleet_args": "--first 15 --per-node 15 --drain-cap-secs 300 --stall-secs 180" }]

and a separate entry with `"fleet_counts": "3/burst:750:60:15:60:30"` for the burst.

## 13. The ceiling of a sharded service library (a bounded measurement, 2026-10-05)

Owner-approved (`rd-shard-scope-2`): an estimate of what sharding the **whole service library**, not
only kernel transfers, would gain, without building it. Each core Agent is the full library: open,
issue, transfer, reserve, capture and release. k of them run side by side on one 16-vCPU machine (AMD EPYC Genoa,
16 vCPU), one per core. Each Agent holds its own accounts, and no message passes between them, so k
Agents give the **ceiling** of k partitions. Tool: `aledgent-core-host measure`, batches of 32, the
production limits, 1 200 operations each (`tools/ci/measure-k.sh`, stress workflow
`core_measure_k`). JAM-compatible default (standard runtime, no features).

- **card**: a reservation, then its capture into a second account or (every other operation) its
  release. Two inputs an operation.
- **transfers**: transfers each way between two accounts. One input an operation.

Runs [37273865787](https://github.com/bloque-app/bloque-ledgent/actions/runs/37273865787) (v1 roots, today's
default) and [37273869870](https://github.com/bloque-app/bloque-ledgent/actions/runs/37273869870) (v2 compact
roots, `smt-v2`, on main behind its feature; adoption is #185). Inputs decided a second, summed over the
Agents (each Agent's rate over its own run). Every Agent committed every input; none failed.

| Mix | Roots | k = 1 | 2 | 4 | 8 | 15 | 15 vs 1 | Round p50 |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| card | v1 | 33.6 | 68.1 | 134.0 | 271.5 | **500.6** | ×14.9 | 0.95 s |
| card | v2 | 43.2 | 86.9 | 171.0 | 335.1 | **594.6** | ×13.7 | 0.77–0.82 s |
| transfers | v1 | 35.9 | 70.8 | 142.8 | 282.9 | **528.2** | ×14.7 | 0.90 s |
| transfers | v2 | 52.3 | 109.0 | 215.7 | 425.3 | **774.9** | ×14.8 | 0.60 s |

Over the wall time of the slowest Agent (set-up included), the k = 15 totals are 465 and 527 (card)
and 467 and 650 (transfers).

**Reading.**

- **Independent library partitions scale almost linearly with cores:** ×13.7–14.9 at 15 on 16 vCPU.
  Only v2's card mix loses 8 % per Agent at 15 (39.6 against 43.2).
- **v2 roots raise the library's rate by 18–28 % (card) and 46–50 % (transfers)**, more than
  proposal 0006's estimate of about 27 % less gas per reservation.
- **The library costs about three times the kernel alone.** One kernel shard Agent decides 102
  transfers a second (F-034's notes), and one library Agent 36 (v1) or 52 (v2).

**The penalty of crossing partitions (an estimate, not a measurement).**

- **Transfers.** 84 % of transfers cross partitions at 15 shards (§12, F-034's notes). A crossing
  transfer costs its debit in the payer's partition and its credit in the payee's. Counting the credit
  as one more input (conservative: a credit carries no operation record), the rate is the ceiling
  ÷ 1.84. That gives about **287/s (v1) and 421/s (v2) on one 16-vCPU machine**, against 653/s for the
  kernel-only shards (§10.2).
- **Relay overhead across machines.** The cross-host relay's round fragmentation cost about a third
  across three hosts (×1.98 for ×3 machines, §12). With that factor too, three machines give about
  **570–850/s (v1) and 830–1 250/s (v2)** for library transfers.
- **Reservations mostly do not cross.** A card's reservation, capture and release stay in the payer's
  partition when the merchant is a pool with a member in every shard (`REQ-RT-12`). So the card
  ceiling stands nearly as measured: **250 card operations a second (v1) and about 297 (v2) on one
  machine**, at two inputs each, scaling with machines like the shards (§12).

| Target: 1 500 a second | Library Agents needed (one a core) | 16-vCPU machines (15 Agents each) |
|---|---:|---:|
| Transfers, v1 (35.9 ÷ 1.84 = 19.5/s each) | 77 | 6 (8 with the relay's overhead) |
| Transfers, v2 (52.3 ÷ 1.84 = 28.4/s each) | 53 | 4 (6 with the relay's overhead) |
| Card operations, v1 (16.8/s each) | 89 | 6 |
| Card operations, v2 (21.6/s each) | 69 | 5 |

These are ceilings and estimates. A sharded library also needs the reservation index, the operation
store and settlement records partitioned (`s-04`), and none of that is built. The Local Agent's image
limit (`DEP-11`) bounds every run at this rate to tens of seconds, as in §12.
