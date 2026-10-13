<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-002 — Measurements

> **Amendment 0007 (2026-09-29).** The decision rules and every comparison "against 50 ms" in this
> note were written against a misreading of `NFR-1`. `NFR-1` bounds the **acknowledgement** of a
> command (50 ms at p99). Deciding a reservation has no budget of its own: it is part of
> **completion** (`NFR-15`: 30 s ceiling, 19 s target, at p99). The measurements stand as
> observations; the gaps to 50 ms they report are not requirement gaps. The note is kept as the
> record of how `DEC-4` was decided.

**Status:** measured, 2026-09-22/23. The decision rules in §2 were written and committed before any
spike ran (T-002-01, commit `3738e20`); one operational choice was withdrawn before any guest run,
in its own commit (`9aac1f3`), and says why. **Outcome: rule R3 applies — `DEC-4` is not confirmed**
(§8). The owner's ruling is pending. **Re-checked on VOS `saga/agents` at `2bb7876` (§11): the outcome
is unchanged.** **§12 measures the first two `F-025` changes: the acknowledgement cap is gone, the budget
is unchanged.** **§13 measures changes 3–4: a quarter of the per-program-byte cost is gone (the
core's reservation is 2.36 s at p99), the rest is hashing the runtime cannot drop alone, and the
core installs with declared resources.** **§14–15 measure change 5, recompiled Agent execution: on real
x86-64 (§15) VOS's suites pass on both engines and the core's reservation is 240 ms at p99, 12.5
times faster than the interpreter at identical gas and still about 4.8 times over 50 ms.**

Informative. Nothing here changes `SPEC.md` or `PLAN.md`. The recommendation on `DEC-4` is advice
to the owner; the ruling is the owner's (PLAN `M-0` gate).

---

## 1. Reference host and pins

| | |
|---|---|
| Machine | MacBook Pro `Mac14,10`, Apple M2 Pro, 12 cores (8 performance + 4 efficiency), 16 GiB |
| Disk | Apple SSD AP1024Z, 1 TB, APFS (29 GiB free at the start of the runs, 10 GiB at the end) |
| OS | macOS 26.6.2 (25G83), `aarch64-apple-darwin` |
| Linux for VOS | Docker Desktop 29.7.2 on the same machine: `aarch64` Linux VM (kernel 7.0.12-linuxkit), 12 vCPU, 7.8 GiB |
| Rust (native spikes) | stable 1.98.0, release profile (`opt-level = 3`, no LTO) |
| Rust (VOS host) | `nightly-2025-05-09` (VOS-saga `rust-toolchain.toml`), release, LTO off |
| Rust (PVM guests) | `nightly-2026-03-20` + `rust-src`, `-Zbuild-std`, target `riscv64em-vos` (VOS example actors' toolchain) |
| CipherClerk | `e9a7b0851f183c14dbd1635dbf0437c86682ad6f` (`e9a7b08`), default features off, as F-001 |
| VOS | `saga/agents` at `7bae926`. `master` at `d237827` is not measured (`DEP-5`) |
| aledgent | `ledger-kernel` as merged by F-001 (`26c6d7d`); its source is unchanged at this branch's base (`4c7d115`) |

**Not the production class.** The core's production host class is not fixed yet (Open points). This is
a developer laptop. Two consequences are known before any run and are stated here so they cannot be
argued after:

- (Withdrawn, see §2.1: the Agent path is interpreter-only on every target.) The PVM **recompiler** only exists on `x86_64` Linux (`pvm/runtime/src/kernel.rs:1286`, `:1303`).
  On this machine every guest runs on the **interpreter**. Guest `gas` is portable; guest wall time
  here is an upper bound for an `x86_64` Linux host.
- Local Agent creation, installation and invocation are Linux-only on `saga`
  (`vos/src/agent/host.rs:31`, `README.md:9-12`). S4 runs in the Linux VM above, on the same silicon.

## 2. Decision rules (written before any run)

### 2.1 The budget

`NFR-1`: a reservation decision completes within **50 ms at p99**, measured inside aledgent. Following
the plan, the reservation-path budget is:

    budget = S2(reserve, in the guest)
           + S3(verify-and-apply at 1 000 000 accounts, in the guest)
           + S4(one invocation at the state size the design carries: roots + hot state from S3)

Operational choices, fixed now:

1. Each term is its **p99**; the budget is the **sum of the p99s**. That over-states the p99 of the
   sum, so a budget that passes this way passes; one that fails is re-checked on the joint samples if
   they exist.
2. S2 and S3's verify-and-apply both run the kernel over the same posting (S3 re-runs it over the
   witness). The sum therefore counts the kernel twice. That is conservative and deliberate: the plan
   defines the budget this way, and `F-005` may not be able to avoid the second run.
3. The reserve term uses the **worse** of the two `DEC-2` options, so the rule does not pick `F-004`'s
   option for it.
4. The S4 term is taken at the smallest measured state size that is at least the design's hot state
   (roots + open-reservation index + bound counters, from S3). If the hot state exceeds 4 MiB, the design
   does not fit and rule R3 applies regardless of time.
5. "The witness fits the invocation limits" means: witness + events + openings, serialised as they
   would travel, is **≤ 16 KiB** (one invocation message), for the transfer and both reserve shapes at
   1 000 000 accounts. The reply limit (16 KiB) is checked against the delta the core returns.
6. "The book cannot live in 4 MiB" means: the account leaves that fit in 4 MiB (S3) are fewer than the
   accounts of a book at `NFR-2`'s volume, i.e. fewer than 1 000 000.
7. Guest time is measured on the interpreter (§1). If the budget exceeds 50 ms **only** through a guest
   term, the result is "open — re-run the guest terms on an `x86_64` Linux host", not a `RISK-2`
   trigger. Gas is reported so that re-run can be predicted.
8. A term that cannot be measured is written as missing with its cause. If the known terms alone
   exceed 50 ms, R3 applies. Otherwise the rule's outcome is "open", and says which term is missing.

**Correction to choice 7, made before any guest run (committed separately so the order is on
record).** Reading `saga`'s Agent execution path while writing the guest harness showed that the
premise of choice 7 is wrong: the Agent runtime runs through `RefineContext`, and
`pvm/runtime/src/refine.rs:10-12` says that path "always uses the interpreter — never the kernel or
the recompiler — on every target". The recompiler at `kernel.rs:1286` belongs to the older service
kernel, not to Agent invocations. So guest time on this host is **not** inflated relative to an
`x86_64` host by a missing recompiler; it differs only by CPU speed. Choice 7 is withdrawn: a guest
term counts in the budget like any other, and the host section says what CPU it ran on.

### 2.2 The rules

| # | Result | Recommendation to the owner |
|---|---|---|
| R1 | The witness fits the invocation limits, the budget p99 ≤ 50 ms, and S3 shows the book cannot live in 4 MiB | Confirm `DEC-4` |
| R2 | The witness exceeds the message limits, but the budget holds | Keep `DEC-4`, and carry the witness as a private input or in rows. Raise it with @olanod (VOS's maintainer) (Q2, Q17) |
| R3 | The budget exceeds 50 ms | `RISK-2` fallback: a smaller hot set plus a cold store. Escalate `DEP-1`. `NFR-1` is not relaxed |
| R4 | Precompile unavailable, and the budget exceeds 50 ms only without it | Record the dependency on the precompile. Raise it with @olanod and the zkpvm team |

The recommendation on the holding-position option goes to `F-004`, which decides.

## 3. Method

Raw samples: `spikes/results/<run>.csv`, one row per sample:

    spike,case,variant,size,sample,metric,value,unit

- `spike`: `s1` … `s4`; `case`: the posting shape or scenario; `variant`: store, batch or option;
  `size`: book size in accounts, input size or state size in bytes; `sample`: 0-based index after
  warm-up; `metric`: what was measured; `value`: an unsigned integer in `unit` (`ns`, `us`, `gas`,
  `bytes`).
- Summaries are nearest-rank p50, p95, p99 and max over the sorted samples, computed by
  `spikes/native` (`summarize` reproduces every row of these tables from the CSVs).
- 1 000 samples after warm-up per figure unless a table says otherwise and why. Guest **gas** is
  exactly repeatable (it does not vary between samples), so where a guest series is shorter the gas
  figure is unaffected.
- Every run checks what it times. S1 first reproduces `F-001`'s golden roots on both store variants and
  asserts every step applied and every shadow commit gives the book's root. S2 asserts every kernel
  status and the root in the guest against the host's run of the same input. S3 asserts that the
  proofs reconstruct `root_before`, that CipherClerk's `verify_transition` accepts the witness, and
  that the root after, derived from the proofs alone, equals the store's root after commit.
- How to reproduce each run: `spikes/README.md`.

**The machine was shared.** Guest runs in two containers, the 1 000 000-account generation, and S1
overlapped (and unrelated containers were up). p50s are stable across the
overlap; p95/p99 of the guest runs carry the noise, and the text says where that matters. It does
not change any conclusion below: the conclusions rest on p50s and on exactly repeatable gas.

## 4. S1 — kernel, native

`Book::apply` (F-001's binding, CipherClerk kernel, `FileStore` + `FileVault`) on a small book (16
customers in two assets), release profile, 1 000 samples per row after 50 warm-up units. A fresh book
every 250 samples keeps memory and disk bounded. `file-mem` is redb's in-memory backend (the same code
without fsync); `file-disk` is on the APFS SSD, both files fsynced on every commit.

| Store | Shape | Batch | Postings | p50 µs | p95 µs | p99 µs | max µs | p50 µs per posting | Signing p50 µs | Store commit p50 µs | Store commit p99 µs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| file-mem | transfer | 1 | 1 | 1708.3 | 2103.5 | 2352.5 | 4349.2 | 1708.3 | 133.9 | 1130.8 | 1751.7 |
| file-mem | transfer | 4 | 4 | 6203.3 | 7255.2 | 7964.9 | 20002.9 | 1550.8 | 539.0 | 4030.8 | 5695.8 |
| file-mem | transfer | 10 | 10 | 15804.5 | 18168.8 | 21756.6 | 34714.0 | 1580.5 | 1345.9 | 10446.8 | 14631.0 |
| file-mem | transfer | 64 | 64 | 101413.2 | 113803.7 | 124005.0 | 211463.5 | 1584.6 | 8565.3 | n/m | n/m |
| file-mem | reserve-new-holding | 1 | 1 | 2173.0 | 2725.2 | 2931.5 | 12357.7 | 2173.0 | 183.8 | 1498.8 | 2199.6 |
| file-mem | reserve-new-holding | 4 | 4 | 8969.7 | 11333.9 | 13064.8 | 22116.4 | 2242.4 | 739.0 | 6334.5 | 10166.9 |
| file-mem | reserve-new-holding | 10 | 10 | 23446.6 | 27339.7 | 30503.1 | 59054.3 | 2344.7 | 1850.2 | 17019.0 | 23821.0 |
| file-mem | reserve-new-holding | 64 | 64 | 147985.2 | 181816.4 | 234297.1 | 367381.6 | 2312.3 | 11779.8 | n/m | n/m |
| file-mem | reserve-pooled | 1 | 1 | 1663.3 | 2018.8 | 2354.5 | 16908.6 | 1663.3 | 133.8 | 1096.1 | 1612.9 |
| file-mem | reserve-pooled | 4 | 4 | 6059.2 | 7045.9 | 7738.3 | 11282.6 | 1514.8 | 539.9 | 3868.5 | 5272.6 |
| file-mem | reserve-pooled | 10 | 10 | 16035.2 | 18704.5 | 24726.3 | 36369.5 | 1603.5 | 1345.5 | 10663.8 | 15387.1 |
| file-mem | reserve-pooled | 64 | 64 | 100355.9 | 116505.2 | 135217.9 | 318795.6 | 1568.1 | 8542.0 | n/m | n/m |
| file-mem | capture-for-less | 1 | 1 | 2476.6 | 3050.5 | 4272.0 | 14793.3 | 2476.6 | 183.3 | 1668.5 | 2648.4 |
| file-mem | capture-for-less | 4 | 4 | 9601.8 | 11494.5 | 12311.8 | 32449.1 | 2400.4 | 734.1 | 6456.0 | 9226.0 |
| file-mem | capture-for-less | 10 | 10 | 24610.9 | 29494.3 | 35800.5 | 89764.0 | 2461.1 | 1825.0 | 17032.6 | 25612.5 |
| file-mem | capture-for-less | 64 | 64 | 161060.8 | 248974.2 | 337766.6 | 536421.4 | 2516.6 | 11663.6 | n/m | n/m |
| file-mem | conversion | 1 | 1 | 2491.5 | 2866.1 | 3184.8 | 4770.4 | 2491.5 | 268.0 | 1389.0 | 1947.2 |
| file-mem | conversion | 4 | 4 | 8777.5 | 9900.5 | 11244.3 | 21254.6 | 2194.4 | 1072.1 | 4555.6 | 6389.8 |
| file-mem | conversion | 10 | 10 | 22120.0 | 24679.8 | 30186.0 | 46011.3 | 2212.0 | 2678.0 | 11592.0 | 16111.8 |
| file-mem | conversion | 64 | 64 | 137450.5 | 152735.0 | 185880.2 | 244201.5 | 2147.7 | 17105.5 | n/m | n/m |
| file-mem | chain-2 | 1 | 2 | 3192.2 | 3803.3 | 4165.4 | 6786.5 | 1596.1 | 268.9 | 2075.8 | 2944.4 |
| file-mem | chain-10 | 1 | 10 | 15596.3 | 18290.5 | 21404.2 | 46234.5 | 1559.6 | 1344.0 | 10246.9 | 15452.3 |
| file-disk | transfer | 1 | 1 | 14938.5 | 18933.9 | 23620.0 | 101456.8 | 14938.5 | 137.0 | 10099.6 | 18045.5 |
| file-disk | transfer | 4 | 4 | 26243.0 | 50441.5 | 68834.8 | 182453.3 | 6560.8 | 540.8 | 19631.8 | 49344.4 |
| file-disk | transfer | 10 | 10 | 51679.6 | 78129.4 | 105876.5 | 460292.6 | 5168.0 | 1359.3 | 40166.2 | 72146.5 |
| file-disk | transfer | 64 | 64 | 252297.5 | 367305.0 | 464832.9 | 1317144.7 | 3942.1 | 8620.3 | n/m | n/m |
| file-disk | reserve-new-holding | 1 | 1 | 18839.0 | 62089.4 | 102156.3 | 173882.8 | 18839.0 | 190.5 | 12989.2 | 57162.8 |
| file-disk | reserve-new-holding | 4 | 4 | 36646.5 | 52192.5 | 67389.4 | 139862.8 | 9161.6 | 746.2 | 28463.3 | 48050.3 |
| file-disk | reserve-new-holding | 10 | 10 | 69121.2 | 108460.3 | 127706.3 | 977156.6 | 6912.1 | 1844.5 | 57838.9 | 113429.5 |
| file-disk | reserve-new-holding | 64 | 64 | 367743.9 | 536053.6 | 755603.4 | 1390857.3 | 5746.0 | 11780.5 | n/m | n/m |
| file-disk | reserve-pooled | 1 | 1 | 14602.5 | 35540.0 | 43916.8 | 55680.6 | 14602.5 | 135.3 | 9799.4 | 27780.7 |
| file-disk | reserve-pooled | 4 | 4 | 24989.6 | 30089.7 | 33994.8 | 43864.1 | 6247.4 | 539.8 | 18022.8 | 26079.6 |
| file-disk | reserve-pooled | 10 | 10 | 47917.3 | 65420.5 | 81943.2 | 164088.3 | 4791.7 | 1340.8 | 37718.3 | 65820.8 |
| file-disk | reserve-pooled | 64 | 64 | 233821.1 | 318181.3 | 424275.9 | 666396.5 | 3653.5 | 8470.8 | n/m | n/m |
| file-disk | capture-for-less | 1 | 1 | 18442.6 | 23783.1 | 31583.8 | 138061.0 | 18442.6 | 186.7 | 12883.5 | 22110.5 |
| file-disk | capture-for-less | 4 | 4 | 38344.3 | 53292.3 | 68967.0 | 123052.5 | 9586.1 | 730.5 | 30842.6 | 53941.3 |
| file-disk | capture-for-less | 10 | 10 | 78987.2 | 105738.2 | 124591.9 | 138501.7 | 7898.7 | 1818.0 | 66143.4 | 104972.3 |
| file-disk | capture-for-less | 64 | 64 | 514510.5 | 4077962.8 | 6066793.5 | 9690065.4 | 8039.2 | 11611.8 | n/m | n/m |
| file-disk | conversion | 1 | 1 | 16485.4 | 37377.2 | 54167.9 | 262090.1 | 16485.4 | 272.6 | 11619.6 | 36443.5 |
| file-disk | conversion | 4 | 4 | 29071.0 | 47811.1 | 69338.5 | 103318.4 | 7267.8 | 1074.2 | 20069.7 | 49742.9 |
| file-disk | conversion | 10 | 10 | 74197.7 | 132255.0 | 198062.4 | 1956917.0 | 7419.8 | 2686.4 | 57076.0 | 198263.3 |
| file-disk | conversion | 64 | 64 | 324008.5 | 468380.3 | 536217.8 | 5666191.5 | 5062.6 | 17131.6 | n/m | n/m |
| file-disk | chain-2 | 1 | 2 | 18623.0 | 40744.7 | 56440.3 | 369114.3 | 9311.5 | 272.6 | 13220.3 | 41570.0 |
| file-disk | chain-10 | 1 | 10 | 47419.1 | 60122.1 | 71159.2 | 137583.0 | 4741.9 | 1329.7 | 37150.6 | 61108.8 |

`n/m`: at batch 64 the shadow store is not run (memory and disk); the total is still measured.

Reading. In memory a posting costs about 1.6 ms (transfer, pooled reserve), 2.2–2.5 ms (reserve with
a new holding position, capture for less, conversion); batching does not change the per-posting cost.
**Two thirds of it is `FileStore`'s commit** — 128 node rows per written leaf in redb — not the kernel.
On disk every commit is two fsyncs (store and vault): a single transfer is 14.9 ms at p50 and
23.6 ms at p99; batching amortises that to 3.9 ms per posting at batch 64. The tails on disk are the
SSD's (capture for less at batch 64 has a 6 s p99 while the run shared the disk with the Linux VM).

**Split of one posting** (batch 1, `file-mem`, p50, µs). Signing and the store commit are measured
directly (the store's cost is timed by committing the same delta to a shadow `FileStore`); the three
kernel phases are the kernel's per-shape operation counts (`cipher-clerk/src/kernel/apply.rs:142-405`)
times the primitives' measured p50; the residual is the binding's own work (key discovery, loads,
vault sums, simulation).

| Shape | Measured total | Sign (measured) | Signature verification (model) | Commitment checks (model) | Negativity openings (model) | Store commit incl. root update (measured) | of which SMT hashing (model) | Binding residual |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| transfer | 1708.3 | 133.9 | 44.0 | 120.3 | 113.5 | 1130.8 | 124.3 | 165.9 |
| reserve-new-holding | 2173.0 | 183.8 | 87.9 | 120.3 | 113.5 | 1498.8 | 124.3 | 168.8 |
| reserve-pooled | 1663.3 | 133.8 | 44.0 | 120.3 | 113.5 | 1096.1 | 124.3 | 155.7 |
| capture-for-less | 2476.6 | 183.3 | 44.0 | 180.4 | 170.3 | 1668.5 | 155.4 | 230.3 |
| conversion | 2491.5 | 268.0 | 87.9 | 240.5 | 227.0 | 1389.0 | 186.5 | 279.1 |
| chain-2 | 1596.1 | 134.4 | 44.0 | 120.3 | 113.5 | 1037.9 | 124.3 | 146.1 |
| chain-10 | 1559.6 | 134.4 | 44.0 | 120.3 | 113.5 | 1024.7 | 124.3 | 122.8 |

Primitives, native, release, 1 000 samples each:

| Primitive | p50 µs | p95 µs | p99 µs | max µs |
|---|---:|---:|---:|---:|
| verify_signature | 44.0 | 56.0 | 84.0 | 88.5 |
| sign (core, DEC-3) | 36.5 | 39.8 | 43.7 | 58.8 |
| opening check (verify_reveal) | 46.9 | 50.8 | 54.9 | 140.1 |
| point decompress | 3.3 | 3.3 | 3.4 | 8.1 |
| point compress | 3.3 | 3.3 | 3.4 | 4.2 |
| point add | 0.1 | 0.1 | 0.1 | 0.3 |
| SMT leaf write (128 nodes, in memory) | 31.1 | 41.0 | 50.2 | 97.1 |

The kernel alone, on exactly the S2 inputs (a `VecLedger` of the touched leaves, no store), from
`s2prep`:

| Shape | Batch | Kernel p50 µs | p95 µs | p99 µs | max µs |
|---|---:|---:|---:|---:|---:|
| transfer | 1 | 286.1 | 294.2 | 299.5 | 342.9 |
| reserve, new holding position | 1 | 327.7 | 341.5 | 359.1 | 435.0 |
| reserve, pooled | 1 | 283.0 | 291.6 | 295.7 | 324.0 |
| capture for less | 1 | 402.3 | 410.8 | 414.1 | 422.4 |
| two-asset conversion | 1 | 567.8 | 581.0 | 587.5 | 610.5 |
| linked chain of 2 | 1 | 567.3 | 575.8 | 583.0 | 611.5 |
| linked chain of 10 | 1 | 2 796.9 | 2 850.4 | 2 870.8 | 2 949.0 |
| transfer | 4 | 1 130.1 | 1 153.9 | 1 163.3 | 1 215.7 |
| transfer | 10 | 2 849.3 | 2 900.3 | 2 919.8 | 3 409.4 |
| reserve, pooled | 4 | 1 139.0 | 1 167.0 | 1 529.1 | 2 983.0 |

These replace CipherClerk's dev-profile figures (about 480 µs for a transfer, about
177 µs per signature check): in release a transfer is about 0.29 ms of kernel time and a signature
check 44 µs. **The kernel is not what costs time natively; the store commit is** (§4 split), and on
disk the fsync is (`file-disk` rows).

## 5. S2 — kernel in the PVM guest

### Does it build and run?

| Question | Answer at `7bae926` / `e9a7b08` |
|---|---|
| Kernel, default features, for `riscv64em-vos` | **Builds and runs.** `kernel-probe.pvm` 291 219 B (`opt-level = "s"`, LTO, as the `clerk-*` actors) |
| Kernel with `signing` | **Does not build.** `getrandom v0.2.17`: `compile_error!("target is not supported…")` (`getrandom/src/lib.rs:351`), pulled by `signing = ["dep:rand_core", "rand_core/getrandom"]` (CipherClerk `Cargo.toml`). So `ledger-kernel`, which enables `signing` for `DEC-3`, cannot be built for the guest as it stands (`F-001` Open points) |
| Kernel with `pvm-precompile` | **Builds; does not run.** The shim is `zkpvm-precompiles` at VOS `9f5337f`, using ECALLs 110–114 (not 200, as earlier notes recorded). The actor **panics at its first curve operation** (status `Panicked`, 11.5 M gas used): the standard AgentRuntime's actor host-call dispatch (`vos/src/agent/execution.rs:951`) services the blake2b precompile (ECALL 100, `:1162`) and nothing in 110–114, so the call ends in `UnsupportedHostCall` (`:1231`). The host-side Ristretto handler (`vos/src/runtime.rs:286-336`) belongs to the older service runtime, not to Agent actors. **Not available on the Agent path** |
| Execution backend | Interpreter only, on every target (`pvm/runtime/src/refine.rs:10-12`) |

### Gas and time

The probe runs `apply_account_creations` + `apply_batch` exactly as `Book::apply` does, over an
in-message `VecLedger` of the touched leaves (NOTES decision 4), and reports guest gas per phase with
the `GAS` hostcall. Each invocation is a full Agent invocation through the standard runtime (Invoke,
then Acknowledge; §7). Guest gas per phase (exact; identical in every sample):

| Shape | Batch | Postings | Decode gas | Kernel apply gas | Apply gas per posting | `VecLedger` root gas | Native kernel p50 µs |
|---|---:|---:|---:|---:|---:|---:|---:|
| transfer | 1 | 1 | 84 656 | 29 228 089 | 29.2 M | 11 214 091 | 286.1 |
| reserve, new holding position | 1 | 1 | 92 850 | 34 591 857 | 34.6 M | 11 214 433 | 327.7 |
| reserve, pooled | 1 | 1 | 84 656 | 29 227 582 | 29.2 M | 11 214 091 | 283.0 |
| capture for less | 1 | 1 | 110 787 | 41 162 487 | 41.2 M | 13 023 390 | 402.3 |
| two-asset conversion | 1 | 1 | 139 967 | 58 440 839 | 58.4 M | 14 925 084 | 567.8 |
| linked chain of 2 | 1 | 2 | 129 925 | 58 440 767 | 29.2 M | 16 622 384 | 567.3 |
| linked chain of 10 | 1 | 10 | 494 333 | 292 449 485 | 29.2 M | 60 136 160 | 2 796.9 |
| transfer | 4 | 4 | 228 070 | 116 940 127 | 29.2 M | 27 653 802 | 1 130.1 |
| transfer | 10 | 10 | 514 942 | 292 487 844 | 29.2 M | 60 381 413 | 2 849.3 |
| reserve, pooled | 4 | 4 | 228 070 | 116 932 557 | 29.2 M | 27 527 569 | 1 139.0 |

The root phase is the probe's own check over its small `VecLedger` (the core would not recompute a
root this way in the guest; S3 is the root work the design does). Kernel gas is linear in postings:
about 29.2 M per two-entry posting, whatever the batching.

