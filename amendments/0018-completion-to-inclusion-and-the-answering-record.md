<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0018 — completion to inclusion for an external leg, and the answering setting's record

**Status:** ruled by the owner on 2026-10-04: completion is measured to inclusion, not finality; and
ingress's log is enough for the answering setting, because it changes no balances. No question is left
open. Applied to `SPEC.md` v0.4.11.
**Target:** `SPEC.md` v0.4.10 (amendments 0001 to 0016 applied; `0017` is the experimental draft of
#163, not applied); this one takes it to **v0.4.11**.
**Date:** 2026-10-04
**Plan:** `PLAN.md` v0.8.4: §5 (the stress gate), `DEC-5`, `DEC-22` ("Replication"), `DEP-11`, `F-021`.
**Rationale in one line:** the settlement layer's finality is not aledgent's to shorten, and on the
intended layer it alone exceeds `NFR-15`'s ceiling, so a command with an external leg completes, for that
bound, when its leg is included; and a setting that changes no balance and no authority is attributed and
kept, not entered in the audit trail.

---

## The problem

**`NFR-15` and the settlement layer's finality.** `NFR-15` ends a command with an external leg at the
hand-off: the leg final on the settlement layer, the operation terminal, the outcome relayed. The stress
suite (`F-032`, scenario c) measured where that time goes. Of 39.1 s at p50 from request to `Settled`,
aledgent's share is about 1.0 s; the settlement layer's inclusion and finality are 37.6 s. Finality alone,
on the layer the deployment is planned on, is 31.8 s at p50 and 49.5 s at p99. No implementation of
aledgent can meet 30 s at p99 for such a command while the bound includes finality. A bound nothing can
meet is not a bound: it would breach on every withdrawal and measure the settlement layer, not aledgent.

**The answering setting's record.** `REQ-CS-9` says setting a principal's answering setting is
"attributed and audited". The setting changes when a reply carries a decision, never the decision, a
balance or an authority. The owner was asked whether it belongs in the audit trail (`REQ-AT-1`), and ruled
that it does not: its own record, with who set it and when, is enough.

## What does not change

- **`INV-10`.** No entry against the issuance counterparty for an external leg before that leg is final.
  Nothing is posted at inclusion; the settling posting still waits for `Final`.
- **`REQ-IN-7`.** Confirmation, finality and settlement stay three conditions. `Observed` records a
  confirmation, which can still be withdrawn; the caller is told so (`REQ-OP-12`).
- **The ceiling and the target.** 30 s at p99 and 19 s at p99, for every command, as before. Only where a
  command with an external leg stops the clock changes.
- **`REQ-AU-7`.** Setting the answering setting is a use of authority, and records which principal used
  which authority.

---

## R1 — `NFR-15` (revised): completion to inclusion for an external leg

Before:

> | `NFR-15` | A command MUST complete within **30 s at p99**, measured inside aledgent from its
> acknowledgement, or from its arrival for a command answered with its decision (`REQ-CS-8`). It completes
> when its operation reaches the state the command asks for, or is refused: `Reserved` for a reservation,
> and a terminal state for every other command, including one with an external leg. For a command with an
> external leg, completion ends at the **hand-off**: the leg is final on the settlement layer, the operation
> has reached its terminal state, and the outcome has been relayed to the requester (`REQ-EV-1`). Whatever
> follows the hand-off is outside aledgent and outside this bound. That includes any further movement of the
> value beyond the settlement layer, and the requester's own processing. An operation in `Needs-Review` has
> not completed. 30 s is the ceiling: alerts and conformance breach on it. The target is **19 s at p99**: it
> is tracked, and missing it is not a breach. |

After:

> | `NFR-15` | A command MUST complete within **30 s at p99**, measured inside aledgent from its
> acknowledgement, or from its arrival for a command answered with its decision (`REQ-CS-8`). It completes
> when its operation reaches the state the command asks for, or is refused: `Reserved` for a reservation,
> `Observed` for a command with an external leg, and a terminal state for every other command.
> For a command with an external leg, completion ends at **inclusion**: the leg is included on the
> settlement layer, the operation has reached `Observed` (a confirmation, §2), and that state has been
> relayed to the requester (`REQ-EV-1`). If the operation reaches a terminal state without its leg being
> included, it completes there. Inclusion is not finality (`REQ-IN-7`): nothing is posted against the
> issuance counterparty before the leg is final (`INV-10`), and the operation goes on to `Final` and
> `Settled`.
> The time from inclusion to the **hand-off** (the leg final on the settlement layer, the operation at its
> terminal state, and the outcome relayed to the requester) is not bounded by this requirement: most of it
> is the settlement layer's finality, which aledgent cannot shorten. It is measured and reported on every
> release with `NFR-18`, split into the settlement layer's time to finality and aledgent's time from
> finality to the relayed outcome. Whatever follows the hand-off is outside aledgent. That includes any
> further movement of the value beyond the settlement layer, and the requester's own processing. An
> operation in `Needs-Review` before it completes has not completed. 30 s is the ceiling: alerts and
> conformance breach on it. The target is **19 s at p99**: it is tracked, and missing it is not a breach.
> *Completion to inclusion ruled by the owner on 2026-10-04 (amendment 0018).* |

**Reading.** An inclusion the settlement layer later withdraws (`Observed` to `Orphaned`, §5.6) does not
undo the completion already measured; the re-inclusion falls in the time to the hand-off, which is
reported. A command refused, or a leg released or expired before inclusion, completes at that state as
before.

## R2 — §11, the paragraph on latency

After "Deciding a reservation is part of its completion; it has no budget of its own.", §11 gains:

> For a command with an external leg, completion ends at the leg's inclusion; the settlement layer's
> finality after it is measured and reported, not bounded (`NFR-15`).

## R3 — `REQ-CS-9` (revised): the answering setting's record

Before, in "Who sets it":

> Setting it is an account and security command: it is answered with its decision, attributed and audited
> (`REQ-CS-8`, `REQ-AU-7`).

After:

> Setting it is an account and security command: it is answered with its decision and attributed
> (`REQ-CS-8`, `REQ-AU-7`). The setting is kept with the principal that set it and when, and every change
> is logged. It is not a record of the audit trail (`REQ-AT-1`): it moves no value, changes no balance and
> grants or revokes no authority. *Ruled by the owner on 2026-10-04 (amendment 0018).*

---

## No invariant is affected

| Invariant | Why it holds |
|---|---|
| `INV-10` | Completion moves earlier; postings do not. The settling posting still follows finality. |
| `INV-11` | Inclusion is an observation at a coordinate, recorded by the settlement adapter as before; no caller asserts it. |
| `INV-19` | The audit trail and what an anchor covers are unchanged. The answering setting was never in either. |

## What this costs, stated plainly

- **One more figure, not one more bound.** A withdrawal's time to finality and to its relayed outcome is
  reported on every release, and nothing alerts on it. A slow settlement layer shows up in that report and
  in the age of the oldest non-terminal operation (`NFR-11`), not as a breach of `NFR-15`.
- **A caller that waits for `Settled` waits longer than 30 s.** On the intended settlement layer, about
  half a minute more at p50 and close to a minute at p99. That is the layer's finality, stated, not hidden.
- **The answering setting's history lives where the setting lives.** A reviewer reads it there, not in the
  audit trail. If a later requirement needs it in the trail, setting it becomes a command of the book.

## Recorded in the plan, from the same day

Two rulings of the same day are plan-level only, and change no requirement. They are recorded here for
the trail:

- **The burst bound** (the owner, 2026-10-04). The stress gate's burst is measured against the sharded capacity under
  test, many small execution nodes across several hosts, not against one core. The multiple (50 × the
  reference load's peak minute) and the drain bound (`NFR-15`'s ceiling) apply to the deployment's measured sharded
  capacity. `PLAN.md` §5.
- **The checkpoint archive** (the owner, 2026-10-04): an S3 sink first, an archiver later.
  The off-node archive a fresh replica installs from (`NFR-21` (b)) is an object-store sink first, and a
  stateful archiver node later. `PLAN.md` `DEC-22`, `DEP-11`, `F-021`.

## Carried to the plan

- **`F-032`** reports scenario c against `NFR-15` to `Observed` relayed, and the time from `Observed` to
  `Settled` on its own, split into finality and aledgent's share (`T-032-21`). The stress gate's
  withdrawal item is judged that way (`PLAN.md` §5).
- **`F-009`** keeps the answering setting in ingress's store, with who set it and when, and a log line on
  change, as built (`T-009-18`); its note 30 records the ruling.
