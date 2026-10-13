<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0007 — acknowledgement and completion latency

**Status:** ruled by the owner on 2026-09-29, including the conflict it raised (`OQ-61`, ruled the same
day); applied to `SPEC.md` v0.4.1
**Target:** `SPEC.md` v0.4.0
**Date:** 2026-09-29
**Issue:** #98
**Rationale in one line:** `NFR-1`'s 50 ms was read as a budget for deciding a reservation. It was always a
bound on acknowledging a command; the time to decide is part of completion, which had no number at all.

## The problem

`NFR-1` said: "A balance-affecting decision (reserve or refuse) MUST complete within 50 ms at p99."
Amendment 0002 repeated it (`REQ-CS-3`: "`NFR-1` binds this decision and nothing else"), and the plan
built a feature (`F-025`), a risk (`RISK-2`) and a milestone gate (`M-3`) around reaching 50 ms for the
reservation decision inside the runtime. The owner has ruled that this was a misreading:

- **Acknowledgement.** What must happen within 50 ms at p99 is that ingress *accepts* a command, that is,
  acknowledges its receipt. That was the requirement all along.
- **Deciding a reservation within 50 ms is not a requirement.**
- **Completion** had no bound. It now has two numbers.

## The ruling (2026-09-29)

### R1 — Acknowledgement: 50 ms at p99

Ingress acknowledges any command (takes it in durably and returns its operation reference and state,
`REQ-CS-2`) within **50 ms at p99**, measured inside aledgent. `NFR-1` is revised to say so.

"Accepts" here means *acknowledges receipt*. It is not the state `Accepted` of §5.6, which is the
deciding component taking the command.

### R2 — Completion: a target of 19 s and a ceiling of 30 s at p99

A command **completes** when its operation reaches the state the command asks for, or is refused:
`Reserved` for a reservation, and a terminal state for every other command, including one with an external
leg. Completion is measured inside aledgent, from the acknowledgement.

- The **target** is **19 s at p99**. It is tracked, and missing it is not a breach.
- The **ceiling** is **30 s at p99**. Alerts and conformance breach on the ceiling.

An operation that goes to `Needs-Review` has not completed. New `NFR-15`.