Times, kernel invocation against a no-op invocation of the same actor with a payload of the same size
(1 000 kernel and 200 no-op invocations for the three budget shapes):

| Shape | Invoke p50 ms | p95 | p99 | No-op invoke p50 ms | p99 | Invoke + ack (caller) p50 ms | p99 | Runtime gas per invoke |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| transfer | 442.6 | 570.6 | 677.3 | 395.6 | 726.0 | 470.2 | 742.5 | 263.1 M |
| reserve, new holding position | 450.4 | 620.0 | 705.8 | 374.9 | 436.1 | 478.5 | 777.3 | 268.6 M |
| reserve, pooled | 440.9 | 483.2 | 527.9 | 375.3 | 444.4 | 468.6 | 564.3 | 263.1 M |

The other shapes (100 invocations each; their gas is exact, their time is the same runtime at the
same speed):

| Shape | Batch | Message B | Invoke p50 ms | p95 | p99 | Actor gas |
|---|---:|---:|---:|---:|---:|---:|
| capture for less | 1 | 2 233 | 459.1 | 481.6 | 493.8 | 54.5 M |
| two-asset conversion | 1 | 2 873 | 488.5 | 505.0 | 531.4 | 73.8 M |
| linked chain of 2 | 1 | 2 753 | 492.1 | 510.8 | 521.2 | 75.4 M |
| linked chain of 10 | 1 | 11 521 | 942.0 | 979.8 | 1 018.4 | 353.5 M |
| transfer | 4 | 5 041 | 604.7 | 643.2 | 702.4 | 145.1 M |
| transfer | 10 | 11 809 | 947.3 | 978.4 | 998.4 | 353.9 M |
| reserve, pooled | 4 | 5 041 | 611.1 | 640.4 | 654.1 | 145.0 M |

The `pvm-precompile` build panics on the first transfer (11.5 M gas used) and on the first reserve
with a creation (0.6 M gas used: the registrar signature is its first curve operation); 3 attempts
each, all the same.

Reading. On this host the standard runtime runs at about **590 M gas/s** (a no-op invocation: 222.6 M
gas in 374.9 ms). A reservation's kernel work is 34.6 M gas (new holding position) or 29.2 M gas
(pool): **about 58 ms and 49 ms of interpreter time for the kernel alone**, against 0.33 ms and 0.28 ms
natively — a factor of about 175. CipherClerk estimated 10–20 M cycles per transfer without the
precompile; the measured figure is 29.2 M gas. Batch 64 was not run: at about 1.2 KB
per posting it is about 75 KB, over the 16 KiB invocation message; a batch of 10 transfers is 11.8 KB,
and about 13 would fit.

