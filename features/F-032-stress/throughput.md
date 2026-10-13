<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-032 — Throughput: from one Agent invocation per command to millions of agents

> **Read with `F-031`.** The levers below are being built in parallel as `F-031` (core throughput,
> PRs #124, #130, #131: amendment 0012 and `DEC-22`, batched invocations, account-partitioned keys
> and a shard prototype). This section is the stress suite's side: what the curve and the runs
> measured, the ceiling each step can reach, and what each needs. Where `F-031`'s amendment 0012
> (pending) proposes partitions, §4's spec consequences are the ones it has to answer.

The owner's direction (2026-10-03): 3.5 commands a second through the core Agent is far too low;
the target is millions of concurrent AI agents. This section says what limits throughput today,
measured, and what each step of a design would buy, with its ceiling and what it needs from VOS.
Numbers marked *measured* name their run; numbers marked *estimated* show their arithmetic.
Nothing here is decided: the open questions are `s-02` to `s-10` (there is no `s-01`: the owner ruled on 2026-10-03 that no NFR sets a throughput target for now), and any change to `REQ-RT-1`, `INV-25` or §1.7 of `SPEC.md` needs an amendment
first.

## 0. The headline: the greatest sustained rate today, and how far each lever goes

The owner's ruling of 2026-10-03: no NFR target for now; the headline is the greatest sustained
commands a second realistically achievable today, per scenario and per lever, against **10 000 a
second** (an acceptable start) and **1 000 000 a second** (the aspiration). *Sustained* means at
least 90 s with every bound of §11 held, errors at or under 1 ‰ and no growing queue
(`plan.md` §2). *Measured* and *estimated* as everywhere in this file. Runs against the stress
plans are stated as multiples of the reference load L (`plan.md` §2), never as absolute rates; the
per-lever rates below are measurements of the core alone, not of a plan.

