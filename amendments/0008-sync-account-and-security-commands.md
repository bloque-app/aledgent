<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0008 — account and security commands are answered with their decision

**Status:** ruled by the owner on 2026-09-30; applied to `SPEC.md` v0.4.2. The number in R3 (`NFR-16`'s
bound) is **proposed** and waits for the owner's confirmation. The question R4 raises (`OQ-63`) was closed by the owner on 2026-10-02 (`d-oq63`, `f-oq63`).
**Target:** `SPEC.md` v0.4.1
**Date:** 2026-10-01
**Follows:** amendment 0007 (`NFR-1`, `NFR-15`, `OQ-61`)
**Rationale in one line:** after amendment 0007 every command looked as if it could be acknowledged at
`Received`. The owner has ruled that only commands that move value are; the account and security layer
always waits for its decision.

## The problem

Amendment 0007 made `NFR-1` a bound on the acknowledgement of *any* command, and `REQ-CS-2` still said
that the state a command returns "MAY be `Received`". Read together, any command could be answered
`202 Received`, with its decision left to a notification, and `NFR-1`'s 50 ms would then bind it.

The code never did that. After #103, ingress acknowledges three commands at `Received` (a transfer, a
reservation and a withdrawal) and answers every other command with its decision. `PLAN.md` `DEC-5` says
so ("Other commands are answered with their decision"). The spec did not: it neither required it, nor
bounded the time such a reply takes, nor said which commands are which. The plan's review
recorded the gap and stayed open on that last point.

## The ruling (2026-09-30)

### R1 — Commands that move value are acknowledged at `Received`

A command that moves value is acknowledged at `Received` (`202`) within 50 ms at p99 (`NFR-1`). Its
decision follows asynchronously, by query and notification (`REQ-CS-3`). This is what #103 built for
transfers, reservations and withdrawals; nothing changes for them.

### R2 — Account and security commands always answer with their decision

Account and security commands are answered **synchronously, with their decision, and never with `202`**.
They are the commands that:

- open identities, and open or close ledger accounts;
- register or remove credentials;
- grant or revoke authority, including issuance authority and operator authority;
- initiate, cancel or complete a recovery, name a successor or claim a succession;
- change custody (`delegate_custody`, `reclaim_custody`);
- and every operator command (`CTR-OPS`).

In the owner's words: "the account and security layer should wait for confirmation always. That'll
simplify a lot of stuff." A caller that changes who may act on an account learns at once whether the
change was made, and no change of authority sits half-known while a notification travels.

So `NFR-1`'s 50 ms bound applies to commands that move value only.

### R3 — A bound of its own for a command answered with its decision (proposed)

A command answered with its decision gets its own bound, `NFR-16`. **Proposed: 1 s at p99**, measured
inside aledgent from the command's arrival to the reply that carries its decision. A reply of
`ERR-DecisionUnavailable` is measured the same way and counts against the bound.

The basis:

| Observation | p50 | p99 | Host | Source |
|---|---:|---:|---|---|
| A reservation through the core Agent, recompiler, 1 747 M gas a call: the driver's round | 284 ms | 344 ms | AMD EPYC Genoa, x86-64 | run 36520579669, `notes/MEASUREMENTS-F005.md` §9 |
| The same, interpreter | 3.15 s | 3.32 s | same | same |
| A grant, an identity or an account through the core Agent, interpreter, 1.33 G gas | 2.3 s (one run) | — | Apple silicon, aarch64 VM | `notes/MEASUREMENTS-F005.md` §2 (the smoke run) |
| A reservation decided on the native replica | 1.3 ms | 2.0 ms | Apple silicon, aarch64 VM | `notes/MEASUREMENTS-F005.md` §2 |
| A reservation's reply through ingress and the in-process core, debug build, idle | about 1 ms | — | CI | `features/F-009-ingress-process/NOTES.md` |

