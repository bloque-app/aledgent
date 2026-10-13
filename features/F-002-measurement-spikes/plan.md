<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-002 — Measurement spikes

| | |
|---|---|
| Milestone | M-0 — Foundations |
| Track | A |
| Home | `spikes/` (code, never shipped). Results in `notes/` |
| Depends on | `F-001` (stores and builders for S3). `DEP-1` and `DEP-5` (exposure, not blockers) |
| Unblocks | `F-006` (its shape follows `DEC-4` as confirmed or revised), `F-004` (holding-position choice), `F-005` (invocation budget) |
| Realises | tests `DEC-4`. Gives inputs to `DEC-2` |

## Goal
We have measured numbers, not estimates, for the three costs that decide the core's shape: the kernel
per posting, a witnessed-leaves invocation, and a VOS Local Agent invocation as state grows. The
numbers are in `notes/`, with method and raw data. `DEC-4` is then confirmed, or the owner has a
revision grounded in them. `NFR-3` and the reservation's share of completion (`NFR-15`) get their
first real numbers.

## Spec traces
- `NFR-15`: completion, which the reservation decision is part of. The spikes measure its parts.
  *Amendment 0007 (2026-09-29):* this line said "`NFR-1`: the reservation decision at 50 ms p99";
  that was a misreading. `NFR-1` bounds the acknowledgement of a command, and the decision has no
  budget of its own. The decision rules below were written against the misread budget and are kept
  as the record of how `DEC-4` was decided.
- `NFR-3`: a balance read at 5 ms p99. First numbers for a leaf read at the three sizes.
- `NFR-2`: sets the growth assumptions for the book sizes and holding positions.
- `DEC-4`, `DEP-1` (the PLAN traces of this feature).

## Scope
**In:** four measurements (S1–S4), one reference host, a result format, pre-stated decision rules,
and a written result with a recommendation on `DEC-4`.
**Out:**
- The production leaf store and its 150-million-entry read test (`F-006`).
- Settlement-layer finality and inclusion (see Open points).
- Tuning of any kind.
- Shipping code: `spikes/` is never a dependency of a shipped crate.

## Design

**Reference host and pins.** All runs use one named machine class, the one the core is expected to
run on, with its CPU, memory and disk recorded. Pins: CipherClerk `e9a7b08`, VOS `saga/agents` at
`7bae926`. Builds are release profile. Each figure is reported as p50, p95, p99 and
max, after warm-up, over at least 1 000 samples. Raw samples go to `spikes/results/<spike>.csv`.

**Posting shapes measured.** These are the shapes the core will actually submit, built with
`F-001`'s builders:

| Shape | Entries | Why |
|---|---|---|
| transfer | 2 | `US-B1` |
| reserve with a new holding account | 1 creation + 2 | `DEC-2`, per-operation option |
| reserve into a pooled holding account | 2 | `DEC-2`, pooled option |
| capture for less | 3, with `CLOSING_DEBIT` | `F-004` |
| two-asset conversion | 4 | `REQ-KER-2` |
| linked chains | 2 and 10 events | `REQ-KER-10` |

Batch sizes are 1, 4, 10 and 64. CipherClerk defines bench sizes 1, 4 and 10 but has recorded no
results.

**S1 — kernel, native.** Criterion benches of `Book::apply` over each shape on the reference host.
Each posting's time is split into signature verification, commitment checks, negativity openings
and root update. The only published numbers are dev-profile ones: about 480 µs for a two-entry
transfer and about 177 µs per signature check. S1 replaces them.

**S2 — kernel in the PVM guest.** The same shapes, built for the VOS guest target (the clerk actors
build for `riscv64em-vos`). They run inside a Local Agent actor that applies a
pre-built unit to an in-state `MemStore` of small size. We record `gas_used` and `execution_us` from
the runtime's debug tracing:

- **Without the curve precompile.** The estimate is 10–20 million cycles per transfer.
- **With `pvm-precompile`** (`ecalli 200` into zkpvm's Ristretto chip; estimate
  50–200 thousand cycles). The precompile is a request to the zkpvm team, not a shipped dependency.
  If the host cannot run it, we record "not available", with the build error or the host's refusal.

S2 also records whether the kernel's `signing` feature builds for the guest (`F-001` Open points).

**S3 — witnessed-leaves invocation.** We build books with 10 000, 100 000 and 1 000 000 accounts in
`F-001`'s `FileStore`, with history. Activity per account follows a Zipf distribution, because
`NFR-3` says to assume extreme skew. For one transfer and one reserve of each `DEC-2` option, we
measure four things:

1. **Witness size.** The touched leaves, plus one batch proof per sub-tree. This is the shape of
   CipherClerk's succinct witness and of `clerk-witness`.
2. **Build time** on the host.
3. **Verify-and-apply time**, native and in the guest.
4. **Root-after derivation** from the proofs alone.