## 6. S3 — witnessed-leaves invocation

### The books

Seeded (seed 1), through `Book::apply`: N accounts created and funded, then N transfers with Zipf
(s = 1) senders over a seeded permutation and uniform receivers. So each book has N accounts, 2N
transfers and 2N external ids.

| Accounts | Store | Leaves | Generation wall time | Root |
|---:|---|---:|---:|---|
| 10 000 | `FileStore`, on disk | 50 003 | 180 s | `8de03ac9…2da67` |
| 10 000 | `IndexStore` (same seed) | 50 003 | 13 s | `8de03ac9…2da67` (equal) |
| 100 000 | `IndexStore` | 500 003 | 148 s | `0de75a62…01afa` |
| 1 000 000 | `IndexStore` | 5 000 003 | 1 781 s | `2ebcbf6c…8b607` |

The two 10 000-account books have the same root, and their witnesses are byte-identical: the spike's
store and `FileStore` hold the same book. `FileStore` itself cannot hold the larger books on this
host: its 10 000-account file is 1 077 940 224 B (912 265 216 B after compaction), **about 18 KB per
leaf**, because every leaf write stores 128 node rows (NOTES decision 7).

### Witness size, build, verify-and-apply, root after

1 000 samples per timing. Build: load the touched leaves, extract the six frontiers from the index,
serialise. Verify-and-apply: decode the bytes as CipherClerk's `SuccinctTransitionWitness` and run its
`verify_transition` (for the new holding position, also the registrar signature and the two root
derivations of the creation). Root after: CipherClerk's `BatchProof::root` over the post-state leaves.

| Accounts | Case | Witness B | Frontier nodes (acc/xfer/jrnl/eid) | Build p50 / p99 µs | Verify+apply p50 / p99 µs | Root after p50 / p99 µs |
|---:|---|---:|---|---:|---:|---:|
| 10 000 | transfer | 4 312 | 22/14/1/17 | 207.5 / 222.9 | 825.7 / 860.3 | 179.0 / 191.5 |
| 10 000 | reserve, pooled | 4 512 | 24/17/1/16 | 242.3 / 258.0 | 830.1 / 867.5 | 179.2 / 190.0 |
| 10 000 | reserve, new holding | 4 384 | 19/13/0/14 | 155.4 / 325.8 | 1 317.0 / 1 380.9 | 197.3 / 211.0 |
| 100 000 | transfer | 5 112 | 31/18/1/20 | 186.8 / 238.5 | 825.5 / 948.2 | 179.5 / 328.9 |
| 100 000 | reserve, pooled | 5 008 | 31/18/1/18 | 171.4 / 300.0 | 825.7 / 971.4 | 174.7 / 207.6 |
| 100 000 | reserve, new holding | 5 384 | 32/17/0/17 | 171.9 / 468.2 | 1 321.7 / 1 490.0 | 197.1 / 208.5 |
| 1 000 000 | transfer | 5 712 | 36/23/1/22 | 192.8 / 232.5 | 830.0 / 899.7 | 179.9 / 194.8 |
| 1 000 000 | reserve, pooled | 5 408 | 31/23/1/21 | 205.3 / 239.8 | 826.7 / 875.8 | 178.6 / 191.1 |
| 1 000 000 | reserve, new holding | 5 880 | 36/20/0/20 | 134.6 / 156.3 | 1 333.3 / 1 387.8 | 197.9 / 212.0 |
| 10 000 | capture for less (`IndexStore`) | 5 440 | 35/15/1/14 | 242.6 / 278.6 | 950.6 / 1 055.9 | 190.6 / 222.7 |
| 100 000 | capture for less | 6 240 | 44/18/1/18 | 183.6 / 211.8 | 954.6 / 1 365.8 | 186.0 / 217.6 |
| 1 000 000 | capture for less | 6 936 | 50/23/1/21 | 256.6 / 1 080.5 | 1 010.2 / 2 786.2 | 199.2 / 342.3 |

(10 000 on `FileStore` except the capture, 100 000 and 1 000 000 on `IndexStore`. The capture for
less — three entries with `CLOSING_DEBIT`, of a reservation into a new holding position — was added
at `F-004`'s request, from a separate run of the same seeded books, `--capture-only`. Voided and pending sub-trees are empty
in these books, so their frontiers are empty.) Of the 1 000 000-account transfer witness's 5 712 B,
the proofs are 4 152 B, the event 504 B, the openings 296 B. The reserve with a new holding position
also carries its 384 B signed creation.

The witness grows with log₂ of the sub-tree sizes — about 800 B (1.5 KB for the capture, which
touches three accounts) from 10 000 to 1 000 000 accounts — and **every shape fits the 16 KiB
invocation message at 1 000 000 accounts, the largest (capture for less, 6 936 B) with 9 KB to
spare**. Verify-and-apply
is independent of book size natively (about 0.83 ms; 1.33 ms with the creation).

### In the guest

The same witness bytes, sent to `kernel-probe`'s `verify(witness, roots)` in an Agent invocation, which
decodes them as `SuccinctTransitionWitness` and runs `verify_transition` (1 000 invocations at
1 000 000 accounts, 100 at the smaller sizes; gas is exact). The capture rows ran after the others.

| Accounts | Case | Message B | Decode gas | `verify_transition` gas | Invoke p50 ms | p95 | p99 | Invoke + ack (caller) p50 / p99 ms |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 10 000 | transfer | 4 441 | 239 011 | 87 651 281 | 536.7 | 556.8 | 569.2 | 566.8 / 604.2 |
| 10 000 | reserve, pooled | 4 641 | 252 069 | 87 813 651 | 531.1 | 555.2 | 569.6 | 561.1 / 600.1 |
| 10 000 | reserve, new holding (chain part) | 4 129 | 223 713 | 93 257 753 | 530.7 | 557.5 | 575.4 | 558.5 / 610.9 |
| 100 000 | transfer | 5 241 | 286 432 | 88 002 566 | 527.7 | 548.9 | 556.1 | 556.1 / 593.5 |
| 100 000 | reserve, pooled | 5 137 | 280 720 | 87 989 658 | 523.7 | 542.3 | 548.7 | 551.8 / 585.0 |
| 100 000 | reserve, new holding (chain part) | 5 129 | 282 114 | 93 746 214 | 540.4 | 552.9 | 578.6 | 569.3 / 607.4 |
| 1 000 000 | transfer | 5 841 | 322 148 | 88 220 225 | 541.5 | 576.6 | 621.5 | 573.0 / 675.9 |
| 1 000 000 | reserve, pooled | 5 537 | 304 029 | 87 819 812 | 538.4 | 641.4 | 675.6 | 569.5 / 714.2 |
| 1 000 000 | reserve, new holding (chain part) | 5 625 | 312 220 | 94 019 105 | 553.4 | 643.7 | 683.7 | 584.6 / 722.5 |
| 10 000 | capture for less | 5 569 | 298 390 | 106 004 645 | 552.6 | 573.5 | 592.1 | 582.9 / 622.6 |
| 100 000 | capture for less | 6 369 | 345 162 | 106 344 621 | 557.0 | 578.2 | 594.6 | 587.4 / 633.1 |
| 1 000 000 | capture for less | 7 065 | 385 110 | 106 334 613 | 565.3 | 683.4 | 748.8 | 596.1 / 787.2 |

For the new holding position the guest runs the chain part only; the creation beside it (registrar
signature and two root derivations) is about 5.4 M gas more — the difference between the two reserve
options in S2's apply phase.

**Verifying a witness costs about 88 M gas (94 M with a new holding position, 106 M for a capture
for less), independent of book size: about 150–180 ms of interpreter time on this host**, against
0.83–1.33 ms natively. It is three
times the kernel's apply (29.2 M gas) because `verify_transition` also rebuilds the six sub-roots
from the proofs before and after the batch, in software hashing inside the guest; the split between
the two was not measured.

### Point read of one account leaf

1 000 reads per row, Zipf-hot and uniform accounts.

| Accounts | Store | Zipf p50 / p95 / p99 / max µs | Uniform p50 / p95 / p99 / max µs |
|---:|---|---|---|
| 10 000 | `FileStore` on disk (leaf + node check) | 3.1 / 1 118.1 / 1 497.7 / 3 813.6 | 3.3 / 766.6 / 1 125.8 / 7 189.6 |
| 10 000 | leaf-only redb table (leaf + hash check) | 1.3 / 3.5 / 6.5 / 57.8 | 1.3 / 3.3 / 4.8 / 12.0 |
| 100 000 | leaf-only redb table | 2.4 / 4.8 / 6.9 / 41.2 | 2.5 / 4.3 / 6.3 / 28.0 |
| 1 000 000 | leaf-only redb table | 150.7 / 314.2 / 395.2 / 718.8 | 128.5 / 268.5 / 323.5 / 682.3 |

A first number toward `NFR-3` (5 ms p99 at 150 M entries), not a test of it: the 1 000 000-account
table (539 MB) no longer sat in the page cache of a machine whose memory the Linux VM held, and the
10 000-account `FileStore` file (1 GB) shows the same effect earlier. Both stay under 5 ms at p99.

### Growth at `NFR-2`'s volume, 12 months

`NFR-2`: 2.5 M operations and 5 M postings a month, so 60 M postings a year. Each posting adds one
transfer leaf and one external-id leaf. The per-operation holding option also adds one account leaf per
reserving operation; nothing deletes it. Upper bound: every operation reserves, 30 M a year.

| | Pooled | Per-operation | Difference |
|---|---:|---:|---:|
| New leaves in 12 months | 120 M | 150 M | +30 M account leaves |
| Leaf content only (transfer ≈ 500 B, external id 33 B, account 313 B) | 32 GB | 41 GB | +9.4 GB (+29 %) |
| As `FileStore` stores them (18.2 KB per leaf, measured) | 2.2 TB | 2.7 TB | +0.55 TB |
| Account leaves as a leaf-only table (539 B each at 1 M, measured) | — | +16 GB | |

