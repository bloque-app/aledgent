<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# The core Agent, measured (F-005, T-005-10)

2026-09-24. The reservation path through the core Agent, end to end, with `F-002`'s method
(`notes/MEASUREMENTS-F002.md` §3). `NFR-1` asks for a balance-affecting decision within **50 ms at
p99**. The Agent path takes **2.59 s at p99**, about fifty times over. **`RISK-2` is raised with
these numbers**.

**§8 (2026-09-25) lifts §4's heap bound:** the service store is now witnessed leaves, and 1 000
reservations, captures and transfers through VOS leave the Agent's state at 614 B; a reservation's
actor gas is 297 M, flat but for the proofs' logarithmic depth.

## 1. What was run

| | |
|---|---|
| Host | As `MEASUREMENTS-F002.md` §1: Apple silicon, Docker's Linux VM (aarch64), one run at a time |
| VOS | aledgent's VOS fork, `agent/compact-acknowledged-results` at `de0f12f8` (`T-025-01`), **with the two program limits raised from 1 280 KiB to 2 560 KiB** (`MAX_RUNTIME_PROGRAM_BYTES`, `MAX_EXECUTION_PROGRAM_BYTES`): the pinned runtime refuses the core's program (§4). Runtime and host both built from that tree |
| Runtime | The standard AgentRuntime (`services/agent-runtime`, `cargo actor`), linked by `aledgent-core-host runtime-pvm`: 1 081 973 B |
| Actor | `agents/core`, every command group (`full`): package 1 969 952 B, program 1 968 385 B. `opt-level = "z"`, LTO, `-Zfmt-debug=none`, `-Zlocation-detail=none`, `-Cpanic=immediate-abort`, 256 KiB stack (`pvm.ld`) |
| Path | `aledgent-core-host measure`: the ABI path, as `serve` drives it (`vosx` is unusable, `T-025-05`). Each reservation: the library's decision on the host's replica, then the driver's round (witness and private inputs from `F-006`'s leaf store, Invoke, Acknowledge, the reply checked root for root against the replica) |
| Book | Genesis, an issuance grant, one identity, one account, one issue; then reservations of 100 + i minor units until the budget refuses one |
| Budget `H` | The default, 26 KiB of Agent state (§3) |
| Samples | 3 warm-up, then 26 recorded; the 29th reservation was refused `OverBudget` (26 921 B against 26 624 B). Raw samples: `spikes/results/f005-reserve.csv` (`spike` = `s5`; `size` is the runtime state before the sample) |

Every sample was checked: the Agent committed, and its kernel root, service root and audit head
equalled the replica's.

## 2. The reservation path at `H`

Nearest-rank, n = 26.

