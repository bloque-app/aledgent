<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0020 — pass-through data, credit notifications, and multi-leg submissions

**Status:** ruled by the owner on 2026-10-05, on the decision form: `prq-08` ("'reference', ≤1 KiB UTF-8
JSON, echoed: reply, operation, notifications, movements"), `prq-10` ("amend spec to notify every
credited account's party") and `prq-15` ("one atomic transfer per batch; refuse batches over limit", with
the note that the caller's own batch is unlimited and is chunked). Applied to `SPEC.md` v0.4.12.
**Target:** `SPEC.md` v0.4.11 as on `main` (amendments 0001 to 0016 and 0018 applied; `0017` is #163's
draft, and `0019` is claimed by `amend-0019-compact-smt`). This one takes it to **v0.4.12**. If 0019 lands
first, this one is renumbered to the version after it.
**Date:** 2026-10-05
**Rationale in one line:** a caller's own pass-through data travels with its operation and comes back to it
everywhere it reads that operation; a party learns when value lands in an account it serves, not only when
it moved the value itself; and a multi-leg submission is a caller-facing, atomic transfer with a maximum the
deployment publishes, which closes `OQ-24`.

Nothing here resolves a question beyond what was ruled. The owner's other rulings of 2026-10-05 on an
integrating party's plan bind that plan, not this specification.

---

## R1 — `prq-08`: pass-through data

**The problem.** `REQ-PO-4` permits opaque pass-through data and forbids branching on it. It never says
whether a command may carry it, how much, or where it comes back. An integrating party that starts an
operation for an order, a graph node or a card authorisation needs its own key to come back with the
operation. Otherwise it must keep a second map from operation reference to its own records, and that map
breaks when a crash falls between the reply and the write.

**The ruling.** A field named `reference` (the wire name is the plan's), at most 1 KiB of UTF-8 JSON,
echoed in the reply, the operation, its notifications and its movements.

**Vocabulary.** The spec calls it **pass-through data**. "Reference" alone is ambiguous here: the
operation reference is aledgent's own identifier (`REQ-OP-10`). The wire field the owner named is a plan
choice (§7: wire formats do not bind).

### `REQ-PO-5` (new)

> **`REQ-PO-5`** **Pass-through data.** A command that submits an operation MAY carry pass-through data:
> one JSON value, encoded in UTF-8, of at most 1 KiB. aledgent records it with the operation and never
> branches on it (`REQ-PO-4`).
>
> - **It is part of the request.** A repeat under the same client key with different pass-through data is
>   a different request, refused with `ERR-ClientKeyReused` (`REQ-OP-10`).
> - **It comes back to its owner.** It is echoed in the command's reply, whether acknowledgement or
>   decision; in `operation(reference)`; in every notification of the operation's transitions
>   (`REQ-EV-1`); and in the operation's movements (`REQ-QR-1`). It is shown only to principals of the
>   integrating party whose principal submitted it (`REQ-ID-7`). A principal tied to no party is its own
>   party here.
> - **Nobody else sees it.** It is not in a credit notification to another party (`REQ-EV-9`), in a
>   movement read by another party, in a disclosure, or in an anchored value (`INV-17`).
> - **Over the bound, or not one JSON value,** the command is refused and nothing is taken in. It fails
>   closed, as any command whose shape is not one aledgent accepts, with `ERR-NotAuthorised`.
> - **It is part of the operation's record.** An export carries it, and an import restores it
>   (`REQ-RC-3`).
>
> The caller MUST NOT put personal data in it. aledgent cannot check this and does not try. Because it
> anchors none of it and shows it to no other party, a caller that does is exposed only to itself.

### §6 — `US-M8` (new)

> **`US-M8`** — As an **integrating party**, my own reference for an operation comes back with it
> everywhere I read it.
> - `AC-M8.1` Given a command with pass-through data, Then its reply, `operation(reference)`, every
>   notification of its transitions and its movements carry the same data (`REQ-PO-5`).
> - `AC-M8.2` Given a repeat under the same client key with different pass-through data, Then it is refused
>   with `ERR-ClientKeyReused` (`REQ-PO-5`, `REQ-OP-10`).
> - `AC-M8.3` Given pass-through data over 1 KiB, or not one JSON value, Then the command is refused with
>   `ERR-NotAuthorised` and nothing is taken in (`REQ-PO-5`).
> - `AC-M8.4` Given another party's principal that reads a movement of the operation, or receives its
>   credit notification, Then the pass-through data is not in what it reads (`REQ-PO-5`, `REQ-EV-9`).

---

## R2 — `prq-10`: every credited account's party is notified

**The problem.** `REQ-EV-1` notifies the requester. When value lands in an account because another
principal acted, the party that serves that account learns nothing. Examples: an issuance by an
issuance authority; a transfer a holder signed with their own credential (`REQ-AU-21`); a capture into a
merchant's account. That party must then poll, which `REQ-EV-7` rules out as a primary contract, or miss
the credit.

**The ruling.** Amend the spec so the party of every credited account is notified.

### `REQ-EV-9` (new)

> **`REQ-EV-9`** **Credit notifications.** When a posting credits a ledger account's `available` balance
> with value from outside the account, aledgent MUST notify the account's **serving principal**.
>
> - **Which postings.** `Transfer` (each credited leg), `Capture` (the destination), `Issue` (the
>   destination, an inbound arrival's included) and `Reversal` (each account it credits). Not `Release`,
>   which returns the account's own reservation, and not `Opening` (`REQ-RC-4`).
> - **The serving principal** of an account is the principal that opened it (`REQ-ID-6`). For an account
>   its own identity opened, it is the principal that opened that identity. The notification reaches that
>   principal's registered destination (`REQ-EV-4`), and so its integrating party (`REQ-ID-7`).
> - **What it carries.** The credited account; the asset; the amount credited; the posting kind; the
>   operation reference and the operation's state when the posting was appended; and the latest final
>   anchor, as every notification does (`REQ-EV-1`). The counterparty is shown only as far as a movement
>   of that account shows it to the serving principal (`REQ-QR-4`). Pass-through data only if the serving
>   principal's party is the requester's (`REQ-PO-5`).
> - **Its identity** derives from (operation, posting, credited account) (`REQ-EV-3`).
> - **Also to the requester.** A serving principal that is also the requester, or of the requester's
>   party, receives both the transition notification and the credit notification. Each has its own
>   identity, and neither replaces the other.
> - **Delivery** is as for every notification: at least once, unordered, authenticated, recorded
>   (`REQ-EV-2`, `REQ-EV-4`, `REQ-EV-6`). A serving principal with no destination is parked like any other
>   recipient. A credit notification is never a condition of the credit.

### `REQ-EV-3` (revised)

> **`REQ-EV-3`** Every notification MUST carry a stable identity: derived from (operation, transition) for
> a notification of a transition, from (operation, posting, credited account) for a credit notification
> (`REQ-EV-9`), and from (recipient, period) for a reservation report (`REQ-EV-8`). This makes a
> redelivery recognisable. A notification identity MUST NOT contain a timestamp or anything else that
> varies between attempts.

### §6 — `US-M7` (new)

> **`US-M7`** — As an **integrating party**, I learn when value lands in an account I serve, whoever moved
> it.
> - `AC-M7.1` Given a transfer, a capture, an issuance or a reversal that credits an account another
>   principal serves, Then that account's serving principal receives a credit notification for it. The
>   notification carries the account, asset, amount, posting kind, operation reference and latest final
>   anchor (`REQ-EV-9`).
> - `AC-M7.2` Given a release of a reservation, or an opening, Then no credit notification is sent
>   (`REQ-EV-9`).
> - `AC-M7.3` Given a credit to an account the requester also serves, Then the requester receives the
>   transition notification and the credit notification, each with its own identity, and a redelivery of
>   either carries the same identity (`REQ-EV-9`, `REQ-EV-3`).
> - `AC-M7.4` Given an account its own identity opened, Then its credit notifications reach the principal
>   that opened that identity (`REQ-EV-9`).

---

## R3 — `prq-15`: multi-leg submissions, and `OQ-24` closed

**The problem.** `OQ-24` asks whether a multi-leg submission is a caller-facing operation, and what its
contractual maximum is. `US-B3` already lets a caller submit several transfers as one unit (`AC-B3.1`) and
refuses one over "the declared maximum" (`AC-B3.3`). But the spec never says that the maximum is the
caller's to know, nor whether a caller may bring a composite external instruction (`REQ-SO-2`).

**The ruling.** One atomic transfer per batch; a batch over the limit is refused. The owner's note: the
integrating party's own batch has no limit, and it is cut into chunks. The owner's reference is an earlier system,
where a transaction that failed within a batch was redirected, on its own, to a failed-items queue and
sent back to the caller (`prq-15`). A submission here is atomic (`REQ-CS-10`), so one refused leg refuses
every leg in its chunk: "as before" holds only if the integrating party isolates the failed item. When a
chunk is refused, it submits its legs again one by one (submissions of one leg), returns the legs that
are refused to its caller through the failed-items queue, and lets the rest proceed. Cutting, retrying
and the queue are the integrating party's. What aledgent promises is the unit: one submission, atomic,
within a published maximum.

### `REQ-CS-10` (new)

> **`REQ-CS-10`** **Multi-leg submissions.** A `transfer` MAY carry several legs. It is one operation and
> one unit: every leg applies or none does (`REQ-KER-10`, `AC-B3.1`). "Atomic" covers that one
> submission, and nothing across submissions.
>
> - **The maximum is published.** The deployment declares the most legs one submission may carry, and
>   publishes it on the query surface (`limits()`, §8.2). A submission over it is refused at submission
>   with `ERR-BatchTooLarge`, and nothing is taken in (`AC-B3.3`).
> - **aledgent does not split.** A caller with more legs than the maximum divides them into submissions
>   within it. Each is a separate operation with its own client key, atomic on its own. aledgent makes no
>   promise across them: one may apply and another be refused.
> - **Internal only.** A multi-leg submission moves value between ledger accounts. An outbound leg is
>   always submitted on its own, and only aledgent composes what goes to the settlement layer
>   (`REQ-SO-2`).

### `REQ-SO-2` (revised)

> **`REQ-SO-2`** aledgent MUST assemble its own external instructions. The composition submitted to the
> settlement layer MUST be aledgent's. A caller never submits a composite external instruction: a
> multi-leg submission is internal (`REQ-CS-10`).

### `AC-B3.3` (revised) and `AC-B3.4` (new)

> - `AC-B3.3` Given the submission exceeds the published maximum, Then it is refused at submission with
>   `ERR-BatchTooLarge`, and nothing is taken in (`REQ-CS-10`).
> - `AC-B3.4` Given I read `limits()`, Then it states the most legs one submission may carry. Given a
>   submission of exactly that many legs, Then it is taken in (`REQ-CS-10`).

### §8.2 — one row

> | `limits()` | What this deployment accepts in one command: the most legs of a multi-leg submission, and the largest pass-through data (`REQ-CS-10`, `REQ-PO-5`) |

### §14 — `OQ-24` closed

> | `OQ-24` | *Closed by amendment 0020:* yes. A multi-leg submission is a caller-facing `transfer`, atomic as one unit; its maximum is declared by the deployment and published (`limits()`); a caller splits anything larger into separate submissions (`REQ-CS-10`). | — |

`OQ-25` (atomicity on the book when the settlement layer applies some legs and not others) is untouched.
Multi-leg submissions are internal, so it concerns only aledgent's own composed external instructions.

---

## §15

- Epic B: requirements add `REQ-CS-10`.
- Epic M: `US-M2`–`US-M8`; requirements `REQ-EV-1`–`REQ-EV-9`, `REQ-CS-1`–`REQ-CS-10`, `REQ-PO-5`.
- Release scope: V0 gains `US-M7` and `US-M8`.

## No invariant is affected

| Invariant | Why it holds |
|---|---|
| `INV-16`, `INV-20` | A credit notification carries only what the serving principal could read as a movement of the account it serves. Pass-through data is shown only to its owner's party. |
| `INV-17` | Pass-through data is never anchored, never in an identifier, never disclosed. |
| `INV-1`, `INV-3`, `REQ-KER-10` | A multi-leg submission is one posting, as before. Only its maximum becomes public. |

## Carried to the plan

- **`F-009`.** The pass-through data, wire field `reference`: taken in, part of the request digest, and
  echoed in the reply (`T-009-21`). Credit notifications delivered to the serving principal
  (`T-009-22`). `limits()` routed (`T-009-23`).
- **`F-007` / `F-011`.** The operation record keeps the pass-through data and the serving principal of
  every account. The outbox gains credit notifications. Movements echo the pass-through to its owner's
  party only (`T-011-*`, numbered by that feature).
- **`F-008`.** `max_legs` becomes a deployment setting, published by `limits()`.
- **Integrating parties.** A batch of any size is cut into submissions of at most `limits().max_legs`.
  Each is atomic. A refused one goes to the caller's failed-items queue, the integrating party's
  choice.