On the Agent path the program reload dominates every call (about 1.31 G of 1.49 G gas,
`notes/MEASUREMENTS-F005.md` §3), so an account command costs about what a reservation costs: roughly
300 ms on the recompiler. That is inferred from the interpreter's figures, not measured on the recompiler.
The in-process core decides in milliseconds. 1 s at p99 leaves about three times the Agent path's p99
for ingress, the received-log's commit and queueing.

The owner gave 1 s at p99 as an example. It is recorded here as a proposal, and `NFR-16` carries the
mark until he confirms or changes the number.

### R4 — What the ruling did not name (`OQ-63`, open)

Four commands move value and are answered with their decision today: `capture`, `release`, `issue` and
`redeem`. The ruling names transfers, reservations and withdrawals as the commands acknowledged at
`Received` ("already implemented by #103"), so this amendment leaves the four where they are: answered
with their decision, within `NFR-16`. The ruling's first sentence ("commands that move value are
acknowledged at `Received`") would put them in the other class. That conflict is `OQ-63`. It does not
block V0: until it is ruled, they keep their decision reply.

The rest are answered with their decision, as they are today: `submit_outbound` on an existing
reservation; the settlement adapter's inputs (`CTR-SET`, whose submitter already gets the outcome
aledgent derived, `REQ-SI-5`); and `issue_disclosure`. `anchor_checkpoint` and `compare_settlement` are
operator commands, so R2 covers them.

## The rewording, exact

### `SPEC.md` front matter

`spec_version: 0.4.1` → `spec_version: 0.4.2`; `date: 2026-09-29` → `date: 2026-10-01`.

### `REQ-CS-2` (revised)

> **`REQ-CS-2`** A command MUST return synchronously, with its **operation reference** and the
> operation's state at that moment. For a command acknowledged at `Received` (`REQ-CS-8`), that state is
> `Received`, and the reply is an **acknowledgement**, not an outcome. For a command answered with its
> decision, the reply is that decision: the state the command reached, or its refusal named
> (`REQ-CS-4`). A reply MUST NOT wait on the settlement layer.

It said: "A command MUST return synchronously with the **operation reference** and the operation's state
at that moment. That state MAY be `Received`. The reply is an **acknowledgement**, not an outcome. It
MUST NOT wait on the settlement layer."

### `REQ-CS-8` (new)

> **`REQ-CS-8`** Every command is answered in exactly one of two ways:
>
> - **Acknowledged at `Received`.** The commands that move value between ledger accounts, or out of
>   them: `transfer`, `reserve`, and a withdrawal, that is a reservation that carries its outbound leg
>   (`REQ-SO-1`). The reply is `Received`, with the operation reference, within `NFR-1`. The decision
>   follows by query and notification (`REQ-CS-3`).
> - **Answered with its decision.** Every other command (`OQ-63`). The reply carries the decision
>   within `NFR-16`: the command's result, or its refusal named. It MUST NOT be `Received`.
>
> The account and security commands MUST always be answered with their decision. They are the commands
> that open identities; open or close ledger accounts; register or remove credentials; grant or revoke
> authority, issuance authority and operator authority included; initiate, cancel or complete a
> recovery; name a successor or claim a succession; change custody; and every command of the operator
> surface (`CTR-OPS`). Only an amendment may move a command from one way to the other.

### `REQ-CS-3` (revised)

The first sentence and the second-to-last change; the rest is as amendment 0007 left it.

> **`REQ-CS-3`** A command that reserves MUST be acknowledged like every command that moves value: its
> reply is `Received`, with its operation reference (`REQ-CS-2`, `REQ-CS-8`, `NFR-1`). Its decision,
> `Reserved` or a refusal, MUST reach the caller asynchronously, through the channel every outcome uses:
> `operation(reference)` and the requester's notification (`REQ-CS-6`, `REQ-EV-1`). The reservation
> reduces `available` in the same atomic step that decides it (`REQ-BA-4`). The decision is part of the
> command's completion (`NFR-15`); it has no budget of its own. Any command refused after its
> acknowledgement MUST be recorded as `Received → Rejected` with the refusal named in its notification.
> A repeat of the same request under the same client key MUST return that refusal.