| Metric | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|
| Driver round (`sync`), ms | 2 534.7 | 2 588.8 | 2 589.5 | 2 589.5 |
| At the port (Invoke + Acknowledge, encode, decode), ms | 2 534.2 | 2 586.5 | 2 587.9 | 2 587.9 |
| Invoke, ms | 2 493.2 | 2 542.7 | 2 545.9 | 2 545.9 |
| … of which preparation, ms | 1.2 | 3.2 | 3.7 | 3.7 |
| Invoke gas, M | 1 487.1 | 1 504.3 | 1 504.7 | 1 504.7 |
| Actor gas (the core's own work), M | 173.4 | 185.9 | 186.2 | 186.2 |
| Acknowledge, ms | 32.8 | 42.3 | 47.0 | 47.0 |
| Acknowledge gas, M | 33.3 | 37.0 | 37.3 | 37.3 |
| Invoke input, B | 1 995 730 | 2 008 106 | 2 008 832 | 2 008 832 |
| Message, B | 187 | 187 | 188 | 188 |
| Caller blobs (witness and private inputs), B | 1 981 | 2 181 | 2 277 | 2 277 |
| Runtime state, B | 23 711 | 35 831 | 36 661 | 36 661 |
| **Native decision on the replica, ms** | **1.3** | **2.0** | **2.0** | **2.0** |

The smoke run (`aledgent-core-host smoke`) gives the same shape for other commands: genesis 2.4 s;
a grant, an identity or an account 2.3 s at 1.33 G gas with 26 M of actor gas; an issue 2.5 s at
157 M of actor gas.

## 3. Where the time goes

- **The program, reloaded every time: about 1.31 G of the 1.49 G gas.** The Invoke's input is the
  work, and the actor's program travels in it as availability (1.97 MB of its 2.0 MB). §7 of
  `MEASUREMENTS-F002.md` found about 650 gas per byte of actor program per invocation ("the
  runtime loads a fresh inner machine for the actor on every invocation"); 650 × 1 968 385 is
  1.28 G. This is `T-025-03`'s term (cache the prepared actor across invocations), and at this
  program size it is almost the whole cost.
- **The core's own work: 173 M gas, about 0.29 s** at the runtime's 600 M gas/s on this host: the
  kernel's reserve and the witness verification, interpreted (`MEASUREMENTS-F002.md` §11.6's two
  interpreted terms), plus decoding and re-encoding the service image (below). With `T-025-03`
  alone the path would still be about six times over `NFR-1`; `T-025-06` (a compiled path) is
  what closes it, if anything does.
- **The Acknowledge** is small (33 ms), because the Agent's state is small.

## 4. Two walls before the budget

**The program limit.** The pinned runtime refuses a program over 1 280 KiB. The core with every
command group is 1 968 385 B; the reservation path alone (no custody, settlement, anchoring,
operator or opening groups) is still a 1 453 878 B package, with the size flags above applied.
So the core does not install on the pinned VOS at all; everything here ran on the raised limit.
Drafted upstream as `proposals/vos/0003`.

**The heap.** An application actor runs in a fixed 256 KiB heap (`vos/src/abi/pvm/alloc.rs`); past
it, the invocation panics and nothing commits. The core decodes its whole service image (`F-006`
decision 5 is still open: the library's records are not service leaves yet), and its peak heap
runs at about **six times its state** (`crates/core-binding/tests/heap.rs`, natively, under a
counting allocator):

| Agent state, B | Peak heap, B |
|---:|---:|
| 3 226 | 74 877 |
| 10 579 | 94 366 |
| 18 750 | 146 678 |
| 26 920 | 183 976 |
| 35 090 | 243 964 |
| 43 260 | 282 306 |

In VOS, with the budget at 3 MiB, the 32nd reservation panicked, at 38 297 B of runtime state. The
default budget is therefore **26 KiB**, which keeps the peak under three quarters of the heap
(`tests/heap.rs` asserts it): about **thirty operations over genesis, for the Agent's whole
life**, because the service store keeps every operation. That is a demonstration bound, not a
book. What lifts it is the service tree (`F-006` decision 5: encode `ledger-core`'s records as
service leaves, witnessed like the kernel's), or a larger heap for this actor (`proposals/vos/0003`
asks for one declared by the package).

## 5. The conformance suite through VOS

`through-agent/vos` (`crates/conformance/src/agent.rs`), against `aledgent-core-host agent-server`
with a guest built with `conformance-ports` (package 1 995 009 B), one fresh Agent per case, four
cases at a time, by hand:

| Cases | Pass | Not runnable here | Fail | Wall time |
|---|---:|---:|---:|---:|
| `F-003` (identities, accounts, assets, authority) | 39 | 3 | 0 | 169 s |
| `F-004` (the reservation model) | 13 | 1 | 0 | 109 s |

The not-runnable ones are kernel or store cases, which have no Agent in them. The whole suite
through VOS is hours at this speed, and cases whose book outgrows the heap would panic (§4); the
full suite runs through the Agent's transition natively (`through-agent/native`) on every CI run.

## 6. What this means for `DEC-4`

The owner ruled #38 for the Agent (`DEC-4` as ruled, `DEC-16`, `F-025`). The binding works: the
library runs in the Agent, the Agent refuses what it must (another principal, a tampered or stale
witness, a full ring, the budget), and the host rebuilds from the Agent after any failure. But at
the pinned VOS it cannot be installed, it holds about thirty operations, and a reservation takes
2.5 s against 50 ms. In order of effect: `T-025-03` (program cache, −1.3 G gas), the service tree
(the heap bound), `proposals/vos/0003` (program limit, heap), `T-025-06` (the remaining 0.29 s).
`T-025-08` re-measures with each accepted change: re-run §1 with the same command.

## 7. Reproducing

`agents/README.md` has the commands. In short, on Linux with the pinned toolchain:

    tools/cipherclerk-patched.sh
    (cd agents/core && cargo actor)
    aledgent-core-host runtime-pvm <agent_runtime.elf> agent_runtime.pvm
    aledgent-core-host package aledgent_core.elf core.vos
    aledgent-core-host measure agent_runtime.pvm core.vos reserve.csv --n 100 --warmup 3

with the runtime and the host built from a VOS tree whose two program limits are raised.

## 8. The Agent's working state over 1 000 operations (`T-005-14`)

2026-09-25, after `T-005-11` to `T-005-13`: the service store is witnessed leaves (`F-006` decision
5), so the Agent keeps roots plus bounded records and each input brings the service leaves it
touches (`features/F-005-core-agent-binding/NOTES.md`, decisions 20 to 33). Before, §4's heap bound
held the Agent to about thirty operations for its whole life.

| | |
|---|---|
| Host, VOS, runtime | As §1: Docker's Linux VM (aarch64); aledgent's VOS fork at `de0f12f8` with the two program limits raised to 2 560 KiB; the standard AgentRuntime, 1 081 973 B |
| Actor | `agents/core`, every command group: package 2 148 867 B, program 2 147 300 B (§1's was 1 968 385 B) |
| Path | `aledgent-core-host measure --mix mixed`: the ABI path, as `serve` drives it. Each input: the library's run on the host's replica, then the driver's round (the kernel witness from the leaf store, the service witness from the host's service tree, the private inputs, Invoke, Acknowledge, the reply checked root for root against the replica, the input's service leaves applied to the host's tree) |
| Book | Genesis, an issuance grant, two identities, two accounts, each issued 10⁹ minor units |
| Workload | Rounds of four operations: a reservation and its capture into the other account, a transfer each way, and a reservation left open (so the open-reservation index grows by one a round). 4 warm-up operations, then **1 000 operations, 1 250 invocations**, every one committed |
| Budget `H` | The default, 26 KiB |
| Samples | `spikes/results/f005-mixed.csv` (`spike` = `s5`, `case` = the command, `size` the runtime state before the sample) |

Every sample was checked: the Agent committed, and its kernel root, service root and audit head
equalled the replica's. No invocation was refused.

The branch was then rebased onto #89 (`F-016`'s nonce facts, which add to the settlement store):
its program is 2 168 660 B, and a 40-operation run of the same workload on it gives 291 M of
actor gas and 1 741 M of invoke gas at p50 over its 50 invocations, as the first quarter below
does; the extra 21 KB of program is about 14 M of invoke gas.

### 8.1 State against the operation count

| At | Agent state, B | VOS runtime state, B |
|---|---:|---:|
| The first recorded reservation | 591 to 638 on every input of the native run (§8.4) | 8 205 |
| The fortieth invocation | | 11 886 |
| 1 000 operations (1 262 inputs since genesis) | **614** | **11 914** |

The Agent's state is its hot row, its configuration and its unarchived ring: 614 B at the end, with
the ring archived after every input (`hot:` of the run). The VOS runtime state (every lane
component, the runtime's own records included) reaches 11.9 KB within the first forty invocations,
as its windows of results and acknowledgements fill (32 each, `vos/src/agent/standard.rs`), and
stays there. Before, the Agent's state was the service image, and every operation added to it
until the budget's 26 KiB refused the twenty-ninth reservation (§1).

### 8.2 Gas and size per call

Nearest-rank, by command.

| Command | n | Actor gas p50, M | p99 | Invoke gas p50, M | Acknowledge gas p50, M | Blobs p50, B | p99 | Message, B |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Reserve | 250 | 296.8 | 300.8 | 1 738.1 | 31.9 | 8 914 | 9 848 | 224 |
| Capture | 250 | 324.2 | 328.5 | 1 765.8 | 31.9 | 9 238 | 10 123 | 211 |
| Transfer | 500 | 287.8 | 291.1 | 1 728.5 | 32.0 | 8 636 | 9 521 | 245 |
| Reserve, left open | 250 | 296.7 | 300.8 | 1 738.1 | 31.9 | 8 877 | 9 860 | 224 |

The blobs are the kernel witness, the service witness and the private inputs together.

| Time, ms (all 1 250) | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|
| Driver round (`sync`) | 2 933 | 3 729 | 4 221 | 6 187 |
| Invoke | 2 890 | 3 666 | 4 127 | 6 075 |
| Acknowledge | 30 | 67 | 96 | 192 |
| Native decision on the replica | 2.0 | 6.1 | 9.7 | 33.7 |

Trade-off, stated plainly: the times are noisier than §2's, because the host built and tested the
root workspace, at the lowest priority, for part of the run. Gas is deterministic and is the
figure to compare.

### 8.3 The trend

Means by quarter of the run, by command:

| Invocations | Reserve actor gas, M | Capture, M | Transfer, M | Reserve blobs, B | Transfer blobs, B | Runtime state max, B |
|---|---:|---:|---:|---:|---:|---:|
| 1 to 312 | 294.4 | 322.7 | 286.5 | 7 212 | 7 084 | 11 914 |
| 313 to 624 | 296.2 | 326.0 | 287.1 | 8 683 | 8 397 | 11 914 |
| 625 to 936 | 297.5 | 324.8 | 287.8 | 9 180 | 8 845 | 11 914 |
| 937 to 1 250 | 298.2 | 323.6 | 289.0 | 9 487 | 9 147 | 11 914 |

Nothing an invocation costs grows with the number of operations but the proofs: a multiproof's
frontier holds about `log₂ n` nodes per touched leaf, where `n` is the leaves in its tree, so the
blobs grew by 2.3 KB (32 %) and a reservation's actor gas by 3.8 M (1.3 %) while the book went from
a few dozen leaves in each tree to thousands. Doubling the book adds about one node to each path.

### 8.4 Natively, 10 000 operations

`crates/core-binding/tests/heap.rs` runs the same rounds natively, with every notification
delivered once a round, under a counting allocator (`ALEDGENT_HEAP_OPERATIONS=10000`; CI runs
1 200):

| Operations | Invocations | Agent state, B | Peak heap and inputs, first tenth, B | Last tenth, B | Leaves given, max | Kinds read whole |
|---:|---:|---|---:|---:|---:|---:|
| 1 200 | 1 800 | 591 to 636 | 88 254 | 93 909 | 21 | 0 |
| 10 000 | 15 000 | 591 to 638 | 93 116 | 98 771 | 21 | 0 |

The worst invocation of the 10 000 needs 98.8 KB of the guest's 256 KiB heap, message and blobs
included; before, the peak heap alone was 184 KB at 26.9 KB of state, and grew with it (§4). The
number of service leaves an input is given does not change with the book (21 at most: the
deliveries' input), and no reservation, capture or transfer reads a kind of the service store
whole.

### 8.5 Against §2

| Reservation | §2 (image, at `H`) | Now (leaves, after 1 000 operations) |
|---|---:|---:|
| Agent state | Every operation added to it, to the budget's 26 KiB | 614 B |
| VOS runtime state | 23.7 to 36.7 KB, and growing | 11.9 KB, flat |
| Operations the Agent can hold | about 30 | not bounded by the Agent's state |
| Actor gas p50 | 173.4 M | 296.8 M |
| Invoke gas p50 | 1 487.1 M | 1 738.1 M |
| Driver round p50 | 2.53 s | 2.93 s |

The actor gas rose by about 120 M: most of it is the service proof's hashing (about twenty leaves
folded, twice, through 128 levels) and the decoding and re-encoding of the leaves; §2's image was
cheap only because the book was small, and it grew with the book. The invoke gas rose by 250 M: the
actor's 120 M, and 179 KB more program for the runtime to reload on every invocation (at §3's 650
gas per byte, about 116 M). `NFR-1` is still missed by about fifty-eight times, for §3's reasons;
`T-025-03` (the program cache) and `T-025-06` (a compiled path) remain what closes it, and neither
depends on the book's size any more.

### 8.6 Reproducing

As §7, then:

    aledgent-core-host measure agent_runtime.pvm core.vos mixed.csv --n 1000 --warmup 4 --mix mixed
    ALEDGENT_HEAP_OPERATIONS=10000 cargo test -p core-binding --test heap -- --nocapture

## 9. On x86-64 with the recompiler (2026-09-29, amendment 0007)

Run 36520579669 of the manual
x86-64 bench workflow, on `main` at `a923fe9`: AMD EPYC Genoa, 16 CPUs. VOS was the fork's
`agent/recompiled-agent-execution` branch (`3140c3d`, recompiled Refine execution for Agents; `62229f4`),
with both program limits raised to 2 560 KiB. The core package was 2 170 190 B (the current 2.17 MB
core; program 2 168 623 B), and the runtime 1 081 973 B. Each run did 100 reservations after a warm-up
of 3.

| Engine | Driver round p50 | p95 | p99 | Invoke gas p50 | Actor gas p50 |
|---|---:|---:|---:|---:|---:|
| Interpreter (round 1) | 3.15 s | 3.27 s | 3.32 s | 1 747.2 M | 294.6 M |
| Interpreter (round 2) | 3.16 s | 3.26 s | 3.28 s | 1 747.2 M | 294.6 M |
| Recompiler (round 1) | 283.9 ms | 315.9 ms | 344.2 ms | 1 747.2 M | 294.6 M |
| **Recompiler (round 2)** | **284.6 ms** | **316.3 ms** | **344.2 ms** | 1 747.2 M | 294.6 M |

- **Same work on both engines.** Each call used exactly 1 747 M gas of invocation on both engines.
  The lockstep run executed every invocation on both and compared exit, counter, registers, gas and
  output. **All 227 executions were identical**, with no divergence.
- **The recompiler is about 11 times faster** than the interpreter on the same gas: 284 ms p50 and
  344 ms p99 for the driver's round, against 3.15 s p50 for the interpreter.
- The acknowledgement of the invocation took 4.7 ms p50 on the recompiler and 27.8 ms p50 on the
  interpreter.

These are **observations, not requirements** (amendment 0007 R3). Since that amendment, `NFR-1`
bounds the acknowledgement of a command (50 ms p99), which ingress gives before the core decides.
The decision is part of completion (`NFR-15`: 19 s target, 30 s ceiling at p99), and 344 ms sits far
inside it. The run's summary and raw CSVs are in its artifact,
`x86-bench-reservation-timing-36520579669`.
