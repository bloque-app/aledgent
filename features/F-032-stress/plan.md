<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-032 — Stress suite

| | |
|---|---|
| Milestone | M-3 — Production V0 (a go/no-go gate before the production window) |
| Track | B |
| Home | `e2e/aledgent-driver/src/stress/`, `e2e/stress/`, `.github/workflows/stress.yml`; `agents/core-host` (`batch-curve`) |
| Depends on | F-005, F-009, F-016, F-017, F-018, F-026, F-027, F-028 |
| Unblocks | The production window (the stress gate in `PLAN.md` §5) |
| Asked by | The operator, before the production window |

## Goal

Before the production window, show with numbers what aledgent sustains, where it breaks, and what
breaks first: the acknowledgement, the decided reply and completion against `SPEC.md` §11, error
rates, the backlog, and the memory of the core and ingress, under a workload shaped like an
operator's and scaled well beyond it. Every target is a multiple of a reference load measured on
the operator's own record (§2), and every result names the run that produced it (the stress
workflow's run summaries).

## Spec traces

- `NFR-1` — acknowledgement within 50 ms at p99, inside aledgent.
- `NFR-16` — a decided reply within 1 s at p99 (confirmed by amendment 0011).
- `NFR-15` — completion within 30 s at p99 (ceiling), 19 s (target).
- `NFR-2` — 2.5 million operations a month without redesign.
- `NFR-4` — no acknowledged operation lost: every one read back decided after each run.
- `NFR-5` — deployable without an outage of the book: the core restarted under load.
- `NFR-11` — the backlog, the oldest undecided command, undelivered notifications: observed.
- `INV-5` — the trial balance after every run.
- `REQ-CS-7` — `ERR-DecisionUnavailable` counted as an error, and retried under the same client key.

## 1. What exists already, and what this adds

| Before | What it measured |
|---|---|
| `notes/MEASUREMENTS-F002.md`, `-F005.md` | One reservation through the Agent, one at a time: 2.5 s (interpreter, M2), 284 ms p50 / 344 ms p99 (recompiler, EPYC) |
| `features/F-009-ingress-process/NOTES.md` | A chaos run of 100 operations through ingress and the in-process core, ten core restarts |
| `.github/workflows/x86-bench.yml` | The reservation path on x86-64, interpreter against recompiler |
| `features/F-016-settlement-adapter/plan.md` §Throughput | ~0.105 operations a second per signing lane on a previous chain operator |

None of them drives ingress with a mix at a rate, and none measures the decided reply, completion
under load, the backlog or memory. This feature adds:

