<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-032 · Withdrawals under load: where the time goes (T-032-12)

Gate item 5, finding F9 (the stress workflow's run summaries): in scenario c every withdrawal settles once, but in 36–45 s
(p50 42 s, run 37166637190), above `NFR-15`'s 30 s ceiling. This note breaks that time into phases,
says what aledgent cut, and what is left: the settlement layer's own inclusion and finality.

## How it was measured

Scenario c on the zombienet fleet (`e2e-zombienet.yml`, `driver: aledgent`,
`stress_phases` at ½ ×, 1 × and 2 × the reference hour, 300 s each): Kreivo 0.17.0 (spec 133) on
`polkadot-stable2606-2`, rococo-local with two validators, the adapter over its light client.
Marks added by T-032-12, all always on:

- the adapter's step log carries the finalized and best heads it read; each observation and each
  provisional check is logged with its block; the light client's transaction watch logs
  `broadcast`, `in-best-block` and `in-finalized-block` with the time since submission;
- the harness records wall-clock marks per withdrawal (`requested_at`, `submitted_at`,
  `settled_at` in `stress/withdrawals.json`) and polls the state every 100 ms (it polled every
  3 s, which added up to 3 s to each figure in the earlier run summaries);
- the workflow records the chain clock, every best and finalized head of the collator and of the
  relay chain (`stress/heads.jsonl`, `e2e/scripts/heads.mjs`), and keeps the collator's log;
- `e2e/scripts/withdrawal-phases.py <artifact>/aledgent` prints the table below from them.

## The breakdown

Milliseconds, p50 / max over each run's withdrawals. Before: run
37180398923 (instrumented,
adapter unchanged, 15 withdrawals). After: run
37182795151 (the cut below, 16
withdrawals; step 4's five fault cases pass on it).

| Phase | Whose | Before p50 / max | After p50 / max |
|---|---|---|---|
| Ingress acknowledgement, the core's decision (reservation), `submit_outbound` | aledgent | 34 / 100 | 32 / 70 |
| Adapter picks the instruction up from the outbox, signs, broadcasts | aledgent (adapter) | 1 790 / 2 147 | **486 / 718** |
| Light client to the collator's pool | network | < 50 (same second in the collator's log) | < 50 |
| Pool to the block that includes it (2 s blocks) | the collator | 12 346 / 14 481 | 11 999 / 13 071 (4 of 16 in 54–1 703) |
| That block imported to finalized, at the collator | the settlement layer's finality | 23 449 / 26 977 | 25 614 / 27 127 |
| Finalized at the collator to finalized in the light client | light client | 730 / 1 800 | 888 / 1 835 |
| Light client's finalized block to the check reported to the core (core decides `Settled` in it) | aledgent (adapter) | 1 249 / 2 495 | **420 / 783** |
| Core's `Settled` seen by the harness (notification and query) | aledgent | 35 / 94 | 26 / 93 |
| **Request to `Settled`** | | **40 389 / 41 690** | **39 086 / 41 595** |
| of which aledgent's (rows marked aledgent) | | 3 183 / 4 738 | **1 047 / 1 365** |
| of which inclusion and finality | | 35 911 / 37 568 | 37 608 / 38 842 |

The chain clock over each whole run (every block, not only the withdrawals'):

| | Before | After |
|---|---|---|
| Kreivo block time at the collator (p50 / mean) | 2.0 s / 2.5 s | 2.0 s / 2.5 s |
| Kreivo finality lag, best import to finalized (p50 / p90 / p99) | 25.3 / 37.2 / 39.6 s | 25.6 / 35.4 / 37.8 s |
| Relay block time | 6.0 s | 6.0 s |
| Relay finality lag (p50 / p99) | 16.0 / 18.0 s | 16.1 / 16.8 s |
| Kreivo's finalized head advances | every 8 s (p50) | |

Kreivo **mainnet**, the same chain clock through a public full node for 10 minutes
(2026-10-04, 287 blocks): block time p50 2.0 s (mean 2.1 s); finality lag p50 **31.8 s**, p90
36.0 s, p99 **49.5 s**, max 51.5 s.

## What was cut

**The adapter now steps on news, not only every interval** (`Adapter::news`, `wake_poll`). It
stepped every `interval` (2 s in the suite and the canary, plus the step's own ~0.4 s): a
withdrawal waited up to one interval to be signed after the core put it in the outbox, and up to
another to be observed final after the light client had the finalized block. Neither wait is
required by finality. The process now checks every `wake_poll` ms (default 250; 0 restores the
fixed interval) whether the finalized or best head moved or a call arrived in the outbox, and steps
at once if so; `interval` stays the longest gap between steps. A check is two head reads from the
light client and one read of the outbox: nothing is proved or dry-run. A step's work is unchanged,
and settlement still waits for the finalized inbox (`INV-10`): the check that makes an operation
final is the same, it only runs sooner. Pick-up fell from 1.8 s to 0.5 s at p50, observation from
1.2 s to 0.4 s; aledgent's whole share of a withdrawal from 3.2 s to 1.0 s (max 4.7 s to 1.4 s).

**The harness's 3 s poll** is replaced by a 100 ms poll in scenario c; it was measurement, not the
system, and inflated the earlier runs' figures by up to 3 s.

Not cut, and why:

- **Broadcasting also through the dry-run node's RPC.** The transaction reaches the collator's pool
  within the same second through the light client already; a second path would buy nothing.
- **Observing at the best block instead of the finalized one.** That is `INV-10`: no entry against
  an external counterparty before finality. The provisional observation already happens at
  inclusion (12 s in) and moves nothing.

## What is left: inclusion and finality

On zombienet, inclusion and finality are 37.6 s of the 39.1 s (p50 after the cut).

- **Inclusion, 12 s at p50, bimodal.** When the transaction reaches the pool just before a slot's
  burst of three blocks that all become best, it is included in 0.05–1.7 s (4 of 16 after the cut,
  whose withdrawals then complete in **27.5–29.0 s**). Otherwise the collator builds 3–4 blocks
  without it while it sits in the pool: blocks on a fork (`🆕 Imported`, not best) and blocks after
  "Timeout fired waiting for transaction pool at block" (87 of 781 blocks in the first run). That
  is the zombienet collator's fork-aware pool with slot-based authoring (three blocks per relay
  slot, one PoV), not aledgent. Whether mainnet collators show it is not measured here; the
  withdrawal's own era, nonce and fee are not involved (the pool holds it ready throughout).
- **Finality, 23.4–25.6 s at p50 after inclusion.** On rococo-local with two validators the relay
  chain finalizes 16 s behind its best block, and a Kreivo block is final once the relay block that
  includes it is (backing and inclusion take one to two relay blocks more). Kreivo's finalized head
  moves every 8 s.

**Finality alone leaves too little of the 30 s, and on mainnet exceeds it.** On zombienet, finality
after inclusion is 23–27 s for every withdrawal, which leaves 3–7 s for inclusion and everything
else; with the minimum inclusion seen (0.05 s) a withdrawal completes in 27.5 s, with the usual one
(12 s) in 39–41 s. On Kreivo mainnet the finality lag alone is 31.8 s at p50 and 49.5 s at p99:
no adapter can meet `NFR-15`'s 30 s at p99 there for a command whose completion includes the leg's
finality. aledgent's own share is now about 1 s. The owner question (`gq-nfr15-finality`) is whether to
state the settlement layer's finality separately, exclude it from the bound, or raise the ceiling.