`FileStore`'s layout is not a candidate at this volume; `F-006` needs a node representation that does
not store a row per level for single-leaf subtrees (the spike's index keeps only branching nodes).

### Hot state and the 4 MiB image

| Item | Size |
|---|---|
| Composite root and six sub-roots | 224 B |
| One open reservation in an index (operation reference, holding or pool id, payer, asset, amount, deadline, state) | ≈ 100 B |
| One authority's bound counters (`F-003`) | ≈ 64 B (count not known yet) |
| Account leaves that fit in 4 MiB (313 B each, no history, no nodes) | **13 400** (PLAN estimated about 12 000) |

Open reservations at `NFR-2`'s mean rate (0.96 operations a second), if every operation reserves:

| Mean time open | Open at once | Index size |
|---|---:|---:|
| 1 minute | 58 | 6 KB |
| 1 hour | 3 500 | 0.35 MB |
| 1 day | 83 000 | 8.3 MB |
| 7 days | 580 000 | 58 MB |

So an open-reservation index fits 4 MiB only if reservations stay open less than about 12 hours on
average (1.2 hours at a tenfold peak). And fitting is not the constraint that binds first: §7 shows
every byte of Agent state is carried through every invocation.

## 7. S4 — one Local Agent invocation

### Through the runtime ABI (1 000 invocations per size)

The bundled standard AgentRuntime (`vosx/blobs/agent_runtime.pvm`), driven as the Local host's journal
driver drives it (`vos/src/agent/local_journal_driver.rs:3325-3364`; same outer gas, same preparation
cache), on a Local-profile Agent with the `state-blob` actor (program 65 831 B). `touch` rewrites 64
bytes of one 32 KiB row. Each invocation is `Invoke` then `Acknowledge`; both are whole-state
round trips. Fields are the driver's own debug fields; "caller" is both executions plus encode and
decode, in process, without the host's journal write.

| Agent state B | Invoke input B | Invoke gas | Invoke p50 / p95 / p99 ms | Ack gas | Ack p50 / p95 / p99 ms | Caller p50 / p95 / p99 / max ms |
|---:|---:|---:|---:|---:|---:|---:|
| 70 557 | 141 514 | 99.5 M | 139.7 / 168.9 / 201.1 | 43.1 M | 49.2 / 58.7 / 73.1 | 190.0 / 227.1 / 260.8 / 327.4 |
| 1 057 302 | 1 126 908 | 346.6 M | 520.7 / 570.8 / 621.0 | 252.3 M | 409.1 / 445.7 / 482.1 | 932.9 / 1 006.9 / 1 095.5 / 1 396.7 |
| 2 147 805 | 2 218 762 | 823.0 M | 1 158.8 / 1 303.1 / 1 402.9 | 664.6 M | 1 015.2 / 1 130.3 / 1 229.4 | 2 178.9 / 2 463.9 / 2 652.4 / 2 796.0 |
| 4 121 295 | 4 189 357 | 1 282.8 M | 1 898.3 / 2 422.0 / 2 678.6 | 1 060.4 M | 1 711.8 / 2 154.5 / 2 402.9 | 3 620.4 / 4 620.8 / 4 984.0 / 6 559.2 |

(1 000 invocations per row. The fill of each state and the retained results it leaves are described in
NOTES decisions 12–13. At an empty Agent, 4 582 B of state, 3 invocations: 111 ms and 78 M gas for the
Invoke, 24 ms for the Acknowledge. `preparation_us` — loading the prepared runtime — is 2–4 ms at p50
at every size; the rest is execution.)

**Fixed cost and cost per byte.** A least-squares fit over the four sizes: the Invoke costs
**84 M gas + 300 gas per byte of Agent state**, the Acknowledge **28 M gas + 258 gas per byte**, and the
caller sees **135 ms + 0.86 µs per byte — about 0.9 s per MiB** on this host (about 620–880 M gas/s).
At the caller the per-byte term passes the fixed one at about 160 KB of state. At the 4 MiB cap one invocation costs
3.6 s at p50, 5.0 s at p99. The branch says whole-state transport is the dominant, unfixed cost;
this puts a number on it, and it holds in the Acknowledge as much as in the Invoke.

The fixed part depends on the actor, too: the same no-op path costs 78 M gas for this 66 KB actor
and 223 M gas for the 291 KB `kernel-probe` (§5), at near-empty state. On two points that is about
650 gas per byte of actor program per invocation. The runtime loads a fresh inner machine for the
actor on every invocation.

### The retention cap (a finding that shapes every Agent design)

On one Agent, with no reset: the **33rd acknowledgement is refused** (`ResultCapacity`) and the **65th
invocation is refused** (`ResultCapacity`). The standard runtime keeps at most 32 positive
acknowledgements and 32 unacknowledged results per result lane (`vos/src/agent/standard.rs:23-29`) and
evicts neither: "Positive facts are never evicted: a full component rejects the next acknowledgement"
(`:24-27`, enforced at `:4493-4501` and `:4700-4706`). No compaction path for invocation
acknowledgements was found in `standard.rs`. As driven through the ABI, **a Local Agent stops taking
Linear invocations after 64 of them.** The harness therefore restores the post-setup state every
window of invocations (NOTES decision 13). Whether the Local host compacts this somewhere outside the
runtime was not established: the `vosx` run below failed first.

### Through `vosx` (the canonical path)

A Space up in the Linux VM, a Local Agent created (18.3 s) and `state-blob` installed (25.5 s) with
`vosx`, two fills, then `vosx space invoke-local --intent` with canonical ATQ1 intents written by
`vos-host atq1`, the daemon at `RUST_LOG=vos::agent=debug`.

| | n | p50 | p95 | max |
|---|---:|---:|---:|---:|
| Caller wall time per `invoke-local` (authorise, prepare, deliver, retire) | 12 | 22.5 s | 36.8 s | 36.8 s |
| Supervisor dispatch `service_us` | 43 | 279 ms | 522 ms | 675 ms |
| Supervisor dispatch `queue_wait_us` | 43 | 0.19 ms | 2.0 ms | 6.2 ms |
| Runtime executions on the ≈ 1.1 MB system Agent (`input_bytes` > 500 KB) | 141 | mean 1.50 s, 0.82 G gas each | | |
| Runtime executions on the application Agent and small states | 1 069 | mean 24 ms | | |

About nine executions of the system Authority Agent's whole ≈ 1.1 MB state per application invocation
account for most of each invocation's 18–37 s. The **13th** invocation's authorisation returned HTTP 503
(daemon: `native operation authorization incomplete error=Unavailable`); every later intent was refused
client-side (`RequestConflict`, a reservation pending), and `--resume` then failed with
`CrossStoreMismatch` / `CorruptResidue`. Not investigated further: 12 samples, not 1 000, and the
reason is the failure.

## 8. The rule applied, and the recommendation on `DEC-4`

### The terms

Following §2.1: each term at its p99, the reserve term at the worse `DEC-2` option (the new holding
position), S3 at 1 000 000 accounts, S4 at the design's hot-state size. An Agent invocation has a fixed
part (the runtime loading the actor and carrying the state) and a variable part (the actor's work), so
the three terms are taken as follows, to avoid counting the fixed part three times:

| Term | What is counted | p99 |
|---|---|---:|
| S2 — reserve, in the guest | The kernel's gas for the reserve (34.6 M) at the runtime's p99 speed (no-op invocation p99: 222.6 M gas in 436.1 ms, 510 M gas/s) | 67.7 ms |
| S3 — verify-and-apply at 1 000 000 accounts, in the guest | The witness verification's gas for the reserve with a new holding position at 1 000 000 accounts (94.0 M in the guest, plus about 5.4 M for the creation beside it), at the same p99 speed | 194.6 ms |
| S4 — one invocation at the hot-state size | The design's hot state is not known yet: it depends on how long reservations stay open (§6), from a few KB to beyond 4 MiB. So the term is taken at its floor — one invoke + acknowledge of an actor **with the kernel linked**, at near-empty state: the no-op invocation of `kernel-probe` (291 KB program), caller p99. Choice 4 asked for the first measured size at or above the hot state; the floor can only understate the term, and the budget fails anyway. Each 100 KB of hot state adds about 86 ms (§7) | 475.5 ms |
| **Budget** | Sum of the p99s | **737.8 ms** |

Choice 1's re-check on joint samples has nothing to run on: no measured invocation does the reserve
and the verification together. Cutting it differently changes nothing. The p50s alone give
58.2 + 167.3 + 402.5 = 628.0 ms, and each of the three terms exceeds 50 ms on its own at p50: **the
kernel's reserve in the interpreter (58 ms)**, **the witness verification (167 ms)** and **one no-op
invocation of an actor that links the kernel (403 ms at the caller, 375 ms of runtime execution)**.

### Which rule

- **R1** needs the witness to fit (it does: 5 880 B ≤ 16 KiB at 1 000 000 accounts), the book not to fit
  in 4 MiB (it does not: 13 400 account leaves at most, §6) **and** the budget p99 ≤ 50 ms. The budget
  fails by more than an order of magnitude. **R1 does not apply.**
- **R2** needs the budget to hold. **Does not apply.**
- **R4** needs the budget to exceed 50 ms *only* without the precompile. The precompile is not available
  on the Agent path (§5), but even with the kernel's curve arithmetic taken to zero the fixed invocation
  cost (≈ 0.4–0.5 s p99) exceeds 50 ms nine times over. **R4 does not apply**, though its finding is
  recorded (§5, §10).
- **R3 applies: the budget exceeds 50 ms.** The pre-stated recommendation is the `RISK-2` fallback (a
  smaller hot set plus a cold store), escalating `DEP-1`, with `NFR-1` not relaxed.

### The recommendation, and what the numbers add to it

**`DEC-4` is not confirmed. By the pre-stated rule its outcome is R3.** Stated plainly, the numbers
also show that R3's prescribed fallback is not enough on its own:

1. **The half of `DEC-4` that is about where the leaves live is supported.** The book cannot live in
   Agent state (13 400 accounts in 4 MiB; the 32-acknowledgement cap; every byte of state costs about
   300 gas in every Invoke and 258 more in its Acknowledge). The witnessed-leaves
   shape works at 1 000 000 accounts: 5.4–5.9 KB, CipherClerk's own verifier accepts it, the root after
   comes from the proofs alone, and natively it costs under 1.4 ms.
2. **The half that puts the reservation decision inside a VOS `saga` Agent invocation fails `NFR-1`,
   and not because of state size.** The dominant costs are the runtime's fixed cost per invocation,
   which grows with the actor's program size (≈ 78 M gas for a 66 KB actor at empty state, ≈ 223 M gas
   for a 291 KB actor), and interpreted curve arithmetic (≈ 29–35 M gas per posting, plus 88–94 M gas to
   verify a witness). A smaller hot set, which is what `RISK-2`'s fallback changes, removes neither. On
   `saga` at `7bae926` the Agent path is interpreter-only on every target, so a faster host shortens it
   by its clock ratio, not by the orders of magnitude missing.
3. **So the ruling the owner is asked for is not only "`RISK-2` fallback".** It is one of:
   - **(a)** keep `DEC-4` and make `NFR-1` depend on upstream work (`DEP-1` escalated, and the asks in
     §10: a cheaper invocation, the precompile on the Agent path, an acknowledgement retention that
     compacts, and native or recompiled execution for Agents), with `F-005`/`F-006` blocked on it; or
   - **(b)** take the reservation decision out of the Agent: the library in a native process
     (`DEC-1`'s "library first"), where the kernel costs 0.3 ms and a durable commit about 15 ms at p50
     on this laptop (§4), and VOS used off the decision path. That changes `DEC-4` and `F-005`'s scope
     and is a `PLAN.md` change, not this feature's to make.

   The measurements do not choose between (a) and (b); they show that (a) is not a matter of tuning.
4. `NFR-1` is not relaxed by anything here. `DEP-1` is escalated either way.

The owner's ruling on `DEC-4` is **not recorded yet**: this document is the input to it. The `M-0` gate
closes when it is.

## 9. Holding-position numbers for `F-004`

`F-004` decides; these are its inputs. "Per-operation" is a reserve into a new holding position (one
creation + two entries); "pooled" is a reserve into one holding position per asset (two entries).

| | Pooled | Per-operation | Difference |
|---|---:|---:|---:|
| Kernel alone, native (S2 input), p50 | 283.0 µs | 327.7 µs | +44.7 µs (+16 %) |
| `Book::apply`, `file-mem`, batch 1, p50 (S1) | 1 663.3 µs | 2 173.0 µs | +509.7 µs (+31 %), most of it the store (+403 µs) |
| Kernel in the guest, apply phase | 29 227 582 gas | 34 591 857 gas | +5.36 M gas (+18 %), ≈ +9–10 ms |
| Witness at 1 000 000 accounts | 5 408 B | 5 880 B (incl. 384 B creation) | +472 B |
| Verify-and-apply, native, p50 | 826.7 µs | 1 333.3 µs | +0.5 ms (registrar signature, creation roots) |
| Verify-and-apply in the guest | 87 819 812 gas | 94 019 105 gas, + ≈ 5.4 M for the creation | +11.6 M gas (+13 %), ≈ 20 ms |
| Capture for less (per-operation; three entries, `CLOSING_DEBIT`): witness at 1 000 000 accounts | | 6 936 B | fits 16 KiB |
| Capture for less: verify-and-apply, native p50 / in the guest | | 1 010.2 µs / 106 334 613 gas | |
| New leaves per year at `NFR-2` (upper bound) | 0 | 30 M account leaves | +9.4 GB content, +0.55 TB as `FileStore` stores it |
| Leaf the witness proves for the holding position | the pool's (present, shared) | the new one's (absent) | |

Not measured, and relevant to the choice: under the pooled option every reservation touches the
same account leaf, so no two reservations in one unit are independent of each other in the kernel;
and matching a capture to its reservation (`INV-14`) moves from the holding account's identity to the
library's own index, which then lives in hot state (§6).

## 10. For @olanod (Q2, Q9, Q15, Q17)

A short summary of the upstream questions. It answers part of Q9 and gives Q2, Q15 and Q17 numbers.

> We measured `saga/agents` at `7bae926` on an Apple M2 Pro (Linux `aarch64` in a VM for the Local
> Agent path), with CipherClerk `e9a7b08`. Everything is reproducible from `spikes/` in our repository.
>
> **Q9 — one invocation.** Driving the bundled `agent_runtime.pvm` through the runtime ABI as the
> Local journal driver does, one Linear invocation plus its acknowledgement costs, at p50: 190 ms at
> 70 KB of Agent state and 933 ms at 1 MB, 2.18 s at 2 MB and 3.62 s at 3.9 MB (a 66 KB actor). The
> runtime spends about 250–300 gas per byte of state in the Invoke and about as much again in the
> Acknowledge, at 600–850 M gas/s on this host. With CipherClerk linked (a 291 KB actor) a no-op invocation costs
> 223 M gas, 375 ms at near-empty state: the fixed cost grows with the actor's program size. Through
> `vosx space invoke-local` the same invocation took 18–37 s (12 samples), most of it in about nine
> executions of the system Authority Agent's whole ~1.1 MB state (1.5 s each); the 13th authorisation
> returned 503 (`authorization incomplete error=Unavailable`) and `--resume` then reported
> `CrossStoreMismatch` / `CorruptResidue`. Is there a plan for touched-state execution, for
> recompiled or native execution of Agents (the Agent path is interpreter-only, `refine.rs:10-12`),
> and for keeping the Authority's cost off every invocation?
>
> **Q2 — state.** The acknowledgement cap binds before the 4 MiB one: 32 positive acknowledgements and
> 32 results per lane, never evicted (`standard.rs:23-29`), so as we drive it an Agent refuses its 33rd
> acknowledgement and its 65th Linear invocation. Is there a compaction path we missed? For the book we
> plan roots-in, leaves-out: CipherClerk's succinct witness is 5.4–5.9 KB at a million accounts, well
> inside the 16 KiB message (Q17: we did not need a larger private input for that).
>
> **Precompile.** `pvm-precompile` builds (ECALLs 110–114, VOS `9f5337f`) but the Agent runtime's actor
> host-call dispatch does not service them (`execution.rs:951`, only blake2b at `:1162`), so the actor
> panics at its first curve operation. Is servicing them on the Agent path planned? Interpreted, a
> reservation's kernel work is about 35 M gas (≈ 60 ms) and verifying its witness about 90 M gas
> (≈ 150–185 ms).
>
> **Q15 — signing.** CipherClerk's `signing` pulls `getrandom 0.2`, which refuses to compile for
> `riscv64em-vos`. We would like the randomness source to be a parameter (our nonces are derived
> deterministically anyway); would you take that patch?

## 11. Re-check on `2bb7876` (2026-09-23)

Three questions bear on §8 (Q2 and Q9): whether recent work on `saga/agents` addresses the Agent
state-size limit; whether the Agent path uses the PVM recompiler, which would explain the slowness;
and whether outbound cross-Agent calls, timers, chain observation and a randomness source for actors
are available. This section reviews the 25 commits between `7bae926`
and `2bb7876` (`saga/agents`, 2026-09-23) against each finding above, re-runs S2, S3 and S4 in the
guest on the new tip by the same method, and measures the new experimental state path as well.
Earlier sections are left as they were, apart from a pointer in the status line; where §7 needs a
correction, it is made here (§11.4).

**Outcome: rule R3 still applies.** On the path §8 measured, the new tip gives the same kernel gas to
the unit, the same runtime gas within 0.2 %, and the same times within noise. The budget is **666 ms at p99** (737.8 ms at `7bae926`); the
difference is a quieter machine, not VOS. The new state work is real, but it is experimental, not
selected by any released path, and it does not touch the terms that fail the budget. None of the six
asks in §8 is met; one is met in part, behind a feature flag.

### 11.1 What the 25 commits are

All 25 are by one author on 2026-09-23 (`git log 7bae926..2bb7876`). The first, `b2457dd` (55 files,
+31 063/−6 178), lands a **bounded external-state workstream**: actor rows move out of the whole
runtime image into an authenticated, content-addressed block tree that the runtime guest reads block by
block through a new host call. The other 24 build an "external Local" journal owner around it: signed
Create lifecycle, retained slots, authenticated replay, retry and response recovery, and read-only
inspection of retained outcomes.

All of it sits behind the cargo feature `experimental-state-blocks`. The bundled runtime,
`vosx/blobs/agent_runtime.pvm`, is byte-identical (SHA-256 `d6e77981…fe91ddfe`, runtime source
`2ccfacb`, ABI r19, `support/production-artifacts.toml:9-30`), and `Cargo.lock` is unchanged. VOS's own
plan says so: "No released startup, lifecycle queue or route adapter selects this owner yet"
(`docs/agent-saga-status.md:117`), "Production dispatch still selects the legacy reader" (`:236`),
"Runtime limits, bundled artifacts and production behavior are unchanged" (`:1838`). The candidate
runtime implements Create, Install, ACK, non-yielding Invoke for non-Merge actors and read-only
inspection; other operations trap (`services/agent-runtime/src/lib.rs:8-11`).

### 11.2 Item by item

File and line references are at `2bb7876`.

| Item | Finding at `7bae926` | At `2bb7876` | Changed? |
|---|---|---|---|
| (a) Whole-state transport, state limits | Every Invoke and Acknowledge carries the whole Agent state; 4 MiB image; ≈ 300 + 258 gas per byte | **Released path: unchanged.** The work is still built from the whole `RuntimeState` (`vos/src/agent/replay.rs:609-639`, now shared by `local_journal_driver.rs:1767` and `:3789`). `MAX_RUNTIME_STATE_BYTES` is still 4 MiB (`vos-agent-sdk/src/lib.rs:73`, `vos/src/agent/execution.rs:52`) and a lane still holds at most 16 384 rows (`standard.rs:42`, `actor_storage.rs:32`). **Experimental path: new.** Rows live in a block tree under signed per-lane limits (`ExternalStateResourceLimits`, `vos-agent-sdk/src/contract.rs:81-84`; the fixture grants 1 M rows / 1 GiB), blocks ≤ 64 KiB (`state_blocks.rs:16`), values ≤ 1 MiB in chunks (`state_tree.rs:28`). The guest fetches blocks through host call `0x180` (`state_blocks.rs:31`; `vos/src/agent/state_block_pvm.rs:286-331`), at a "provisional" 100 + 1 gas per byte (`:305-307`). The runtime's own metadata (results, acknowledgements, directory) is still one opaque component, ≤ 3 MiB (`state_execution.rs:31`), reloaded whole on every execution | Released: no. Experimental: rows yes, metadata no |
| (b) Retention cap | 32 acknowledgements and 32 results per lane, never evicted; 33rd ACK and 65th invocation refused | Unchanged: `standard.rs:23-29`; refusals at `:4714-4721` and `:4921-4926`. Re-run: the 33rd acknowledgement and the 65th invocation are refused, as before. The experimental path runs the same `StandardAgentRuntime`, so the same cap applies there (the harness needs the same window reset). The new "at most 32 outcomes per domain" (`docs/agent-saga-status.md:97-98`) is a host-side response cache for retries, not this | No |
| (c) Interpreter-only Agent execution | `pvm/runtime/src/refine.rs:10-12` | `refine.rs` is untouched; the doc comment still says Refine execution "always uses the interpreter — never the kernel or the recompiler — on every target, which is the cross-machine determinism guarantee". The only change in `refine_host.rs` is `run_with_host` (`:119`), which lets an embedder service extra host calls (the block fetch) without leaving the interpreter. The recompiler is selectable only on the older kernel path, through `PvmBackend` and the `GREY_PVM` variable (`pvm/runtime/src/backend.rs:11-18`, `:74-104`); no Agent code constructs that path. Every Agent execution goes through `RefineContext::load` or `load_prepared` (`local_journal_driver.rs:1219-1247`, `runtime_pvm.rs:61`, `state_block_pvm.rs:368`), and inner actor machines are interpreters too (`pvm/runtime/src/inner.rs`, untouched). **There is no switch that was left off**: interpreter-only is the design, stated as the determinism guarantee. The recompiler is also compiled only for `x86_64` Linux (`backend.rs:53`), so on this `aarch64` host it could not be measured even on the kernel path. Only the interpreter was measured | No |
| (d) Curve precompile ECALLs 110–114 | Not serviced by the Agent actor dispatch; actor panics at first curve op | Unchanged. The actor host-call dispatch is `execution.rs:951-1237`: `GAS`, `ACTOR_EFFECT_EXPORT`, `STORAGE_R`, `PREIMAGE_LOOKUP`, `FETCH`, `GROW_HEAP`, BLAKE2b (`:1169`), `DEBUG_WRITE`, and `UnsupportedHostCall` for anything else (`:1237`). The experimental runner services only `0x180` (`state_block_pvm.rs:287-289`). Re-run: the `pvm-precompile` build panics at the first transfer (11.46 M gas used) and at the first reserve with a creation (0.61 M gas used), as before | No |
| (e) Fixed cost of loading the actor | The runtime loads a fresh inner machine for the actor on every invocation; ≈ 650 gas per byte of actor program | Unchanged on the released path: the outer runtime is predecoded once per executor (`local_journal_driver.rs:1211-1247`), the actor is not; `inner.rs` is untouched. The same kernel-probe, rebuilt against `2bb7876`, is 554 B smaller and its no-op Invoke costs 0.36 M gas less, i.e. ≈ 650 gas per byte again. **The experimental path is worse**: it cold-loads the 1.79 MB runtime on every execution (`state_block_pvm.rs:368`, `RefineContext::load`, no preparation cache), 77 ms at p50 for the Invoke and 77 ms again for the Acknowledge (§11.5) | Released: no. Experimental: worse |
| (f) The Authority Agent on every canonical invocation | ≈ 9 executions of the ≈ 1.1 MB Authority state per `vosx invoke-local` | No change. The one edit to the issuer (`clean_authority_issuer.rs:1460-1502`) shares its acknowledgement path with the external Create. Attributing Authority cost is the plan's next batch, not done: "Measured performance. Attribute credential/Authority, queue, VM, quorum, persistence and retirement costs" (`docs/agent-saga-status.md:32-38`). The `vosx` run of §7 was not repeated: its path is unchanged, a sample takes 18–37 s, and it failed at the 13th | No |
| (g) Outbound cross-Agent calls | The r19 ABI cannot express one | Unchanged: `RuntimeOutcome` has the same four variants (`vos-agent-sdk/src/runtime.rs:1057-1070`) and `YieldReason::Await { call }` still has no destination or payload (`:840-843`). Matches @olanod: missing for now | No |
| (g) Timers | Durable timer model, nothing fires it | `vos-agent-sdk/src/scheduling.rs` untouched | No |
| (g) Randomness for actors | — | None on the Agent path, at either commit. The service ABI's `BOOT_CONTEXT` seam (`vos/src/abi/hostcall.rs:113-122`) is non-deterministic, Local-only, and not in the Agent dispatch above; nor is `NOW_MS` (`:124-132`). Consistent with @olanod's remark that one existed and may have been lost in the refactor. CipherClerk's `signing` still cannot build for the guest (§5) | No |
| (g) Private-input limit | — | Unchanged. The Agent path has no private-input channel of its own; its bounds are the 16 KiB message and reply (`vos-agent-sdk/src/runtime.rs:14-15`) and the caller's content-addressed availability (`execution.rs:33-34`). `ACTOR_PRIVATE_INPUT_MAX_BYTES` (64 KiB, `vos/src/service/mod.rs:207`) belongs to the service runtime. Our witness is ≤ 7 KB (§6), so this does not bind us | No |
| (h) Chain observation | Pull-only light-client queries | `extensions/` is untouched. The substrate extension still exposes `status`, `query`, `query_map` over finalized snapshots and transaction submission (`extensions/substrate/src/lib.rs:513-650`), with no subscription. An Agent actor cannot reach it at all: `NATIVE_EXTENSION_INVOKE` (`hostcall.rs:85`) is not in the Agent dispatch. Consistent with @olanod's remark: sube can observe, but the extension does not expose it | No |

### 11.3 The released path, re-run

Same harness (`spikes/vos-host`), same inputs, same sample counts, same Linux VM on the same laptop,
two containers in parallel as before. The only change to the harness is one manifest field that
`2bb7876` added (`external_state_limits: None`, the r19 tag the bundled runtime carries); `run.sh`
inserts it only when the mounted VOS has it. The guest actors were rebuilt against `2bb7876`.

**The guest's kernel gas is the same to the unit, and the runtime's gas per invocation agrees within
0.2 %.** The kernel's phases in the guest are identical (reserve with a new holding position:
decode 92 850, apply 34 591 857, root 11 214 433 gas; transfer: 84 656 / 29 228 089 / 11 214 091), and
so is `verify_transition` at 1 000 000 accounts (88 220 225; 94 019 105 for the new holding position).
S4's Agent states are byte-identical (70 557, 1 057 302, 2 147 805, 4 121 295 B), and their Invoke
and Acknowledge gas agrees within 0.02 %.
The actors themselves differ by a few hundred bytes (`kernel-probe` 290 665 B, was 291 219), which
moves actor gas by ≈ 100 and the runtime's gas by ≈ 0.36 M.

S2, kernel in the guest (1 000 kernel and 200 no-op invocations per shape):

| Shape | Invoke p50 / p95 / p99 ms, `7bae926` | `2bb7876` | No-op invoke p50 / p99, `7bae926` | `2bb7876` | Invoke + ack (caller) p50 / p99, `7bae926` | `2bb7876` | Runtime gas per invoke, `2bb7876` |
|---|---:|---:|---:|---:|---:|---:|---:|
| transfer | 442.6 / 570.6 / 677.3 | 437.7 / 473.4 / 528.4 | 395.6 / 726.0 | 371.3 / 465.1 | 470.2 / 742.5 | 464.5 / 564.2 | 262.7 M |
| reserve, new holding position | 450.4 / 620.0 / 705.8 | 444.9 / 474.5 / 507.0 | 374.9 / 436.1 | 370.2 / 396.1 | 478.5 / 777.3 | 471.9 / 540.2 | 268.2 M |
| reserve, pooled | 440.9 / 483.2 / 527.9 | 435.8 / 458.7 / 484.2 | 375.3 / 444.4 | 368.5 / 394.3 | 468.6 / 564.3 | 462.4 / 518.6 | 262.8 M |

S3, the witness verified in the guest at 1 000 000 accounts (1 000 invocations each):

| Case | Invoke p50 / p95 / p99 ms, `7bae926` | `2bb7876` | Invoke + ack p50 / p99, `7bae926` | `2bb7876` | `verify_transition` gas (both) |
|---|---:|---:|---:|---:|---:|
| transfer | 541.5 / 576.6 / 621.5 | 533.5 / 557.3 / 582.0 | 573.0 / 675.9 | 564.2 / 619.5 | 88 220 225 |
| reserve, pooled | 538.4 / 641.4 / 675.6 | 531.3 / 551.7 / 584.4 | 569.5 / 714.2 | 561.3 / 616.8 | 87 819 812 |
| reserve, new holding (chain part) | 553.4 / 643.7 / 683.7 | 540.2 / 558.3 / 590.7 | 584.6 / 722.5 | 570.1 / 624.6 | 94 019 105 |

S4, one invocation against Agent state (1 000 invocations per size):

| Agent state B | Invoke gas p50, `2bb7876` | Invoke p50 / p99 ms, `7bae926` | `2bb7876` | Ack gas p50, `2bb7876` | Caller p50 / p99 ms, `7bae926` | `2bb7876` |
|---:|---:|---:|---:|---:|---:|---:|
| 70 557 | 99.5 M | 139.7 / 201.1 | 138.2 / 195.6 | 43.1 M | 190.0 / 260.8 | 186.6 / 267.0 |
| 1 057 302 | 346.6 M | 520.7 / 621.0 | 514.0 / 585.4 | 252.3 M | 932.9 / 1 095.5 | 921.7 / 1 028.7 |
| 2 147 805 | 822.9 M | 1 158.8 / 1 402.9 | 1 159.2 / 1 226.9 | 664.5 M | 2 178.9 / 2 652.4 | 2 171.1 / 2 270.1 |
| 4 121 295 | 1 282.9 M | 1 898.3 / 2 678.6 | 1 894.0 / 1 992.4 | 1 060.6 M | 3 620.4 / 4 984.0 | 3 624.6 / 3 791.1 |

The p50s agree within 2.5 %, except the transfer no-op (6 %), the noisiest series of §5. The p99s are
lower on `2bb7876` throughout: §3 recorded that the first runs shared the machine with the
1 000 000-account generation and S1; these did not (unrelated containers were up both times).
That is the machine, not VOS: the gas did not move.

### 11.4 A correction to §7: retained results, not only bytes

§7 fitted the Invoke at 84 M gas + 300 gas per byte and the Acknowledge at 28 M + 258, over four states.
Those four states did not differ only in size. Filling 2 and 4 MiB takes more invocations than the 32
acknowledgements allow, so the harness left 31 results unacknowledged at those two sizes and none at
the other two (§7, "NOTES decisions 12–13"). A control run on `2bb7876` separates the two: the same
four sizes, each with 31 unacknowledged results retained before measuring (`F002_RETAIN=31`,
100 invocations per size; the first row is §11.3's run, for reference):

| Rows written | Agent state B | Retained results | Invoke gas | Ack gas | Caller p50 / p99 ms |
|---:|---:|---:|---:|---:|---:|
| 64 KiB | 70 557 | 0 | 99.5 M | 43.1 M | 186.6 / 267.0 |
| 64 KiB | 113 275 | 31 | 271.9 M | 205.8 M | 622.2 / 658.0 |
| 1 MiB | 1 100 020 | 31 | 538.7 M | 428.1 M | 1 365.9 / 1 615.7 |
| 2 MiB | 2 147 805 | 31 | 822.8 M | 664.4 M | 2 133.6 / 2 305.8 |
| ≈ 3.9 MiB | 4 121 295 | 31 | 1 282.9 M | 1 060.6 M | 3 648.1 / 3 761.5 |

At constant retention the fit is **Invoke 258 M + 252 gas per byte, Acknowledge 191 M + 213 gas per
byte** — the bytes cost about 16 % less than §7 said. The rest of §7's slope is the retained results:
at near-empty state, 31 of them add 172 M gas to every Invoke and 163 M to every Acknowledge, while
adding only 43 KB of state. That is **about 5 M gas per retained result in each execution**. The likely
cause is that restoring the runtime re-checks each retained result's authorisation, an Ed25519
signature, on every execution (`standard.rs:1837-1862` → `verify_clean_accepted_authorization`,
`:2732-2764`); this was not profiled. It was already there at `7bae926` (`standard.rs` only gained
feature-gated functions).

What changes. §7's conclusion stands: at the caller the per-byte term passes the fixed one at a few
hundred KB, and whole-state transport is the dominant cost of a large Agent. But a second term joins
it: **an Agent's fixed cost grows by ≈ 10 M gas per invoke-plus-acknowledge for every result it has not
acknowledged yet**, up to 32. An Agent that acknowledges late pays for it on every invocation. §8 is
unaffected: its S4 term is a no-op invocation with nothing retained.

### 11.5 The experimental external-state path

Nothing in VOS's released code selects this path (§11.1), so it was measured through VOS's own test
fixture: `Fixture` in `vos/src/agent/state_block_pvm_lifecycle_tests.rs`, which creates a Local Agent
under the candidate standard runtime (`services/agent-runtime` built with `experimental-state-blocks`,
1.79 MB), then stages every candidate's blocks into an in-memory journal store. A measurement test
added to that file (`spikes/vos-external/f002-external-state.patch`) installs the same signed
`state-blob` and `kernel-probe` packages and runs S4, S2 and S3 exactly as `vos-host` does: Invoke then
Acknowledge, the same messages, the same authorisation shape, the same reset windows. The timed span is
`MultiLaneStateBlockHost::execute_admitted_work` (encode, load, run with block fetches, decode and
check), as `vos-host` times the image runtime; staging the returned blocks into the store is timed
apart and left out, as `vos-host` leaves out the journal write. Test-only instrumentation in
`run_block_program` records gas, load and run time.

**Rows no longer travel.** S4 through the external path (1 000 invocations per size):

| Rows written | Retained results | Work sent to the runtime | Invoke gas | Ack gas | Invoke load / run p50 ms | Caller p50 / p99 ms |
|---:|---:|---:|---:|---:|---:|---:|
| 64 KiB | 0 | 74 191 B | 166.7 M | 39.2 M | 76.9 / 250.5 | 440.5 / 493.2 |
| 1 MiB | 0 | 74 191 B | 172.6 M | 42.4 M | 76.5 / 256.6 | 449.8 / 492.3 |
| 2 MiB | 31 | 74 191 B | 390.7 M | 223.5 M | 77.3 / 534.1 | 976.4 / 1 049.0 |
| ≈ 3.9 MiB | 31 | 74 191 B | 396.2 M | 227.2 M | 77.0 / 540.6 | 986.7 / 1 038.0 |

The work handed to the runtime is 74 KB at every size (the actor program travels in it; the state
passed in is 4.4 KB of control and 0.6 KB in the lane fields). With 31 results retained at every size (100
invocations per size), Invoke gas goes 382.1 → 388.1 → 389.5 → 395.3 M from 64 KiB to ≈ 3.9 MiB of rows:
**about 3 gas per byte of rows, against 252 on the image path.** Touched-state transport works for
actor rows.

**But the fixed cost is higher, not lower.** Three things the external path adds or keeps:

1. **The runtime is cold-loaded on every execution**: 77 ms at p50 for the Invoke and 77 ms again for
   the Acknowledge, against 2–4 ms on the image path, which keeps a predecoded runtime per executor.
   `state_block_pvm.rs:368` calls `RefineContext::load` with no preparation cache. This one is cheap to
   fix; it is not a design limit.
2. **More gas per invocation, growing with the actor's size**: +67 M gas on the 66 KB `state-blob`
   (166.7 M against 99.5 M) and **+190 M on the 291 KB `kernel-probe`** (S3 below: 506.0 M against
   315.3 M). Where it goes was not profiled.
3. **The runtime metadata is still whole, and retained results still cost ≈ 7 M gas each per Invoke**
   (382.1 − 166.7 M over 31, at 64 KiB). The metadata is capped at 3 MiB.

S2 and S3 through the external path:

| Case | Actor gas (same as image path) | Invoke runtime gas | Invoke load / run p50 ms | Invoke + ack (caller) p50 / p99 ms | Image path, caller p50 / p99 ms |
|---|---:|---:|---:|---:|---:|
| S2 transfer | 40 744 557 | 449.8 M | 76.5 / 775.7 | 969.2 / 1 058.2 | 464.5 / 564.2 |
| S2 reserve, new holding position | 46 118 868 | 455.3 M | 77.2 / 790.9 | 986.2 / 1 047.0 | 471.9 / 540.2 |
| S2 reserve, pooled | 40 744 050 | 449.8 M | 77.4 / 790.2 | 985.7 / 1 043.6 | 462.4 / 518.6 |
| S2 no-op, `kernel-probe` (reserve-sized payload) | 201 184 | 409.5 M | 77.4 / 720.4 | 916.9 / 977.8 | 396.7 / 427.4 |
| S3 transfer, 1 000 000 accounts (100 invocations) | 88 858 619 | 506.0 M | 77.0 / 880.5 | 1 084.9 / 1 134.2 | 564.2 / 619.5 |
| S3 reserve, new holding (100 invocations) | 94 642 529 | 511.4 M | 76.9 / 890.9 | 1 093.7 / 1 168.9 | 570.1 / 624.6 |

### 11.6 The budget now

Same rule, same method as §8: each term at its p99; S2 and S3 as their gas at the runtime's p99 speed,
taken from the no-op invocation of `kernel-probe` with the reserve-sized payload; S4 as that no-op
invocation's invoke-plus-acknowledge at the caller.

| Term | `7bae926` | `2bb7876`, released path | `2bb7876`, experimental external path |
|---|---:|---:|---:|
| Runtime p99 speed (no-op Invoke gas ÷ p99 time) | 222.7 M in 436.1 ms: 510.6 M gas/s | 222.3 M in 396.1 ms: 561.3 M gas/s | 409.5 M in 860.0 ms: 476.1 M gas/s |
| S2 — reserve, new holding position (34.59 M gas) | 67.7 ms | 61.6 ms | 72.7 ms |
| S3 — verify at 1 000 000 accounts, new holding (94.02 M + 5.36 M gas) | 194.6 ms | 177.1 ms | 208.7 ms |
| S4 — no-op invocation of an actor that links the kernel, caller p99 | 475.5 ms | 427.4 ms | 977.8 ms |
| **Budget, sum of p99s** | **737.8 ms** | **666.1 ms** | **1 259.2 ms** |
| Sum of p50s | 628.0 ms | 619.8 ms | 1 178.5 ms |

**Against 50 ms, the released path is over by a factor of 13, and every term alone still exceeds 50
ms.** The experimental path is slower on every term. R3 applies, by the rule of §2.2, and the choice
§8 put to the owner — (a) keep `DEC-4` and depend on upstream work, or (b) take the decision out of
the Agent — is unchanged.

What it would take, in numbers that do not depend on this host: one reservation decision on the image
path is ≈ 386 M gas of guest work (the no-op Invoke and Acknowledge of a kernel-linked actor, 222.3 M +
29.6 M; the reserve, 34.6 M; the verification, 99.4 M). To fit 50 ms at p99 that needs ≈ 7.7 G gas/s,
**about 14 times this host's interpreter at its p99 speed**, and that is before the Authority round
trip and the durable journal write, neither of which is in these numbers (§7). A recompiler would
have to deliver that factor on this workload, or the fixed cost would have to shrink, or both.

### 11.7 The six asks

| Ask (§8 (a)) | Status at `2bb7876` |
|---|---|
| A cheaper fixed invocation | **Not met.** Released path unchanged (no-op of a kernel-linked actor: 222 M gas, 427 ms p99 at the caller). Experimental path: higher (409 M gas for the no-op Invoke, 978 ms p99 at the caller) |
| The curve precompile on the Agent path | **Not met.** Dispatch unchanged; the precompile build still panics at its first curve operation |
| Acknowledgement and result retention that compacts | **Not met.** Same caps, same refusals. And each unacknowledged result now has a measured price: ≈ 5 M gas per execution (§11.4) |
| Recompiled or native execution for Agents | **Not met, and not a misconfiguration.** Interpreter-only is documented as the determinism guarantee (`refine.rs:10-12`); the recompiler belongs to a different path and to `x86_64` Linux only. @olanod said he will check; the code says it is by design, so a change is a design change, not a flag |
| Touched-state transport | **Met in part, experimentally.** Actor rows are fetched by path (≈ 3 gas per byte of rows instead of 252). Runtime metadata still travels whole, and no released path selects this runtime. It does not help the budget: our design already keeps Agent state small (roots in, leaves out, §8) |
| The Authority Agent off every invocation | **Not met.** Not addressed; VOS plans to attribute that cost next (`docs/agent-saga-status.md:32-38`) |

### 11.8 For @olanod

A summary to go with §10:

> We re-ran F-002 on `saga/agents` at `2bb7876`. The released Agent path is unchanged: same kernel gas
> to the unit, same times within noise (budget 666 ms at p99 against 50 ms). On the recompiler: the Agent
> path goes through `RefineContext`, which documents interpreter-only as its determinism guarantee
> (`pvm/runtime/src/refine.rs:10-12`); the recompiler is only reachable through `PvmBackend` on the
> kernel path (`backend.rs:74-104`) and only on `x86_64` Linux, so we found no switch that is off.
> We measured the `experimental-state-blocks` runtime through your lifecycle fixture: rows are
> transported by path (≈ 3 gas per byte of rows instead of ≈ 250), which is the state-size fix working.
> Its fixed cost is higher, though: it cold-loads the 1.79 MB runtime on every execution
> (`state_block_pvm.rs:368`, 77 ms for the Invoke and again for the ACK, versus the image path's
> preparation cache), and it adds ≈ 67 M gas per invocation for a 66 KB actor and ≈ 190 M for a 291 KB
> one. Separately, each unacknowledged retained result costs ≈ 5 M gas in every execution (≈ 7 M
> in the external path's Invoke), which looks like the per-result authorisation check in `StandardAgentRuntime::restore`
> (`standard.rs:1837-1862`). Numbers and the patch that measures them are in `spikes/`.

### 11.9 Reproducing

`spikes/README.md`, "Re-check on `2bb7876`". Raw samples: `spikes/results/*-2bb7876.csv` (`s4x`, `s2x`
and `s3x` are the external path). Host as §1, same day; the runs went in two containers at once, as in
§3, without S1 or book generation beside them. The external-path S3 rows are 100 invocations each; its
S2 rows are 1 000 and 200, like the released path's.

## 12. F-025 changes (2026-09-24)

The first two upstream changes of `F-025` (`DEC-16`, issue #66), measured before and after by the
method of §7 and §11. Each is one branch on aledgent's VOS fork from `saga/agents` at `2bb7876`; the pull
request drafts are `proposals/vos/0001` and `0002`.

| Change | Branch | Commit |
|---|---|---|
| `T-025-01` Compact acknowledged results | `agent/compact-acknowledged-results` | `de0f12f8` |
| `T-025-02` Curve precompile ECALLs 110–114 on the Agent path | `agent/curve-precompile-dispatch` | `5f5929dc` |

**Outcome.** `T-025-01` removes the functional blocker: an Agent took **10 000 acknowledged Linear
invocations** where it refused the 65th, and an Agent that acknowledges each result no longer pays the
retained-result term. `T-025-02` makes the precompile build run on the Agent path, but on the
interpreter it is **slower** than the software arithmetic, so it does not help `NFR-1`. **Neither
changes the budget of §11.6 (666 ms at p99)**: its three terms are the fixed invocation cost, the
kernel's interpreted reserve and the interpreted witness verification, and neither change touches
them. What the budget needs is still the fixed-cost cache (`T-025-03`) and native or compiled
execution (`T-025-06`).

### 12.1 How it was run

Host as §1, one run at a time, nothing else in the Linux VM. "Before" is the standard runtime built
from `2bb7876` with the same pipeline as "after" (`cargo actor` in `services/agent-runtime`, then
`link_elf_spi`); that build is not byte-identical to the bundled `agent_runtime.pvm`, which is pinned
to the older runtime source `2ccfacb`, but its gas agrees with §11.3 to within 0.1 %. Guest packages
were rebuilt against the curve branch. Commands: `spikes/README.md`, "F-025 changes". Raw samples:
`spikes/results/*-f025.csv` and `spikes/results/logs/*-f025.log`. Sample counts are smaller than §7
(200 per row, 100 for the retained-results control, 20 for the S3 diagnostic): gas is exact, and the
times agree with §11.3's 1 000-sample rows within a few per cent.

### 12.2 `T-025-01`: compaction

The change: opt-in *sequenced* invocation identities (a fixed prefix, the result component and a
position). When a component's 32 acknowledgements are full, the fact at the component's next position
becomes a per-component watermark, and every sequenced identity at or below it is refused as
divergent forever. Unsequenced identities, the 32 retained facts per component and every existing
image are unchanged. The guarantees argument is in `proposals/vos/0001`.

Capacity (`vos-host retention`, one Agent, `state-blob`, Invoke then Acknowledge each time, no reset):

| Runtime | Result |
|---|---|
| before | acknowledgement 33 refused (`ResultCapacity`), invocation 65 refused |
| after, sequenced identities | 10 000 acknowledged invocations after the fill, none refused; Agent state 12 015 B from the 1 000th to the 10 000th |

S4, one invocation against Agent state (200 invocations per size; p50 gas; caller p50 / p99 ms).
Before, the harness can only fill 2 and 4 MiB by leaving 31 results unacknowledged (§11.4); after,
every fill is acknowledged, no result is retained, and the Linear component holds 32 facts.

| Rows | Before: retained | Invoke gas | Ack gas | Caller | After: retained | Invoke gas | Ack gas | Caller |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 64 KiB | 0 | 99.6 M | 43.2 M | 189.1 / 210.0 | 0 | 102.0 M | 45.5 M | 192.7 / 219.3 |
| 1 MiB | 0 | 346.6 M | 252.4 M | 930.4 / 968.3 | 0 | 348.4 M | 254.1 M | 930.3 / 955.8 |
| 2 MiB | 31 | 823.0 M | 664.7 M | 2 182.5 / 2 243.7 | 0 | 612.6 M | 477.6 M | 1 737.7 / 1 786.6 |
| ≈ 3.9 MiB | 31 | 1 283.0 M | 1 060.7 M | 3 637.4 / 3 722.4 | 0 | 1 111.0 M | 898.7 M | 3 163.3 / 3 250.3 |

**The per-result term.** The control (before, 31 unacknowledged results at 64 KiB, 100 invocations):
271.6 M gas per Invoke, 205.6 M per Acknowledge, 622.0 / 653.3 ms at the caller. That is **5.5 M and
5.2 M gas per retained result per execution**, as §11.4 found. The term is still in the code (it is
most likely the receipt re-verification in the hostile-state restore, which the change leaves), but a
caller that acknowledges every result no longer has to leave any retained: at 2 and 4 MiB the change
saves 210 M and 172 M gas per Invoke, 187 M and 162 M per Acknowledge, 20 % and 13 % at the caller.
Holding 32 facts costs about 2.4 M gas per execution (the 64 KiB row).

Tests in VOS's own suite: three new tests (10 000 sequenced invocations with checkpoint/reopen every
1 000, gaps and foreign identities, the encoding section). No regression in `vos --lib` (1 781 passed;
the same 7 fail on unmodified `2bb7876` for environmental reasons), the compiled-runtime gates, the
`experimental-state-blocks` suites or the SDK.

### 12.3 `T-025-02`: the curve precompile

The change: the standard runtime's actor host-call loop serves ECALLs 110–114 as its own guest code,
as it serves BLAKE2b, byte-for-byte the shims' host fallback, and charges each operation to the
actor's gas at its measured interpreted cost plus about 15 % (`curve-bench`: scalar mult 3.92 M, point
add 449 k, wide reduction 12.1 k, scalar mul 26.3 k, scalar add 22.8 k gas).

**A correction to §5.** §5 read the precompile build's panic as `UnsupportedHostCall` in the Agent
dispatch. It is earlier than that: CipherClerk `e9a7b08` pins its shim to `zkpvm-precompiles` at VOS
`9f5337f`, which emits a bare `ecall`, and today's PVM compiler turns an unmarked `ecall` into a trap
(`pvm/compiler/src/riscv.rs:316-328`). The pinned build can never reach any dispatch. The measurements
here build the probe against the current shim (`pvm/proof/precompiles`), a one-line CipherClerk change
(`spikes/guest/build-precompile-current.sh`). Against the unmodified runtime that build then stops at
`UnsupportedHostCall(112)`, which is the finding §5 meant.

| Probe | Before | After |
|---|---|---|
| plain | runs | runs, same gas to the unit |
| precompile, S2 | `UnsupportedHostCall(112)` | runs, kernel root correct |
| precompile, S3 at 1 000 000 accounts | same | `OutOfGas` at the stock per-invocation host-call and BLAKE2b budgets (the precompile build also sends every BLAKE2b block through ECALL 100); runs with both raised 16× in a throwaway build |

Cost ("kernel runtime gas" is the Invoke's runtime gas minus the same actor's no-op Invoke; the
precompile actor is 17.8 KB smaller, so its no-op is 11.8 M gas cheaper):

| Shape | Plain, before: Invoke / kernel runtime gas / caller p50, p99 ms | Precompile, after: Invoke / kernel runtime gas / caller p50, p99 ms |
|---|---:|---:|
| S2 transfer | 262.8 M / 40.6 M / 440.1, 489.6 | 269.9 M / 59.5 M / 453.1, 515.7 |
| S2 reserve, new holding position | 268.2 M / 45.9 M / 448.1, 485.9 | 278.9 M / 68.4 M / 464.4, 504.2 |
| S2 reserve, pooled | 262.7 M / 40.6 M / 442.6, 474.9 | 269.9 M / 59.5 M / 435.1, 460.6 |
| S3 transfer | 315.3 M / — / 545.0, 609.3 | 385.7 M / — / 577.6, 589.4 (raised budgets, 20 samples) |
| S3 reserve, new holding | 320.8 M / — / 578.7, 617.5 | 399.4 M / — / 592.4, 623.9 (raised budgets, 20 samples) |

**Reading.** On the interpreter the precompile path costs 40–50 % more kernel gas in S2 and about
22 % more Invoke gas in S3. The software path multiplies by the basepoint with a precomputed table;
the shim makes every multiplication variable-base, decompresses and recompresses at every call, and
adds a runtime round trip. The expected effect in `F-025`'s table ("curve work becomes native,
roughly 0.3 ms plus 1.4 ms") assumed a native handler. The Agent runtime has none by design: its only
outer host calls are the standard machine calls, and the precompile AIR does not constrain the curve
relation, so a native path is a design decision for @olanod (`proposals/vos/0002`, last section), or it
comes with compiled Agent execution (`T-025-06`). **Until then aledgent should keep CipherClerk's
software arithmetic on the Agent path.**

## 13. F-025 changes 3–4 (2026-09-24)

Two more upstream changes of `F-025` (`DEC-16`, issue #66), measured before and after by the method of
§7, §11 and §12. Each is one branch on aledgent's VOS fork from `saga/agents` at `2bb7876` (still the tip
on 2026-09-24). The pull request drafts are `proposals/vos/0004` and `0003`.

| Change | Branch | Commit |
|---|---|---|
| `T-025-03` Load the actor in place; keep its preparation across invocations | `agent/prepared-actor-cache` | `5424ea5b` |
| Actor-declared program, heap and stack within the owner's ceilings (`F-005`'s `0003`) | `agent/actor-declared-limits` | `491f4306` |

**Outcome.**

- **`T-025-03`** removes about 27 % of what the actor program costs per invocation (630 → 462
  runtime gas per byte), plus the host's native predecode of the actor. Transitions stay
  byte-identical and gas is the same cold or warm.
- **For the core's reservation** (`F-005`) that is 1 487 → 1 136 M gas and **2 677 → 2 360 ms at
  p99**.
- **§11.6's budget** goes from 670 to **603 ms** at p99 on this run.
- **What is left of the program's cost is three BLAKE2b passes** (447 gas per byte). The runtime
  cannot drop them without a design change, set out for @olanod in `0004`.
- **The declared-limits change** lets the core install and run on a runtime with no constant
  raised by hand.
- **`NFR-1` is still missed by about 47 times** on the reservation path.

### 13.1 How it was run

Host as §1, one run at a time, in the Linux VM. The parallel ZK-verifier build was running on the
laptop; the load average was 5–9 during the runs. Before and after were interleaved, twice, and
the rounds agree within 2 % at p50. "Before" is the standard runtime built from `2bb7876` by the
same pipeline as "after" (§12.1), and the harness is built against the checkout under test, so
"after" includes the host-side cache. The guest packages are those of §7 (`state-blob` 66 469 B,
`kernel-probe` 291 219 B). The core is `F-005`'s full guest, 1 968 379 B of program, driven by
`aledgent-core-host measure` exactly as `MEASUREMENTS-F005.md` §1. Its "before" tree is F-005's
own: `de0f12f8` with both program limits at 2 560 KiB. Commands: `spikes/README.md`, "F-025
changes 3–4". Raw samples: `spikes/results/*-f025b.csv`, logs and profiles in
`spikes/results/logs/*-f025b.*`. Gas is exact, and the same in both rounds to the unit.

### 13.2 Where the fixed cost went (`T-025-03`)

§7 found about 650 gas per byte of actor program and guessed at "a fresh inner machine". A gas
profile of one Invoke settles it (`vos-host profile`: every instruction's gas by counter, mapped to
the runtime ELF's functions). It is not the machine. The `machine` host call costs 112 gas per KiB;
the runtime's own guest code does the rest. Per byte of program, from the difference between the
two actors:

| Where | Gas per byte |
|---|---:|
| BLAKE2b, three passes: the public-IO hash of the whole work, the availability reference, the ProgramId | 447 |
| Re-packing the code bitmask (`compact_code`, inlined in `ActorMachine::load_at`) | 76 |
| Expanding the bitmask to a byte per code byte (`parse_compact_code_blob`) | 71 |
| Copies (`memcpy`) | 35 |
| **Total** | **≈ 630** |

A BLAKE2b compression costs about 19 000 gas: the runtime is built for `riscv64em` (16
registers), the hash's 32 working words spill, and the gas model charges 25 cycles per memory
access. The host adds native work outside the gas: predecoding a fresh inner machine takes 3.5 ms
for `state-blob`, 18.0 ms for `kernel-probe` and **95.9 ms for the core**, per invocation
(`prep-bench`, p50).

The change validates the program in place and hands `machine` the program's own compact code
blob, byte-for-byte what the old path rebuilt. Execution borrows the program instead of cloning
it. The host keeps each actor's predecoded preparation on the runtime's preparation. The hashing
stays: the runtime must authenticate bytes the host hands it, and it commits its public IO from
inside the guest. The guarantees argument is in `proposals/vos/0004`. The runtime's transitions
are byte-identical to the base runtime's: `vos-host compare` ran 63 executions per actor on both
runtimes in lockstep, with identical output bytes.

**Runtime gas per Invoke** (p50; the actor's own gas is unchanged in every row):

| Actor | Program | Before | After | Change |
|---|---:|---:|---:|---:|
| `state-blob`, S4 at 64 KiB | 66 469 B | 99.54 M | 87.22 M | −12.32 M |
| `kernel-probe`, no-op | 291 219 B | 222.69 M | 171.05 M | −51.64 M |
| `kernel-probe`, reserve with a new holding position | 291 219 B | 268.60 M | 217.00 M | −51.60 M |
| The core, one reservation | 1 968 379 B | 1 487.06 M | 1 135.63 M | −351.42 M |

**Time** (ms, p50 / p99; 400 samples, the core 52):

| Measurement | Before | After |
|---|---:|---:|
| `state-blob` S4, Invoke + Acknowledge at the caller | 186.4 / 208.0 | 169.8 / 185.0 |
| `kernel-probe` no-op, Invoke + Acknowledge at the caller | 399.1 / 431.8 | 347.8 / 365.1 |
| `kernel-probe` reserve, Invoke + Acknowledge at the caller | 473.5 / 501.8 | 422.0 / 446.6 |
| The core's reservation, the driver's whole round | 2 544.8 / 2 677.0 | 2 262.5 / 2 360.3 |
| … of which the Invoke | 2 507.2 / 2 607.3 | 2 221.6 / 2 319.9 |

Time falls less than gas: the loops removed ran faster than their gas says. The host cache alone is
about 24 ms of the no-op's 51 ms. That was measured by running the "after" runtime under the
"before" harness, which has no host cache: 346.0 against 322.8 ms at p50. For the core the
predecode it saves is about 96 ms.

### 13.3 The budget of §11.6, re-applied

| Term | §11.6 (`2bb7876`) | This run, before | After `T-025-03` |
|---|---:|---:|---:|
| S2 — reserve, new holding position | 61.6 ms | 61.4 ms | 61.4 ms |
| S3 — verify at 1 000 000 accounts, new holding | 177.1 ms | 176.5 ms | 176.5 ms |
| S4 — no-op of `kernel-probe`, caller p99 | 427.4 ms | 431.8 ms | 365.1 ms |
| **Budget, sum of p99s** | **666.1 ms** | **669.7 ms** | **603.0 ms** |

S2 and S3 are the kernel's own gas, which the change does not touch. The kernel's share of an Invoke
is 74 ms at p50 before and after. §11.6's conversion divides the kernel's gas by the no-op's gas
rate, and that rate falls after the change (563 → 501 M gas/s), because the gas removed was
cheaper per millisecond than the rest. Converted that way the budget reads 632.4 ms; the kernel
did not get slower.

### 13.4 Declared program, heap and stack (`0003`)

The change lets an actor package declare what its program already fixes: length, heap pages and
stack. A new actor manifest tag carries the declaration. An owner admits an install under
ceilings, and re-admission on replay applies none. Actors choose a larger allocator arena with a
`vos` feature, and declare a stack with a linker symbol instead of padding their read-only data.
The design and its guarantees are in `proposals/vos/0003`. It departs from the draft `F-005`
carries in one respect: the owner's ceiling is admission policy, not a runtime rule. A runtime
rule needs an ABI change, and that is left to @olanod.

**The hard maximum was measured, and it is lower than the draft's 2 560 KiB.** On the released
runtime every Invoke decodes the whole work and then copies and re-encodes the program several
times, all inside the runtime's 32 MiB arena. At the state ceiling a large program exhausts it
and the runtime guest panics. The probe filled the state to 4 121 295 B with 31 unacknowledged
results, the harness's worst case, using `state-blob` with its program padded by unreachable
`trap`s (`vos-host pad-package`):

| Program | Invoke gas at 64 KiB | At the state ceiling, this branch alone | With `0004` |
|---:|---:|---|---|
| 1 536 KiB | 1.16 G | runs, 2.35 G | — |
| 1 792 KiB | 1.35 G | runs, 2.53 G | — |
| 1 920 KiB | 1.44 G | runs, 2.62 G | — |
| 2 048 KiB | 1.53 G | **runtime panics** at 4 054 287 B of state | — |
| 2 560 KiB | 1.90 G | **runtime panics** at 3 853 263 B of state | runs, 2.44 G |

The branch sets the hard maximum at **1 792 KiB**, at least 128 KiB below the first failure.
With `0004` the load no longer copies the program, and 2 560 KiB runs at the ceiling. Raising
the constant is a follow-up once both are merged.

**The core.** `F-005`'s full guest (1 968 379 B) is above 1 792 KiB, so its declaration is
invalid and the package is refused. The same library with its stack declared
(`__vos_stack_size = 0x40000`, read-only data at the standard base) and the 4 MiB heap feature
is **1 771 777 B**: 196 602 B of zero padding gone. It is declared and admitted, and **runs all
26 of `F-005`'s reservations** on this branch, a runtime with no hand-raised limit. Its Invoke
gas is 1 394.9 M against 1 487.1 M for the padded build (p50), and the actor's own gas is
unchanged. The run used the compaction branch plus this change, because `F-005`'s host needs
sequenced identities.

### 13.5 The reservation against `NFR-1`

| | Gas per Invoke | p99 of the driver's round |
|---|---:|---:|
| `F-005` as measured (fork, limits raised by hand) | 1 487 M | 2 677 ms |
| With `T-025-03` | 1 136 M | 2 360 ms |
| With `T-025-03` and the core's declared stack (`0003`), estimated | ≈ 1 045 M | ≈ 2.2 s |

The rest of the Invoke is the actor's own work (173 M), the runtime's other work (about 80 M) and
the three BLAKE2b passes over the program (about 880 M for the 1.97 MB core). The design options
for the passes are in `proposals/vos/0004`, and every one of them is @olanod's call:

- **A.** Commit the public IO to availability by reference.
- **B.** Identify the program by its verified reference.
- **C.** Take the program out of the work and look it up by hash, verified natively by the
  executor.
- **D.** A BLAKE2b written for this gas model.
- **E.** Compiled Agent execution.

Converting gas at this run's rate (about 510 M gas per second for the core's Invoke):

- A and B together take the reservation to about 1.2 s.
- C takes it to about 0.55 s, where the actor's own interpreted work and the Acknowledge
  dominate.
- 50 ms then needs roughly a tenfold faster execution: `T-025-06`, compiled Agent execution,
  designed with @olanod first.

**The reservation is still about 47 times over `NFR-1`.** `T-025-03` was expected to take "most of
the fixed cost". It took a quarter of it, because most of the cost turned out to be hashing, not
machine construction.

### 13.6 Reproducing

`spikes/README.md`, "F-025 changes 3–4". Raw samples: `spikes/results/*-f025b.csv`; the core's
logs, the symbolized profiles and the probe's output in `spikes/results/logs/*-f025b.*`.

## 14. F-025 change 5: recompiled Agent execution (2026-09-24)

Branch `agent/recompiled-agent-execution` (`3140c3d5`) on aledgent's VOS fork, from `saga/agents` at
`2bb7876`; draft `proposals/vos/0005`. It lets an Agent opt into VOS's existing x86-64 recompiler
for the outer runtime and every inner actor machine. The default stays the interpreter.

**Run under Rosetta 2** (`linux/amd64` Docker on Apple silicon). Correctness results hold; timing is
valid only as a ratio between the two engines.

- **Identical on both engines.** `state-blob` and `kernel-probe`: 63 executions each, plus one
  Invoke swept across 276 outer and 147 actor gas budgets (halts, out-of-gas and out-of-gas at
  load all agree). aledgent's core: 75 reservation executions. Exit, counter, registers, gas and
  output bytes match every time.
- **The core's reservation:** interpreter ≈ 5.1 s, recompiler ≈ 0.48 s at p50, both 1,488 M gas.
  About **10.8× less wall time at identical gas.** A second round was disturbed by host load; its
  tails are not used.
- **Compile cost** (median): the core's 1.97 MB actor compiles in 136 ms to 27.8 MB of native code,
  against 113 ms of interpreter predecode. Paid once per program with 0004's cache.
- **Not yet recorded:** VOS's own suites and the v0.8.0 vectors on both engines for this branch.
  Re-run on x86-64 Linux before the pull request is opened (0005, "Tests").
- **Still needed:** absolute numbers on real x86-64 hardware (a self-hosted x86-64 runner,
  `.github/workflows/x86-bench.yml`).

Raw samples: `spikes/results/*-f025c.csv`, `spikes/results/compile-bench-f025c.log`.

## 15. F-025 change 5 on real x86-64 (2026-09-25)

§14 on real hardware: a self-hosted x86-64 runner, through `.github/workflows/x86-bench.yml` (modes
`vos-suites` and `reservation-timing`, PR #84). VOS `agent/recompiled-agent-execution` at `3140c3d5`,
the branch of VOS PR #10. §14's timing under Rosetta 2 was valid only as a ratio. This section has
absolute numbers.

**Outcome.**

- **VOS's own suites pass on x86-64 with each engine on the kernel path**, including the Gray Paper
  v0.8.0 instruction and block-gas vectors and the new cross-engine tests. The 0005 draft is still
  missing these counts, and PR #10 needs them before it can merge.
- **The core's reservation: 236 ms at p50 and 240 ms at p99 on the recompiler.** The interpreter
  takes 2.95 s and 3.10 s. That is **12.5 times less wall time at identical gas** (1,487.3 M per
  Invoke). Rosetta's ratio was 10.8.
- **Lockstep:** all 75 executions are identical on both engines: exit, counter, registers, gas and
  output bytes.
- **Against `NFR-1`, the reservation is now about 4.8 times over 50 ms**, down from about 47 times
  (§13.5). The rest is gas, not engine. §15.4 sets out what closes it.

### 15.1 Host

| | |
|---|---|
| Runner | A self-hosted x86-64 runner, one ephemeral VM per job |
| CPU | AMD EPYC Genoa (`AMD EPYC-Genoa Processor`), 16 vCPU, AVX2 and AVX-512F |
| Memory, disk | 30 GiB, no swap; 601 GB root |
| OS | Ubuntu, kernel `7.0.0-30-generic`, `x86_64` |
| Build and run | Docker 29.1.3, `rust:1-bookworm` (`sha256:93ce27a8…`), one container per job, native (no emulation) |
| Rust (VOS suites) | `nightly-2025-05-09` (`rustc 1.88.0-nightly 50aa04180`), VOS's `rust-toolchain.toml` |
| Rust (core host and both guests) | `nightly-2026-03-20` (`rustc 1.96.0-nightly bcf3d36c9`), the canonical guest toolchain |
| Load | 1-minute load average 1.6–2.4 when each mode starts, from the job's own setup; nothing else ran on the VM |

The host binary is built with `agents/`'s release profile (LTO off, no `target-cpu=native`), as on
the laptop. VOS's own `.cargo/config.toml` sets `target-cpu=native` for its suites.

### 15.2 VOS's own suites

Run 36094238803 (`vos-suites`, 5 min 23 s including the builds). `GREY_PVM` selects the kernel
path's engine; the x86-64 default is the recompiler. The Refine path (Agents) runs on the
interpreter unless a test asks for the recompiler, and `refine_backends.rs` and
`agent_runtime_pvm.rs` run both engines in lockstep themselves. Before `vos --lib`, the job builds
the two fixtures VOS's `just test` builds: the scripted recovery guest and `echo-extension`. No test
skipped itself.

| Suite | Kernel path on the recompiler | Kernel path on the interpreter |
|---|---|---|
| `vos-pvm`, library | 264 passed, 0 failed, 2 ignored | 264 / 0 / 2 |
| `vos-pvm`, `pvm_vectors.rs` (v0.8.0 and GP 0.7.2 vectors) | 20 passed, 0 failed | 20 / 0 |
| `vos-pvm`, `refine_backends.rs` (both engines) | 7 passed, 0 failed | 7 / 0 |
| `vos-pvm`, `spi.rs` | 4 passed, 0 failed | 4 / 0 |
| `vos --lib` | 1 511 passed, 0 failed, 2 ignored | 1 511 / 0 / 2 |
| `vos --test agent_runtime_pvm` (both engines) | 6 passed, 0 failed, 1 ignored | — (runs both itself) |

The v0.8.0 tests by name, all passing in both runs:

- `graypaper_v080_interpreter_satisfies_every_vector` and `graypaper_v080_recompiler_satisfies_every_vector`;
- `graypaper_v080_block_gas_matches_checked_in_oracle`;
- the corpus, opcode-manifest and coverage checks;
- the seven `v080_*` exit, fault and cyclic-memory tests;
- `v080_vectors_as_inner_machines_agree_across_backends`;
- `every_gas_budget_exits_identically_across_backends` and
  `nested_refine_programs_agree_across_backends_at_every_budget`;
- `faulting_and_spinning_actors_fail_identically_on_both_engines` and
  `agent_executions_match_across_engines_at_sampled_gas_budgets`.

Under Rosetta, `faulting_and_spinning_actors_fail_identically_on_both_engines` failed on an earlier
cut of the branch, and so did `three_node_cluster_elects_a_leader`, which is timing-sensitive. The
two recovery tests failed there for want of their fixture. None fails here.

The five ignored tests give their reasons in their ignore messages: a perf smoke, two measurements that need an input
file, and two that need a compiled probe or runtime fixture. None is a release gate.

### 15.3 The core's reservation

Run 36094889436 (`reservation-timing`, 8 min 13 s including the builds). §14's setup:

- **VOS:** `3140c3d5` with the compaction commit `de0f12f8` (`T-025-01`) cherry-picked (clean), and
  both program limits at 2 560 KiB.
- **Runtime:** built from that tree, 1 081 973 B. §14 reused §13's runtime instead.
- **The core:** F-005's full guest, program 1 968 379 B, byte-for-byte the size F-005 measured.
- **CipherClerk:** `tools/cipherclerk-patched.sh`.
- **Host:** patched as in §14 (`.github/x86-bench/core-host-backends.patch`).
- **Order:** one lockstep run, then the interpreter and the recompiler interleaved, twice, one run
  at a time. 26 samples per run, after 3 warm-up reservations, until the hot budget refuses the 30th
  (as in §14).

**Lockstep** (`F025_LOCKSTEP=1 F025_BACKEND=recompiler`): **75 executions identical**, no divergence.
That is every execution of the run: create, install, the setup commands, the warm-up and recorded
reservations, and their acknowledgements.

**Time** (ms, nearest rank; "round" is the driver's whole reservation, `sync_us`):

| Run | Round p50 | p95 | p99 | Invoke execution p50 | Acknowledge p50 |
|---|---:|---:|---:|---:|---:|
| Interpreter, round 1 | 2 957.8 | 3 020.0 | 3 080.6 | 2 914.6 | 34.3 |
| Recompiler, round 1 | 236.7 | 239.4 | 240.2 | 225.4 | 4.9 |
| Interpreter, round 2 | 2 947.4 | 3 097.2 | 3 103.7 | 2 902.6 | 34.9 |
| Recompiler, round 2 | 236.3 | 238.0 | 239.4 | 224.7 | 5.0 |

**Gas**, identical on both engines and in every run:

- Invoke: 1 487.29 M at p50, 1 504.68 M at worst.
- The actor's own work: 173.41 M.
- Acknowledge: 33.45 M.

The Invoke is 0.23 M more than §14's, and the actor's gas is the same to the unit. The difference
is in the runtime, which here is built from the tree under test.

The engines, then:

- **The recompiler runs 6.6 G gas per second on this workload.** The interpreter runs 0.51 G.
- **The round is 12.5 times shorter at p50 and 12.9 times at p99.** The recompiled rounds are
  tight: p99 is 1.5 % above p50.
- **Outside the Invoke's execution, the round costs about 11.4 ms.** That is the Acknowledge, the
  work encode, the preparation and the host's own decision.
- **Rosetta was pessimistic about the recompiler.** It gave 10.8 times less wall time; real hardware
  gives 12.5.
- **This EPYC's interpreter is about 16 % slower than the M2 Pro's** (2 952 against 2 545 ms at
  p50, §13's "before" tree). Only the recompiler makes x86-64 the faster host.

An earlier dispatch of both modes from the pull request's branch agrees:

- runs 36092667457 and 36093404476;
- the same suite counts;
- recompiler rounds at 236.0 and 236.6 ms p50, 238.2 and 238.9 ms p99;
- 75 identical executions.

### 15.4 The reservation against `NFR-1`

| | Gas per Invoke | p99 of the driver's round |
|---|---:|---:|
| `F-005` as measured, interpreter, M2 Pro (§13) | 1 487 M | 2 677 ms |
| With `T-025-03`, interpreter, M2 Pro (§13) | 1 136 M | 2 360 ms |
| **Change 5, recompiler, EPYC Genoa (this section)** | **1 487 M** | **240 ms** |

**The measured reservation is 4.8 times over 50 ms.** Changes 3 and 5 have not been measured
together. The estimates below convert gas at the recompiler's 6.6 G gas per second, plus the round's
11.4 ms outside the Invoke:

| With change 5, plus | Gas per Invoke | Estimated round |
|---|---:|---:|
| Nothing more (measured) | 1 487 M | 236 ms p50, 240 ms p99 |
| `T-025-03` (prepared actor, §13) | 1 136 M | ≈ 183 ms |
| `T-025-03` and the core's declared stack (`0003`, §13.5) | ≈ 1 045 M | ≈ 170 ms |
| `T-025-03` and option C of `0004` (the program out of the work: no BLAKE2b passes over it) | ≈ 253 M | **≈ 50 ms** |

The conversion assumes that every unit of gas costs the same time on the recompiler. It does not
hold exactly: the three BLAKE2b passes and the actor's own code are different code. The last row is
therefore "at the line", not "under it".

What stands between the reservation and `NFR-1` is now the BLAKE2b cost of carrying the program in
every Invoke (about 880 M gas, §13.5). The engine is no longer the obstacle. Options A to C of
`proposals/vos/0004` are @olanod's call. The Authority round trip and the durable journal write are
still outside these numbers (§7).

### 15.5 Reproducing

`gh workflow run x86-bench.yml -f mode=vos-suites` and `-f mode=reservation-timing`: manual, one
run at a time. The inputs `vos_url`, `vos_ref` and `vos_picks` default to this section's pins. The
job summary carries the tables above.

Raw samples: `spikes/results/core-*-x86.csv`. The lockstep log is
`spikes/results/logs/core-lockstep-x86.log`, and both job summaries are
`spikes/results/logs/x86-vos-suites.md` and `x86-reservation-timing.md`.