- **the harness** (`aledgent-e2e-driver stress`, in the zombienet driver's crate): an open-loop
  generator at a planned rate, a realistic mix (§3), and a report (`report.json`, `summary.md`);
- **a throwaway deployment** (`e2e/stress/run.sh`): ingress on PostgreSQL, the in-process core or
  the core as a VOS Agent (`aledgent-core-host serve`), test secrets drawn per run, a recipient for
  the operator's notifications, and fault injection;
- **the workflow** (`.github/workflows/stress.yml`): GitHub-hosted x86-64 runners, manual, or a
  matrix pushed to a `stress-runs/**` branch;
- **scenario c on the zombienet fleet** (`e2e-zombienet.yml`, input `stress_phases`): the same
  harness inside the zombienet driver while withdrawals settle through the adapter's own process;
- **the batch curve** (`aledgent-core-host batch-curve`, x86 bench mode `batch-curve`): one
  invocation of the core Agent against the commands it carries, 1 to 2 048, on the recompiler.

## 2. Targets, from the reference load

Every target is a multiple of a **reference load L**, read from the operator's own record of the
operations it carries to the settlement layer, read-only. That record is a proxy for the book's
operations: it holds every operation that moves value, and it is the closest record of them.

**How counted.**

- One logical operation per row, except a row that is one leg of a batch: a card purchase and its
  fee are one batch row plus one row per leg, and count once.
- The time of a row is its intake where it has one, else its terminal time. Rows with no usable
  time at all are left out.
- Only the operator's own rails.

**The reference.** **L is the operator's busiest observed hour**, in logical operations; **the
reference minute M is its busiest observed minute**. Beside them the record gives the busiest day
and month (set against `NFR-2`'s 2.5 million a month), the percentiles of operations per hour and
per minute, the number of ledger accounts, and how much of the traffic the busiest accounts carry
(the hot accounts of §3). The harness takes L and M as constants (`PEAK_HOUR_PER_MIN`,
`PEAK_MINUTE` in `e2e/aledgent-driver/src/stress/mod.rs`). **In this repository they are synthetic:
L = 300 commands a minute and M = 900 commands**, round values that stand for no operator's record.
An operator sets them from its own record. Results of runs against the plans are reported as
multiples of L and as shares of the offered load, never as absolute rates, so that an operator's
own L is not published with them.

**The shape of a peak.** A payments deployment's busiest hours tend to be bulk events: bulk
onboarding (an identity, an account activation and a top-up per customer), card spend (a purchase
with its fee legs), and a bulk disbursement. Bursts come from bulk submissions: a few hundred rows
can share one intake second.

**The headline (the owner's ruling of 2026-10-03).** No NFR states a throughput target for now. The
suite's headline number is the **greatest sustained commands a second realistically achievable
today**, per scenario and per lever, set against **10 000 a second** (an acceptable start) and
**1 000 000 a second** (the aspiration). *Sustained* means a phase of at least 90 seconds in which
every bound of §11 held at p99 (`NFR-1`, `NFR-16`, `NFR-15`), errors stayed at or under 1 ‰, and the
queue at the phase's end was under ten seconds of its load. The ladders below are how the load is
raised until that stops being true.

**Load levels** (as the operator asked: ten times the reference hour sustained, fifty times the
reference minute as a burst, a soak at twice the reference hour):

| Target | Load | For |
|---|---|---|
| Sustained: 10 × the reference hour | **10 × L** an hour | 10 minutes (`plan sustained`), and the ladder's top step |
| Burst: 50 × the reference minute | **50 × M** a minute | 1 minute, between two minutes at 1 × L (`plan burst`) |
| Soak: 2 × the reference hour | **2 × L** an hour | 4 hours (`plan soak`) |
| Ladder | ½ ×, 1 ×, 2 ×, 5 ×, 10 × L | 2 minutes each, stopping once the core falls behind |

Each level is held to the bounds of §11: `NFR-1`, `NFR-16`, `NFR-15` at p99, inside aledgent; at
most 1 ‰ errors (a `5xx`, no reply, or `ERR-DecisionUnavailable`); the backlog at the end of a phase
no more than ten seconds of its offered load; after the run, every acknowledged operation decided
and the trial balance zero.

*Trade-off, stated plainly:* the targets are set on a record that is a proxy (the operations
carried to the settlement layer, not the book), and rows with no usable intake time are left out:
a peak among them would not show. `OQ-41` (is `NFR-2`'s volume the right commitment) is still
open; the owner's order of magnitude is in the headline above (`throughput.md`).

## 3. The workload

A flow is one unit of the mix; each sends one to three commands. The scheduler starts flows at the
rate that gives the planned commands a minute, open loop (it never waits for replies). Reads and
passkey sign-ins ride on top.

| Flow | Share of flows | Commands | Stands for |
|---|---:|---:|---|
| Transfer: holder → holder (50 %), holder → merchant hub (30 %), treasury → holder (20 %) | 35 % | 1 | a transfer, a top-up |
| Purchase with its fee: holder → merchant and holder → fees, a third of them with a third leg | 10 % | 1 | a batched card spend |
| Card authorisation and settlement: reserve, then capture (one in five for less) | 15 % | 2 | card spend, reserved at authorisation |
| Authorisation that lapses: reserve, then release | 5 % | 2 | |
| Onboarding: identity (API key), account, top-up from the treasury | 15 % | 3 | account creation, account activation, a top-up |
| Issuance: mint into, or burn out of, the issuance pool, in COPM, TSTA, TSTB or TSTC | 5 % | 1 | an issuing client's mint and burn |
| Balance read | 13 % | (read) | assumed: no read volume is recorded |
| Passkey sign-in | 2 % | (ceremony) | `DEC-19` sessions |

- **Principals.** The operator is the deployment's service principal, as an issuing client is: it opens
  identities and accounts, issues and redeems, and (for `--service-pct`, 50 % by default) makes
  transfers and reservations on holders' accounts under an operator grant. The rest holders make
  themselves: every holder's first credential is an API key; 30 % also register a passkey and act
  through its 15-minute session, signing in again before it lapses.
- **Hot accounts.** A merchant settlement hub (credited by purchases and captures), a fee hub and a
  treasury (debited by top-ups), plus the issuance pool: in the reference record a single
  settlement account carried a large share of all transfers.
- **Notifications.** The operator's go to the harness's receiver, so the delivery worker and the
  delivery records it sends back to the core (`DEC-7`) carry load. A holder has no recipient (none
  can be registered through the surface): its notifications stay undelivered (§5, finding).
- **Population.** 200 funded holders by default, plus the hubs and the pool; onboarding adds more
  during the run.

## 4. Scenarios

| | Scenario | Where | How |
|---|---|---|---|
| a | The core as a VOS Agent, single node, book only | GitHub-hosted x86-64 (2 vCPU, the standard runner of a private repository) | `stress.yml`, `core: agent-recompiler` or `agent-interpreter`; the in-process core as the baseline (`core: inprocess`, also runnable on a laptop) |
| b | a with anchoring | same | `anchoring_ms` (hourly in production; a minute here, to see it under load). The prover is not integrated yet (`F-030`): see `throughput.md` §5 |
| c | The settlement adapter on a zombienet Kreivo, under book load | the `zombienet` fleet (self-hosted x86-64 runners) | `e2e-zombienet.yml`, `driver: aledgent`, `stress_phases`: the harness drives the book in the driver's process while a withdrawal settles through the adapter every 20 s. Binding an asset under load waits for `F-029` (not merged) |
| d | Faults under load | GitHub-hosted | `faults: true`: `kill -9` and restart of the core host (or of ingress with the in-process core), a PostgreSQL restart, `kill -9` and restart of ingress, a minute apart, under load. The adapter disconnecting under load is c's fault cases (`T-027-06`) |
| e | Soak | GitHub-hosted | `plan: soak`: 2 × the reference hour for four hours; memory growth per hour reported |
| — | The batch curve | the `zombienet` fleet | `x86-bench.yml`, `mode: batch-curve` |

## 5. Design notes

- **Measured inside aledgent.** The bounds of §11 are read from ingress's own series
  (`/metrics`, the latest 10 000 samples) at the end of each phase; the client's view, which adds
  HTTP, is reported beside them.
- **Backlog.** Ingress counts a decision for every core input: commands, ticks and delivery
  records. The backlog is commands + ticks + deliveries − decisions.
- **Loss check.** After the load, the harness waits for the backlog to drain (15 minutes by
  default), then reads back every operation it was acknowledged. Any still `Received`, or not
  found, fails `NFR-4`.
- **Test secrets.** `stress-config` draws every secret fresh per run, writes them to a `0600` file
  the run deletes, and never logs them (`REQ-AU-8`). The passkeys are the software authenticator
  of `authn`'s `testing` feature; no production image builds the driver.
- **Integers only.** Rates are commands a minute; latencies microseconds; percentiles nearest
  rank (`INV-7`; the workspace denies floating point).

## 6. The gate

`PLAN.md` §5 makes this suite a go/no-go gate before the production window. The window opens only
when, on the deployment's own hardware class and configuration (the core as the canary runs it):

1. the greatest sustained rate, measured on that hardware with the core the canary runs, is at
   least the reference hour (1 × L): what the window must carry on its first day. No NFR sets a higher number (the owner's ruling of 2026-10-03); the
   rate measured is reported against 10 000 and 1 000 000 a second;
2. `plan burst` drains within `NFR-15`'s ceiling after the burst, on the sharded capacity under test
   (many small execution nodes across several hosts, a core per shard), not on one core; one core's
   figure is reported (the owner, 2026-10-04, `g-03`);
3. scenario d loses nothing and recovers within the deployment's stated RTO;
4. `plan soak` holds memory flat (growth under 1 % an hour after the first hour);
5. scenario c settles every withdrawal under load, each completing within `NFR-15` at its leg's
   inclusion (`Observed`, relayed), with the time to finality and to `Settled` reported apart
   (amendment 0018, `T-032-21`); and binding an asset under load passes once `F-029` lands.

What the gate measured on 2026-10-03 is in the stress workflow's run summaries.