We compare the sizes with the host's limits:

| Limit | Value |
|---|---|
| invocation message | 16 KiB |
| reply | 16 KiB |
| inline fields per machine | 48 KiB |
| whole Agent state | 4 MiB |
| row value | 64 KiB |

S3 also records:

- **Leaf-store growth** after 12 months at `NFR-2`'s volume, under each `DEC-2` option. The
  per-operation option adds one account leaf per reservation, and nothing deletes it.
- **Hot-state size** for roots, the open-reservation index and bound counters at those volumes.
- **Account leaves that fit in 4 MiB.** notes/FIT §1 estimates about 12 000.
- **Point-read latency** of one account leaf from `FileStore` at each size. This is a first number
  toward `NFR-3`, not a test of it.

**S4 — one Local Agent invocation.** A minimal `#[actor(agent)]` actor holds a Linear blob of about
64 KiB, 1 MiB, 2 MiB or 4 MiB. Its `#[msg(linear)]` method rewrites 64 bytes. We send at least 1 000
sequential invocations through the canonical invocation path of `vosx`. We record:

- end-to-end latency at the caller;
- per dispatch: `queue_wait_us`, `service_us` and `payload_bytes`;
- per runtime execution: `elapsed_us`, `preparation_us`, `execution_us` and `input_bytes`.

These are the debug events the branch emits but nobody has recorded. The 64 KiB
case separates the fixed cost from the cost per byte of state moved. The branch says whole-state
transport is the dominant, unfixed cost. S4 puts a number on it.

**Decision rules.** These are written before any run, so the result cannot shape them. The
reservation-path budget is S2 (reserve shape) + S3 (verify-and-apply at 1 000 000 accounts) + S4
(at the state size the design would carry: roots and hot state from S3).

| Result | Recommendation to the owner |
|---|---|
| The witness fits the invocation limits, the budget p99 ≤ 50 ms, and S3 shows the book cannot live in 4 MiB | Confirm `DEC-4` |
| The witness exceeds the message limits, but the budget holds | Keep `DEC-4`, and carry the witness as a private input or in rows. Raise it with @olanod (VOS's maintainer) |
| The budget exceeds 50 ms | `RISK-2` fallback: a smaller hot set plus a cold store. Escalate `DEP-1`. `NFR-1` is not relaxed |
| Precompile unavailable, and the budget exceeds 50 ms only without it | Record the dependency on the precompile. Raise it with @olanod and the zkpvm team |

The recommendation on the holding-position option goes to `F-004`, which decides.

**Output.** `notes/MEASUREMENTS-F002.md` holds the host, pins, method, every table, the rule applied
and the recommendation. It also has a short section for @olanod, since it answers part of an upstream question.
`spikes/README.md` explains how to reproduce each run.

## Test plan
The spikes verify no `AC-*` and no `INV-*`. They are measurements. Their checks are these:

- **Reproducibility:** each spike re-runs from `spikes/README.md` on the reference host, and its p50
  is within 10% of the recorded one.
- **Correctness of what is timed:** each S1 and S2 unit asserts its `Applied` results and root,
  using `F-001`'s golden vectors. S3 asserts that the root derived from the witness equals
  `FileStore`'s root after commit.
- **Honesty:** every table states its sample count, profile and pins. A missing measurement is
  written as missing, with its reason.

## Exit criterion
From PLAN §4: *numbers recorded in `notes/`, and `DEC-4` confirmed or revised.* Made concrete:

- `notes/MEASUREMENTS-F002.md` exists, with S1–S4 at the stated sizes (or an explicit "not
  available" with its cause).
- The decision rule has been applied.
- The owner has recorded a ruling on `DEC-4` (PLAN `M-0` gate).
- `F-004` has the numbers it needs to choose its holding option.

## Open points
- **Four measurements.** PLAN §7 speaks of four measurements. notes/FIT §11 lists four, and the
  fourth is settlement-layer finality and inclusion under load. PLAN §4 does not give that one to
  `F-002`. Here, the fourth is S3's growth and read numbers, and finality is left to
  `F-016`/`F-019`. The owner should confirm, or move finality here.
- **Target machine.** "The target machine" in PLAN §4 is read as both the reference host (S1) and
  the PVM guest (S2). If only one was meant, one spike can be dropped.
- **Precompile.** It may not be runnable on `7bae926`. The spike records its
  absence rather than a guess.
- **Proposals to review:** Zipf skew for `NFR-3`'s "extremely skewed", 12 months as the growth
  horizon, and the reference host class.
- Exposure: `DEP-1` (the answer may change `DEC-4` regardless). `DEP-5` (if `master` is chosen, S4
  must be re-run there). `RISK-1` (limits may move, so the spikes are kept runnable). `RISK-2` (this
  feature is its mitigation).