**Today, per scenario** (each run's summary in the stress workflow):

| Scenario | Greatest sustained, every bound held | Capacity, bounds aside | Of 10 000/s | Of 1 000 000/s | What stops it |
|---|---:|---:|---:|---:|---|
| The core as a VOS Agent, interpreter (the canary's pin), 2-vCPU runner | **0** (no phase held) | ≈ 0.16 commands/s (*measured*, setup) | 0.002 % | 0.00002 % | the interpreter: ≈ 3 s an invocation |
| The core as a VOS Agent, recompiler, 2-vCPU runner | **0** (no phase held; at ⅒ × the reference hour it keeps up but misses `NFR-1` on API keys and `NFR-16`) | keeps up at ⅒ ×, behind at ¼ ×; drains at ≈ 1.1 inputs/s (*measured*) | 0.005 % | 0.00005 % | one invocation an input; reads and API keys under the same lock |
| The core as a VOS Agent, recompiler, 16-vCPU EPYC, one command an invocation | not run under load | **3.6/s** (*measured*, batch curve) | 0.036 % | 0.0004 % | ≈ 1.5 G gas fixed per invocation |
| The in-process core, 2-vCPU runner | **20 × L** (*measured*: for 2 minutes, every bound held; 10 × L for 10 minutes) | between 20 × and 40 × L; at 40 × L decisions **collapsed to under 1 × L** | — | — | the forwarder (one input at a time, two PostgreSQL commits each); past it, clients polling for their decisions starve the core (F3) |
| The in-process core, M2 Pro laptop | **10 × L** (*measured*; 20 × L did not hold) | ≈ 13 × L (*measured*, the drain after 160 ×) | — | — | the same forwarder, on macOS's slower `fsync` (two commits an input) |
| Scenario c (the zombienet driver's in-process core, withdrawals through the adapter) | decided replies and completion held at 2 ×; acknowledgements did not (72–168 ms on that host) | — | — | — | that host runs three nodes; one withdrawal at a time settles in ≈ 40 s |

**Per lever, toward 10 000 and 1 000 000 a second** (one writer unless said; §§1–5 for each):

| Lever | One writer | Of 10 000/s | Shards (writers) for 10 000/s | Shards for 1 000 000/s | Needs |
|---|---:|---:|---:|---:|---|
| Today, Agent, recompiler | 3.6/s | 0.04 % | ≈ 3 300 | ≈ 330 000 | — |
| 1. Batches (bounds raised) | ≈ 14/s (*measured*) | 0.14 % | ≈ 850 | ≈ 85 000 | VOS bounds; `F-031` Lever 1 |
| 1′. … and the binding's root computed once | ≈ 30/s | 0.3 % | ≈ 400 | ≈ 40 000 | an aledgent fix |
| 3. … and native curve arithmetic | ≈ 60/s (*assumption-bound*) | 0.6 % | ≈ 200 | ≈ 20 000 | VOS |
| 4. … and compiled Agent execution | ≈ 500–900/s | 5–9 % | ≈ 13–24 | ≈ 1 300–2 400 | VOS (`T-025-06`) |
| Native decision, no Agent (the replica's 1.07 ms) | ≈ 900/s of decisions | 9 % | ≈ 12 | ≈ 1 200 | `DEC-4` reopened |
| The forwarder batched (two commits per *batch*, not per input) | lifts the in-process ceiling to the decision's | — | — | — | `F-031` Lever 1 in ingress |
| 5. Sharding by account | N × the writer's rate × ≈ 0.85 | — | (columns above) | (columns above) | VOS Agent-to-Agent delivery; amendment 0012 (`F-031`) |
| 6. The received-log (one PostgreSQL primary) | ≈ 10⁴ inserts/s with group commit (*estimated*) | ≈ 100 % | — | **partitioned per shard** | the edge's own lever at 1 000 000 |

**Reading it.** 10 000 a second is reachable only with sharding, and with each shard's writer
running at native or near-native speed: about a dozen to two dozen shards at compiled or native
speed, or several hundred at the batched Agent's 30 a second. 1 000 000 a second is two more
orders: about a thousand to a few thousand shards at native speed, a received-log partitioned with
them, and an edge that pushes decisions rather than serving polls. No lever short of sharding
moves the headline by more than about two orders of magnitude from today's.

## 1. Where the time goes today

*Measured:* the batch curve, x86 bench run
37084984127 (AMD EPYC Genoa,
16 vCPU, the zombienet fleet), the core Agent on VOS `3140c3d` (recompiled Agent execution) with
`de0f12f` picked, every per-invocation bound raised for the experiment. Each size is a fresh book
of 64 funded accounts; each round is K commands (transfers between random accounts and
reservations, alternately) made on the replica and synced as one invocation; five recorded rounds
after one warm-up; medians.

| Commands per invocation | Invocation | Per command | Commands a second | Invoke gas | Actor gas | Runtime gas (Invoke − actor) | Actor gas per command | Native decision per command | Message | Witnesses and private inputs |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 279 ms | 279 ms | **3.6** | 1 787 M | 286 M | 1 501 M | 286 M | 1.07 ms | 242 B | 7.0 KB |
| 8 | 575 ms | 72 ms | **13.9** | 4 102 M | 2 572 M | 1 530 M | 321 M | 1.08 ms | 873 B | 31 KB |
| 32 | 2.44 s | 76 ms | **13.1** | 20 000 M | 18 383 M | 1 617 M | 574 M | 1.04 ms | 3.0 KB | 93 KB |
| 128 | 21.4 s | 167 ms | **6.0** | 177 910 M | 176 051 M | 1 859 M | 1 375 M | 1.13 ms | 11.9 KB | 267 KB |
| 512 | — | — | — | out of gas at 10^12 actor gas | | | | | | |
| 2 048 | — | — | — | out of gas at 10^12 actor gas | | | | | | |

**Three costs, and the third is ours.**

1. **A fixed cost per invocation of about 1.5 G gas** (1 501 M at one command, 1 859 M at 128: it
   barely grows). It is the runtime's, not the core's, and it is **not the service image**: the
   core Agent's own state is under a kilobyte, and its leaves travel as witnesses. It is the
   program. `notes/MEASUREMENTS-F002.md` §13.2 profiled it per byte of actor program: three BLAKE2b
   passes over the 2.23 MB program (the work's public-IO hash, the availability reference, the
   ProgramId; 447 gas a byte, about 1.0 G here), re-packing and expanding its code bitmask (147 gas
   a byte, about 0.33 G), and the rest of the runtime's work. At one command an invocation that is
   84 % of the gas and 1.5 G / 6.4 G gas a second ≈ 235 ms of the 279 ms. Batching amortises it.
2. **About 250 M gas of the core's own work per command**, against **1.07 ms natively** for the
   same command on the same machine (the replica's decision, measured in the same run; 1.3 ms on
   the M2 Pro of `MEASUREMENTS-F005.md` §2): about 38 ms on the recompiler, 35 times native. It is the kernel's curve arithmetic in software on
   `riscv64em` (Ristretto scalar multiplications at about 3.9 M gas each, `MEASUREMENTS-F002.md`
   §12.3), the witness checks, and the encoding of the transition.
3. **A term that grows with the square of the batch: about 8.8 M gas × K per command** (fit over
   the four sizes: actor gas ≈ 251 M × K + 8.8 M × K²; it predicts 17 G at K = 32 against 18.4 G
   measured). The cause is in aledgent's binding, not in VOS: `WitnessStore::commit`
   (`crates/core-binding/src/witness.rs`) recomputes every witnessed leaf's hash and every
   sub-tree's root after **each** command's kernel commit (`leaf_hashes(&self.leaves)` over all of
   them), so a batch of K commands hashes K witnesses of growing size. Hashing only the leaves a
   commit changed, or computing the roots once at the end of the invocation, removes it. This is
   what makes 128 slower than 8, and what runs 512 out of 10^12 gas.

**The stock bounds stop a batch long before any of this.** Each is a constant of VOS today:

| Bound (VOS `3140c3d`) | Stock | Per command, measured | Largest batch it allows |
|---|---:|---:|---:|
| Actor gas per execution (`MAX_EXECUTION_GAS`) | 1 G | 286 → 1 375 M | **3** |
| Caller availability (`MAX_RUNTIME_CALLER_AVAILABILITY_BYTES`) | 192 KiB | about 2 KB of witness (7 KB fixed) | about 90 |
| Invocation message (`MAX_INVOCATION_MESSAGE_BYTES`; the calls travel in it) | 16 KiB | about 92 B | about 170 |
| Actor heap (application profile) | 256 KiB | the message and witnesses are copied in | about 50 to 100 (*estimated*; not isolated) |
| Actor state (`MAX_EXECUTION_STATE_BYTES`; the ring entry holds the calls) | 48 KiB | about 92 B | about 500 |

So today, **with stock VOS, the core Agent cannot carry more than three commands per invocation**,
and the most it can do is about 8 commands a second (three per 1.5 G + 3 × 286 M gas, about 0.37 s).

## 2. Large batches per round (TigerBeetle-style)

**The idea.** TigerBeetle orders and commits transfers in batches of up to 8 190, one consensus
round and one state-machine pass per batch; its throughput comes from amortising every fixed cost
over the batch. The core Agent already has the mechanism: the driver sends every library call the
replica made since its last sync as one input (`Driver::sync`), and the Agent re-executes them in
one transition, one commit (`REQ-RT-1`). What is missing is a forwarder that decides several rows
on the replica and syncs them together, and VOS bounds wide enough to carry them.

**Ceilings** (the recompiler at about 8 G gas a second, the rate the large batches ran at;
*estimated* from the curve's fit):

| Step | Per-command gas | Fixed gas | Best K | Commands a second |
|---|---:|---:|---:|---:|
| Today, one command an invocation | 286 M | 1.5 G | 1 | **3.6** (measured) |
| Batches as the binding is, bounds raised | 251 M + 8.8 M × K | 1.5 G | 8 to 32 | **≈ 14** (measured: 13.9 at 8) |
| Batches with the root recomputed once per invocation (an aledgent fix) | 251 M | 1.5 G | 128 to 512 | **≈ 30** (asymptote 8 G / 251 M ≈ 32) |
| … and the curve arithmetic native in the runtime (`F-025`, `T-025-02` with a native handler) | the actor's non-curve share: not profiled; if the curve is half of it, ≈ 125 M | 1.5 G | 512 | **≈ 60** (*estimated*, on that assumption) |
| … and compiled Agent execution (`T-025-06`), at native speed | ≈ 1–2 ms of CPU (the native decision) | ≈ the hashing of the program, unless `proposals/vos/0004` C | 512 | **≈ 500 to 900** per writer |
| The in-process core (`DEC-1`'s native core, not taken) | 1.07 ms | none | — | measured in the stress runs (scenario a, `inprocess`) |

Large batches therefore buy roughly **ten times** today's rate once the binding's quadratic term
is gone, and stop at about 30 commands a second: the per-command cost of the guest, 35 times
native, is then the ceiling, and only VOS's execution speed moves it.

**`F-031`'s Lever 1 (PR #130) batches differently from this curve**: its `Envelope::Batch` carries K
inputs, each with its own witness built against the state the input before it leaves, where this
curve made K commands into one input with one witness. Per-input witnesses avoid §1's quadratic
term (each input's store is its own) and pay each input's witness checks instead; its default bound
is 32 inputs, inside VOS's stock message, reply and availability bounds, so the actor-gas bound of
1 G (three reservations' worth) is the one it must raise. Its own measurements will say which costs
less per command; the ceilings below hold for either within their per-command gas.

**What it costs.**

- **Latency.** A batch waits to fill (a timer bounds it: 20 to 50 ms) and its commands complete
  together, when the invocation commits. Completion (`NFR-15`) is unaffected at these sizes. The
  decided reply (`NFR-16`, 1 s) is: a decided command waits for the batch it joins, so decided
  commands need their own small batches or a lane ahead of transfers (question `s-06`).
- **One refusal does not sink a batch.** The replica decides each command natively before the
  sync; a refusal is a result, not a fault. Only a fault (the Agent disagreeing with the replica)
  refuses the input, as today, and the host then rebuilds from the Agent.
- **`REQ-RT-1` holds**: a batch is one transition, all of it or none of it. The commit boundary
  widens from one command to K; nothing commits partially.
- **Replication** (`M-4`). Each invocation is one Raft entry on a Shared Agent: batching
  amortises the round too, which is TigerBeetle's point.

**VOS changes.** None to the model; bounds only, each a constant today (to be drafted in `proposals/vos/` if the owner rules for batching, `s-06`):
the invocation message (16 KiB), the reply (8 KiB at the execution, 16 KiB at the invocation),
the caller's availability (192 KiB), the actor's gas (1 G), the host calls (4 096), the actor's
state (48 KiB, the ring entry holds the calls), and the actor's heap (256 KiB). On aledgent's side:
the calls move from the message into a blob (they are the bulk of the message), the ring entry
keeps a digest of the calls rather than the calls, and the forwarder batches.

## 3. State as committed maps over external state blocks

**The idea.** Today the Agent's own state is tiny (614 B: the hot row, the configuration and the
unarchived ring; `notes/MEASUREMENTS-F005.md` §8), because `DEC-4` already moved the book's leaves
out: every input carries a witness of the leaves it touches (caller blobs, about 2 KB a command)
and the Agent checks it against its roots. The leaf store outside is re-derivable from the
committed delta log (`F-006`), which is how `REQ-RT-1` is met.

Committed maps over VOS's external state blocks would put the leaves inside the runtime's own
commit boundary instead: an actor reads and writes map entries, VOS fetches the touched blocks and
commits the new ones with the transition. Per-command cost would track the leaves it touches, as
the witnesses do now, but the witness would no longer travel in the invocation, and `REQ-RT-1`
would hold by construction rather than by re-derivation.

**What the measurements say.** VOS's experimental external-state path (`experimental-state-
blocks`, not selected by released code) moved rows at about 3 gas a byte against 252 on the image
path (`notes/MEASUREMENTS-F002.md` §11.5), but its fixed cost per invocation was higher, not lower:
the runtime cold-loaded on every execution (77 ms), and 67 to 190 M more gas per invocation,
growing with the actor's size. For this core, whose state is already small, the image is not
where the fixed cost is (§1), so this step buys **little throughput on its own**: it removes the
witness from the message and the availability (a large share of a big batch's bytes, §1), and it
moves the leaf store inside the commit boundary. Its value is in correctness and in making large
batches cheap to carry, not in the per-invocation constant.

**Ceiling.** The same as §2's, with the witness bytes gone from the per-command term:
about 30 commands a second per writer on today's execution speed,
but with the availability bound (192 KiB, about 90 commands) gone, so that batches of 512 and more
fit. The witness's bytes are about 2 KB a command (267 KB at 128, measured); what VOS would charge
for fetching the same leaves as blocks is not measured.

**VOS changes.** The external-state runtime selected and qualified for Agents (VOS's
`saga/agents` work), its preparation cached like the image path's, typed committed maps in the
actor SDK, and block fetches billed per block rather than per byte of the whole work.

## 4. Sharding by account

**The idea.** Only debits need an order: a debit, or a reservation, must see every earlier debit
of its source account (never negative, `INV-3`, at the kernel). Credits commute: adding to a
balance in any order gives the same balance. So the book can be split into N shards, each a core
Agent owning a disjoint set of accounts (by a hash of the account's identifier), each its own
single writer:

- a transfer's **source shard** decides it, commits the debit, and emits a **credit** to the
  destination's shard: idempotent (keyed by the operation reference), in its outbox, after commit;
- the **destination shard** applies credits either in VOS's Merge lane, as a commutative
  grow-only set keyed by operation reference (no ordering, no conflict, any order of arrival), or
  as ordinary ordered inputs;
- a reservation is source-local; its capture is a debit of the holding position (source shard) and
  a credit to the destination (a credit like any other).

**What it does to the spec. This is where it gets expensive.**

- **`REQ-RT-1` (one commit boundary) no longer holds for a transfer across shards.** The debit
  commits on one shard; the credit commits later on another. Between the two, the value is in
  transit: a per-(source shard, destination shard, asset) **transit position**, debited when the
  credit is emitted and credited when it lands, keeps every shard's trial balance at zero and the
  book's at zero (`INV-5`), with the transit positions as the in-flight sum. The payee sees the
  credit after its shard applies it: "final on acceptance" (`US-B1`) still holds for the payer and
  for the operation, but the payee's balance is eventually consistent, bounded by the credit's
  delivery. That is a behaviour change, and an amendment.
- **Multi-leg all-or-nothing (`US-B3`) across sources needs a protocol.** Legs whose sources sit
  in different shards cannot commit in one transition. The reservation model is the protocol:
  reserve on every source shard (prepare), then capture every one (commit) or release every one
  (abort), driven by a coordinator in ingress, with the reservations' own deadlines as the
  timeout. A capture after its reservation can never fail for funds (`INV-3` held at the reserve),
  so the commit phase cannot be refused once prepared. Single-source multi-leg (the purchase and
  its fee) stays one transition on one shard.
- **Guardrail 8 and `INV-25`** ("one book; nothing partitions the book"). A sharded book is one
  book logically (one deployment, one registry, one root of roots, one trial balance) and N
  writers physically. Whether that is "one book" in `INV-25`'s sense is the owner's call
  (question `s-07`), and the spec's wording must say so before a plan does it.
- **Proofs and anchors.** Each shard has its composite root. An anchor publishes a root over the
  shard roots **at a consistent cut**: each shard checkpoints at its own sequence, and the cut
  records the transit positions, so that every debit in the cut whose credit is not is accounted
  for in transit. Non-revision (`INV-19`) holds per shard and for the root of roots. A disclosure
  about one account is the shard's proof plus the shard root's inclusion in the anchored root. The
  prover (`F-030`) proves per-shard transitions, which are smaller than a whole book's.
- **Conversions and an issuing client's issuance.** A conversion inside one account is
  shard-local. An issuing client's pattern (the customer pays one asset into the pool, the pool
  pays another out) has two sources in two shards: it is a cross-shard multi-leg, so it goes through reserve-then-capture on both,
  or the pool is split into one sub-account per shard so that each swap is shard-local. Issuance
  and redemption are shard-local credits and debits under the issuance authority, whose bounds
  (`REQ-AU-2`, a limit per window) must then be split per shard or checked by a coordinator.
- **Hot accounts.** A hub that everyone pays (in the reference record a single settlement account
  carried a large share of all transfers) only receives credits: commutative, cheap, unordered. A
  treasury that pays everyone is a hot **debit** source and caps at one shard's rate: split it into one sub-account per shard,
  funded by the operator.

**Ceiling.** Linear in shards for the share of commands that are shard-local, plus one cheap
credit per cross-shard transfer: N shards × one writer's rate × the share that stays
local, with each cross-shard transfer costing its destination one credit (no debit check, no
reservation index; *estimated* at a third of a command). One writer is one core host process on
one CPU (execution is single-threaded), so a 16-vCPU host carries about 12 shards with room for
ingress.

| Writer's rate | 16 shards | 64 shards | 256 shards |
|---|---:|---:|---:|
| ≈ 30 a second (batches, the binding fixed) | ≈ 400 | ≈ 1 600 | ≈ 6 500 |
| ≈ 500–900 a second (compiled execution) | ≈ 7 000–12 000 | ≈ 28 000–50 000 | ≈ 110 000–200 000 |

(*estimated*: each figure is N × rate × 0.85, for cross-shard credits and coordination; the
reservation-based multi-leg protocol costs two commands per source shard.)

**VOS changes.** Several Agents in one space, each its own single writer; Agent-to-Agent
messages after commit, delivered exactly once to the Merge lane (VOS has `MAX_AFTER_COMMIT_MERGE_
MESSAGES = 256` today, and the Merge lane exists, but delivery between Agents with an idempotency
guarantee for a ledger is to be designed with @olanod (VOS's maintainer)); per-shard Shared Agents for `M-4`.

## 5. Ingress fan-in for millions of concurrent clients

**Connections are not commands a second.** A million agents that each act ten times a day are
10 million commands a day: 116 a second on average, perhaps 1 200 a second at a tenfold peak. The
same million agents holding connections open is a million idle sockets, which costs memory at
the edge, not throughput in the book.

**What the edge needs.**

- **Stateless ingress replicas behind a load balancer**, every one accepting commands into the
  received-log (PostgreSQL), one leader forwarding (`F-009`). An ingress replica on axum serves
  thousands of requests a second; the received-log's inserts are group-committed by PostgreSQL.
  *Measured here:* one ingress with the in-process core on a 2-vCPU runner (the stress runs).
- **Authentication without the core.** An API key today is checked by reading its verifier from
  the core on every request (`credentials::authenticate`), and the core host answers reads under
  the same lock as invocations: every API-key request waits for the invocation in flight, up to
  one invocation's time (finding F4 of the stress runs). A verifier cache in ingress, invalidated by
  the core's credential events (the core still checks membership at decision, `REQ-AU-3`), takes
  the core off the authentication path. Sessions already are.
- **No polling.** A client that polls for its decision multiplies reads; notifications (pushed,
  `REQ-EV-1`) or a long-poll on the operation reference serve millions of waiting clients from the
  edge.
- **Rate limits per principal**, so that one agent cannot fill the received-log for everyone.
- **A recipient per principal through the surface.** Today recipients are configured by hand
  (`aledgent-ingress recipient`); a principal with none accumulates undelivered notifications
  forever (finding F5). Millions of agents need registration through the API, or delivery to their
  integrating party.

**Ceiling.** The edge scales out; the received-log is one PostgreSQL primary: tens of thousands of
small inserts a second with group commit (*estimated*, typical of a gp3-backed instance; to be
measured on the deployment's own instance class). The book remains the
limit: §§2 to 4.

**VOS changes.** None.

## 6. The steps together

| Step | Ceiling (commands a second) | Status | Needs from VOS | Needs from aledgent | Needs from the spec |
|---|---:|---|---|---|---|
| Today: one command an invocation, recompiler | **3.6** | measured | — | — | — |
| Today, as the canary is pinned (interpreter) | **≈ 0.3** | measured (`MEASUREMENTS-F005.md` §9: 3.15 s) | — | — | — |
| The in-process core (`DEC-1`, native) | see the stress runs | measured | — | — | none (`DEC-4` ruled the Agent) |
| 1. Batches, bounds raised | ≈ 14 | measured (13.9) | bounds: gas, message, availability, heap, state | a batching forwarder | `NFR-16` lane for decided commands (`s-06`) |
| 1′. Batches, the binding's root recomputed once | ≈ 30 | *estimated* from the fit | the same | `WitnessStore::commit` fix | — |
| 2. State over external blocks | ≈ 30, larger batches | *estimated* | the external-state runtime for Agents, qualified | the binding over committed maps | — |
| 3. Native curve arithmetic | ≈ 60 (*assumption-bound*) | *estimated* | a native handler for ECALLs 110–114 | — | — |
| 4. Compiled Agent execution | ≈ 500–900 per writer | *estimated* from the native decision | `T-025-06` (designed with @olanod) | — | — |
| 5. Sharding by account | N × the writer's rate × ≈ 0.85 | *estimated* | Agent-to-Agent Merge delivery, per-shard Shared Agents | shard routing, transit positions, the reserve-then-capture coordinator, the root of roots | **an amendment**: `REQ-RT-1`, `INV-25`, guardrail 8, `US-B1`, `US-B3` (`s-07`) |
| 6. Ingress fan-in | the edge scales out; the received-log ≈ 10⁴ a second (*estimated*) | partly measured | — | verifier cache, long-poll, recipients through the API, rate limits | recipients per principal (`s-04`) |

**Reading it against the owner's target.** Millions of concurrent agents acting a few times a day
are a few hundred to a few thousand commands a second at peak. A single VOS Agent writer does not
get there by any step in this table short of compiled execution; with it, one writer reaches the
low thousands only in the most favourable estimate. Sharding is the only step that scales with
hardware, and it is the one that changes the spec. The native core already sits where step 4 would
take the Agent: that is the trade-off `DEC-4` ruled on, and the stress runs' numbers are the
cost of that ruling at this load (question `s-02`).

## 7. The prover (`F-030`) at any of these rates

The canary's core is to run VOS's prover over the core Agent's transitions (`DEC-21`,
`F-030`). VOS's prover extension refuses a trace above `TRACE_GAS = 100 000 000`
(`extensions/prover/src/lib.rs`), and the core's own work for one reservation is about 295 M gas
on today's core (`notes/MEASUREMENTS-F005.md` §9): **one transition does not fit**. Chained
proving streams segments; VOS's own measurement of a 7.8 M-step conservation transition is about
4 s per 32 k-step window at 5.2 GiB (from VOS's roadmap of 2026-07-14):
about 9 000 windows, some ten hours, per reservation, on one machine (*estimated*). At 3.6
commands a second the prover would fall behind by a factor of about 130 000. Proving every
transition is not viable at any rate in this document; proving anchors (a sampled or periodic
proof of the checkpoint's transition chain), off the critical path, is (question `s-08`).