**Where completion ends: the hand-off** (the owner's clarification, 2026-09-29). For a command with an
external leg, such as a withdrawal, completion ends at the hand-off. The leg is final on the settlement
layer, and aledgent has relayed the outcome to the requester (its terminal notification, `REQ-EV-1`).
Whatever happens afterwards is outside aledgent and outside the 30 s ceiling. That covers a further leg
that carries the value to another network, and the requester's own flow once it has the outcome. Finality on
aledgent's side is always local: once a withdrawal is handed off to the chain and its outcome relayed to
the requester, what follows is not aledgent's. `NFR-15` says so without naming any technology.

### R3 — No separate budget for the reservation decision

The reservation decision is part of completion (R2). It has no budget of its own. The numbers measured so
far are **observations, not requirements**:

| Observation | p50 | p99 | Host | Source |
|---|---:|---:|---|---|
| The reservation through the core Agent, recompiler, the current 2.17 MB core: the driver's round | 284 ms | 344 ms | AMD EPYC Genoa, x86-64 | run 36520579669, `notes/MEASUREMENTS-F005.md` §9 |
| The same, interpreter | 3.15 s | 3.32 s | same | same |
| An earlier run: the driver's round only, recompiler | 236.7 ms | 240.2 ms | AMD EPYC Genoa, recompiler | `notes/MEASUREMENTS-F002.md` §15.3 |

In run 36520579669 (2026-09-29), both engines used exactly 1 747 M gas per call, and all 227
executions matched between them. All of these figures sit far inside R2's target.

### R4 — Every stale 50 ms target is corrected through this amendment

Each place that stated 50 ms as a budget for the reservation decision now says what R1–R3 say:

| Where | Correction |
|---|---|
| `SPEC.md` `NFR-1` | Acknowledgement within 50 ms at p99 |
| `SPEC.md` `REQ-CS-3` | The last sentence ("`NFR-1` binds this decision and nothing else") now says the decision is part of completion (`NFR-15`) |
| `SPEC.md` `AC-C1.1` | Cites `NFR-15` instead of `NFR-1` for the reservation's reply |
| `SPEC.md` §11 preamble | The numeric targets are `NFR-1`, `NFR-3` and `NFR-15` (and `NFR-2`'s volume) |
| `PLAN.md` | `DEC-4`, `F-005`, `F-025`, `M-2`, `M-3`, `RISK-2`: the reservation's measurements are observations; the gates are `NFR-1` (acknowledgement) and `NFR-15` (completion) |
| Feature notes and plans | `F-002`, `F-004`, `F-005`, `F-009`, `F-025`; `F-018`'s alert rules gain the completion ceiling |
| `notes/MEASUREMENTS-F002.md` | A note at the top: its decision rules were written against the misread budget |
| `OVERVIEW.md` | The two lines that repeat the misread |

`F-018` had no latency alert: the owner's note that its thresholds held a 50 ms target does not match the
rules file, which never had one. It now has an alert on the completion ceiling (R2).

## The conflict R1 raised, and its ruling (`OQ-61`)

**R1 against `REQ-CS-3` and `AC-C1.1`.** `REQ-CS-3` (amendment 0002) said a command that reserves "MUST
return only after the reservation is durable", and `AC-C1.1` said its "response returns once the
reservation is made". The reply to a reservation therefore *was* its decision. R1 bounds the
acknowledgement of *any* command at 50 ms at p99, and R3 records the decision itself at about 300 ms. For a
reservation, both could not hold of the same reply. It was raised as `OQ-61`.

**Ruling (2026-09-29).** A reservation replies "accepted" at ingress within 50 ms at p99, like every
other command: its reply is `Received`, with its operation reference. Its decision, reserved or refused,
arrives asynchronously, through the channel other commands already use for outcomes: the operation query
(`operation(reference)`) and the requester's notification (`REQ-CS-6`, `REQ-EV-1`). Applied:

- `REQ-CS-3` is rewritten: a reserving command is acknowledged; its decision reaches the caller by query
  and notification; any command refused after its acknowledgement, whatever its kind, is recorded as
  `Received → Rejected` with the refusal named, and a repeat of the same request gets that refusal.
- `AC-C1.1` and `US-C1` say the same from the caller's side. `NFR-1` loses its caveat.
- `REQ-CS-7` and `ERR-DecisionUnavailable` now concern a command answered with its decision (an
  issuance, say), because a reservation is no longer one.
- The plan review's question ("only transfers can be acknowledged at `Received`") is resolved the
  first way it offered: the core records `Received → Rejected`. It does so for every command it takes in
  at `Received`: transfers, reservations and withdrawals.

`OQ-61` is closed.

### What changes for a client

For anyone integrating:

- **`POST /v1/reservations` and `POST /v1/withdrawals` now reply `202 Accepted`** with
  `{"operation_reference", "state": "Received", "terminal": false, "client_key"}`, within 50 ms at p99,
  even while the deciding component is down. They used to reply `200` with `"state": "Reserved"`, or a
  refusal (`422`, `409`, ...) after the decision.
- **The decision arrives by notification**, to the requester's registered endpoint: `Reserved`, or
  `Rejected` with the refusal named. It can also be read with `GET /v1/operations/{reference}`, which
  shows `Received` until the core decides, then `Reserved`, or `Rejected` with the refusal in its
  `error`.
- **A repeat of the same request with the same client key** returns `202` until the decision is made.
  After that it returns the decision: `200` with `Reserved`, or the refusal with its `4xx` status.
- **A reservation is not usable until it is `Reserved`.** A capture, a release or a `submit_outbound`
  that the core reaches before the decision is refused (`ERR-UnknownOperation`). Ingress forwards
  reservations first, but a client should wait for the decision before acting on it.
- `ERR-DecisionUnavailable` (`503`) is no longer returned for a reservation or a withdrawal.

## What this costs, stated plainly

- A reservation can take hundreds of milliseconds to decide without breaching anything. A payment network
  that needs its answer faster gets it from the deployment's actual performance, not from a bound.
- A caller that reserves must now wait for a notification, or poll, before it captures, releases or
  submits. The acknowledgement tells it only that the request is durably held.
- The 19 s target is not enforced. Only the 30 s ceiling pages.
- `F-025`'s work stops being a gate for `M-3`. It remains worth doing for cost and headroom.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `NFR-15` (completion: target 19 s, ceiling 30 s, at p99) |
| Raised and closed | `OQ-61` (the conflict R1 raised; ruled on 2026-09-29) |
| Revised | `NFR-1` (acknowledgement, 50 ms at p99); `NFR-15`'s end point for an external leg (the hand-off, clarified the same day); `REQ-CS-3` (rewritten); `REQ-CS-7`; `ERR-DecisionUnavailable`; `US-C1`; `AC-C1.1`; §11 preamble |

## Carried to the plan

- `F-009`: the acknowledgement and completion measurements, and their metrics. Completion of a command
  with an external leg is measured to the hand-off: the terminal state, and the delivery of its outcome
  notification. Today's metric stops at the terminal state of a command ingress sees finish, and does
  not yet cover an external leg that finishes later.
- `F-009`: reservations and withdrawals acknowledged at `Received`, with priority in the forwarder; the
  core records `Received → Rejected` for them (`Ledger::receive_command`).
- `F-018`: the alert on the completion ceiling.
- `F-025`, `RISK-2`, `M-3`: no longer gated on 50 ms for the reservation decision.