It said "acknowledged like any other command" and "Any command refused after its acknowledgement,
whatever its kind, …".

### `REQ-CS-6` (revised)

One sentence is added at the end:

> … There is no blocking "wait until terminal" reply. A command answered with its decision
> (`REQ-CS-8`) waits for that decision and for nothing else: never for the settlement layer or any other
> external event.

### `REQ-CS-7` (revised)

The last sentence names the class and forbids the fallback to `Received`:

> **`REQ-CS-7`** A `Received` command MUST survive the unavailability of the deciding component. Once that
> component returns, the command MUST be decided within a stated bound (`REQ-AR-7`). That bound is
> `Received`'s deadline (§5.6). A command answered with its decision (`REQ-CS-8`) that cannot be decided
> in time MAY be answered with `ERR-DecisionUnavailable`, and MUST NOT be answered `Received` instead;
> the command is kept, and a retry with the same client key returns its decision.

### `AC-C1.1` (revised)

> - `AC-C1.1` Given an authorisation, When I reserve, Then the reply acknowledges the request at
>   `Received` with its operation reference, as for every command that moves value (`REQ-CS-8`,
>   `NFR-1`). The decision, `Reserved` or `Rejected` with the refusal named, follows by query and by
>   notification, not after any external event. Once `Reserved`, available has fallen by the amount, in
>   the same step that decided it (`REQ-BA-4`, `REQ-CS-3`, `NFR-15`).

It cited `NFR-1` alone for the acknowledgement.

### `US-M5` and `AC-M5.1` (new)

> **`US-M5`** — As an **integrating party**, when I change an identity, an account, a credential, an
> authority or custody, the reply tells me whether the change was made. I do not wait for a
> notification to know it.
> - `AC-M5.1` Given an account or security command (`REQ-CS-8`), When I send it, Then the reply carries
>   its decision: the result, or the refusal named. It is never `Received`. If the decision cannot be
>   made in time, the reply is `ERR-DecisionUnavailable`, and a retry with the same client key returns
>   the decision (`REQ-CS-7`, `NFR-16`).

### `ERR-DecisionUnavailable` (revised)

> | `ERR-DecisionUnavailable` | The command cannot be decided now. Only a command answered with its
> decision gets it (`REQ-CS-8`). The command is kept; retrying with the same client key is safe and
> returns its decision (`REQ-CS-7`) |

### §11 preamble (revised)

> The numeric targets in `NFR-1`–`NFR-3`, `NFR-15` and `NFR-16` bind from the first release in which they
> can be measured on the deployed host. Until then they are design targets. A plan MUST NOT relax them
> silently.
> Latency has three numbers and no fourth. A command that moves value is acknowledged within `NFR-1`.
> Every other command is answered with its decision within `NFR-16` (`REQ-CS-8`). Every command
> completes within `NFR-15`. Deciding a reservation is part of its completion; it has no budget of its
> own.

It said "Latency has two numbers and no third".

### `NFR-1` (revised)

> | `NFR-1` | aledgent MUST acknowledge a command that moves value (`REQ-CS-8`: a transfer, a reservation, a
> withdrawal) within 50 ms at p99, measured inside aledgent: take it in durably and return its operation
> reference and the state `Received` (`REQ-CS-2`). Its decision is part of completion (`REQ-CS-3`,
> `NFR-15`). A command answered with its decision is bound by `NFR-16`, not by this requirement. |

It said "aledgent MUST acknowledge a command within 50 ms at p99 … A command that reserves is
acknowledged the same way; its decision is part of completion".

### `NFR-4` (revised)

> | `NFR-4` | No operation that aledgent has acknowledged, or answered with its decision, may be lost. … |

It said "No acknowledged operation may be lost". Since `REQ-CS-2` now calls only the `Received` reply an
acknowledgement, the old words would have stopped covering an account opened with a decided reply.
`AC-K5.1` says the same: "No acknowledged or decided operation is missing".

### `NFR-15` (revised)

"measured inside aledgent from its acknowledgement" becomes "measured inside aledgent from its
acknowledgement, or from its arrival for a command answered with its decision (`REQ-CS-8`)". Nothing else
changes.

### `NFR-16` (new)

> | `NFR-16` | aledgent MUST answer a command that is answered with its decision (`REQ-CS-8`) within
> **1 s at p99**, measured inside aledgent from the command's arrival to the reply that carries its
> decision. A reply of `ERR-DecisionUnavailable` (`REQ-CS-7`) is measured the same way and counts
> against this bound. *The number is proposed and waits for the owner's confirmation (amendment 0008,
> R3).* |

### `OQ-63` (new, open)

> | `OQ-63` | `capture`, `release`, `issue` and `redeem` move value, but are answered with their decision
> (`REQ-CS-8`): amendment 0008's ruling named only transfers, reservations and withdrawals as
> acknowledged at `Received`. Should these four be acknowledged at `Received` too? Until this is ruled
> they are answered with their decision. It does not block V0 | `REQ-CS-8`, `NFR-1`, `NFR-16` |

### §15

Epic M's row: stories `US-M2`–`US-M5`; requirements `REQ-CS-1`–`REQ-CS-8`. The release-scope row for
Epic M: `US-M2`–`US-M5` in V0.

## What this costs, stated plainly

- **A decided reply waits behind the queue.** Ingress sends the core one input at a time: reservations
  first, then everything else in arrival order. An account command therefore waits for every transfer
  queued ahead of it. On the Agent path, at about 300 ms a decision, three queued inputs use up the
  proposed 1 s. `NFR-16` holds there only if decided commands get priority in the forwarder as
  reservations do, or if the core keeps up with arrivals. This is carried to the plan; nothing in this
  amendment changes the forwarder.
- **The interpreter cannot meet it.** At 3.15 s a decision, the Agent path on the interpreter breaches
  `NFR-16` on every call. A deployment on the Agent core needs the recompiler, or the in-process core.
- **An outage shows up as refusals.** While the deciding component is down, an account command gets
  `ERR-DecisionUnavailable` after the wait, where a value-moving command still gets `202`. The caller
  must retry with the same client key. The command is kept either way.
- **`NFR-16` is not yet a gate.** Its number waits for the owner. Ingress measures it from this
  amendment on (`F-009`); `F-018` gets no alert on it until the number is confirmed.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-CS-8` (two ways to answer a command); `NFR-16` (the decided reply, 1 s at p99 proposed); `US-M5`, `AC-M5.1` |
| Raised, open | `OQ-63` (capture, release, issue and redeem: which way they are answered) |
| Revised | `REQ-CS-2`, `REQ-CS-3`, `REQ-CS-6`, `REQ-CS-7`; `AC-C1.1`, `AC-K5.1`; `ERR-DecisionUnavailable`; `NFR-1`, `NFR-4`, `NFR-15`; §11 preamble; §15 |

## Carried to the plan

- `DEC-5`: every command that does not move value is answered with its decision; account and security
  commands always are (`REQ-CS-8`); the bound is `NFR-16`.
- `F-009`: the synchronous path is the decided commands', not the reservation's (the plan row still said
  "synchronous reservation path"). Ingress measures the decided reply (`NFR-16`) apart from the
  acknowledgement (`NFR-1`), which now samples only commands that move value. Open: priority for decided
  commands in the forwarder, and `decision_wait`'s default (today `NFR-15`'s 30 s) set against `NFR-16`
  once its number is confirmed.
- `F-018`: an alert on `NFR-16` once its number is confirmed.
- `M-2`, `M-3`: `NFR-16` measured, then met. `RISK-2`: the Agent path's decision time is again on a
  caller's reply, for account and security commands.
- The plan review's open point is closed: the commands still answered with their decision now have a
  requirement (`REQ-CS-8`) and a bound (`NFR-16`).
