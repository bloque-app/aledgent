<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0013 — answers with the decision per principal, open reservations, and enrolment

**Status:** ruled by the owner on 2026-10-03 (three questions a client's integration plan raised);
applied to `SPEC.md` v0.4.7
**Target:** `SPEC.md` v0.4.6 as on `main` (amendments 0008 to 0012); this one takes it to **v0.4.7**
**Date:** 2026-10-03
**Rationale in one line:** an integrating party that must answer its own caller at once can have its
principal's transfers and reservations answered with their decision; a caller can learn, on demand and
on a schedule, exactly what it holds reserved; and an identity in delegated custody can start with no
credential, then gain its first one through an enrolment the operator opens and the holder completes.

Nothing here resolves a question beyond what was ruled. Where a ruling leaves a gap, an open question
says so.

---

## R1 — A principal's transfers and reservations, answered with their decision

**The problem.** `REQ-CS-8` acknowledges `transfer` and `reserve` at `Received`. Their decision follows
by notification and query, bound only by `NFR-15`'s 30 s. Some integrating parties sit in front of a
caller that waits for an answer of their own, within a fixed time. A payment network that authorises a
spend is one. For such a party, an acknowledgement is not an answer: it must answer yes or no now, and
learn later is too late.

**The ruling.** A setting per principal, controlled by its integrating party, makes that principal's
`transfer` and `reserve` commands answered with their decision instead of acknowledged at `Received`. It
defaults to acknowledged. The wait is bounded by `NFR-16` (1 s) and never longer than the deployment's
wait for any command answered with its decision.

**What is not changed.** A withdrawal (a reservation that carries its outbound leg) stays acknowledged:
the ruling named transfers and reservations. The decision is the same decision. Only when the reply
carries it changes. A command answered with its decision that cannot be decided in time is kept, as
`REQ-CS-7` already requires, so a late decision is still made, notified and queryable. Nothing is lost
and nothing is decided twice.

### `REQ-CS-8` (revised)

Before:

> - **Acknowledged at `Received`.** The commands that move value between ledger accounts, or out of them:
>   `transfer`, `reserve`, and a withdrawal, that is a reservation that carries its outbound leg
>   (`REQ-SO-1`). The reply is `Received`, with the operation reference, within `NFR-1`. The decision
>   follows by query and notification (`REQ-CS-3`).

After:

> - **Acknowledged at `Received`.** The commands that move value between ledger accounts, or out of them:
>   `transfer`, `reserve`, and a withdrawal, that is a reservation that carries its outbound leg
>   (`REQ-SO-1`). The reply is `Received`, with the operation reference, within `NFR-1`. The decision
>   follows by query and notification (`REQ-CS-3`). The one exception is `REQ-CS-9`.

A paragraph is added after "Only an amendment may move a command from one way to the other.":

> The answering setting of `REQ-CS-9` does not move a command from one way to the other. It chooses,
> per principal, which of the two ways that principal's `transfer` and `reserve` are answered.

### `REQ-CS-9` (new)

> **`REQ-CS-9`** **Answered with the decision, per principal.** A principal's `transfer` and `reserve`
> commands MAY be answered with their decision instead of acknowledged at `Received`. This is the
> principal's **answering setting**.
>
> - **Who sets it.** The principal itself, or another principal of the integrating party it is tied to
>   (`REQ-ID-7`). A principal tied to no party sets its own. Setting it is an account and security
>   command: it is answered with its decision, attributed and audited (`REQ-CS-8`, `REQ-AU-7`).
> - **Default.** Acknowledged at `Received`.
> - **What it covers.** `transfer` and `reserve`, and nothing else. A withdrawal stays acknowledged.
> - **The wait.** The setting states how long the reply may wait for the decision. That wait MUST NOT
>   exceed the bound `REQ-CS-7` sets for every command answered with its decision. Unless the setting
>   states a shorter one, it is `NFR-16`'s.
> - **Past the wait.** The reply is `ERR-DecisionUnavailable`. The command is kept and decided like any
>   other (`REQ-CS-7`). Its decision reaches the principal by notification and by query (`REQ-EV-1`,
>   `REQ-CS-6`), and a retry under the same client key returns it.
> - **A command keeps the way it was taken in.** A change of the setting applies to commands taken in
>   after it. A command taken in before is answered, and re-answered on a retry, as it was taken in.
> - **No other effect.** The setting changes when the reply carries the decision, not the decision. The
>   operation, its states, its postings, its deadline and its notifications are those of `REQ-CS-3` and
>   §5.6, whichever way it was answered.
>
> *Trade-off, stated plainly.* A principal answered with its decision holds a request open while the
> decision is made. A decision that misses the wait is still made, after the principal's caller was
> told it was not. An integrating party that sets this MUST be ready to act on a late decision: to
> release a reservation it already told its caller was declined, for example.

### `REQ-CS-3` (revised)

The first sentence becomes:

> **`REQ-CS-3`** A command that reserves MUST be acknowledged like every command that moves value,
> unless its principal's answering setting says otherwise (`REQ-CS-9`): its reply is `Received`, with
> its operation reference (`REQ-CS-2`, `REQ-CS-8`, `NFR-1`).

The rest of `REQ-CS-3` is unchanged.

### `NFR-1` and `NFR-16` (one sentence each)

`NFR-1`, appended:

> A transfer or reservation of a principal answered with its decision (`REQ-CS-9`) is bound by
> `NFR-16`, not by this requirement.

`NFR-16`, appended:

> This includes a transfer or reservation of a principal answered with its decision (`REQ-CS-9`).

### `AC-C1.1` (revised)

> - `AC-C1.1` Given an authorisation from a principal acknowledged at `Received` (the default,
>   `REQ-CS-9`), When I reserve, Then the reply acknowledges the request at `Received` with its
>   operation reference, as for every command that moves value (`REQ-CS-8`, `NFR-1`). …

The rest of `AC-C1.1` is unchanged.

### §6 — `US-M6` (new)

> **`US-M6`** — As an **integrating party** that must answer its own caller at once, I have my
> principal's transfers and reservations answered with their decision.
> - `AC-M6.1` Given my principal's answering setting says "decided", When it reserves or transfers, Then
>   the reply carries the decision: `Reserved` or the transfer's terminal state, or the refusal named.
>   It is never `Received` (`REQ-CS-9`, `NFR-16`).
> - `AC-M6.2` Given the decision is not made within the setting's wait, Then the reply is
>   `ERR-DecisionUnavailable`. The command is decided later, its decision is notified, and a retry with
>   the same client key returns it (`REQ-CS-9`, `REQ-CS-7`, `REQ-EV-1`).
> - `AC-M6.3` Given no setting, Then my principal's transfers and reservations are acknowledged at
>   `Received`. Given the setting changes, Then a command taken in before the change is answered, and
>   re-answered, as it was taken in (`REQ-CS-9`).
> - `AC-M6.4` Given a setting whose wait exceeds the bound of `REQ-CS-7`, When I set it, Then it is
>   refused with `ERR-NotAuthorised` and the setting is unchanged (`REQ-CS-9`).

### §8.1 — one row

> | `set_answering` | The principal itself, or another principal of its integrating party. A wait within the bound of `REQ-CS-7` | The principal's `transfer` and `reserve` taken in from now on are answered as the setting says (`REQ-CS-9`). Otherwise `ERR-NotAuthorised` |

### §15

Epic M: `US-M2`–`US-M6`; requirements `REQ-EV-1`–`REQ-EV-8`, `REQ-CS-1`–`REQ-CS-9`. Release scope: V0
gains `US-M6`.

---

## R2 — Open reservations, on demand and on a schedule

**The problem.** A caller that reserves many times a day against many accounts, a card issuer for
example, must at intervals know exactly what it still holds reserved: per holder, how much it should
capture, release or move. Today it can read each operation by reference, and each account's `reserved`
balance. The first is one call per reservation. The second mixes its reservations with everyone else's
on the same account. Neither is one consistent view.

**The ruling.** Reservations at authorisation, captured at clearing, released on reversal, with a
deadline as long as the clearing window (30 days): that is the integrating party's use of the existing
commands, and needs no change here. What is added is a way to know, on demand or periodically (every
6 hours, for example), exactly how much to capture, release or transfer per holder: a query of open
reservations, aggregated per account and asset, and a report of the same pushed on a schedule.

### `REQ-QR-1` (one item added)

> - open reservations, by account and by the principal that made them, aggregated per (account, asset)
>   and itemised (`REQ-QR-7`).

### `REQ-QR-7` (new)

> **`REQ-QR-7`** **Open reservations.** A caller MUST be able to read the reservations that are open now,
> in one consistent view (`REQ-QR-2`):
>
> - **Selected by** account, by identity, or by the principal that made them; optionally by asset; and,
>   for the principal that made them, optionally only those whose deadline falls before a stated time.
> - **Aggregated** per (account, asset): how many are open, the sum reserved, and the earliest and
>   latest deadline.
> - **Itemised**, paged by cursor (`REQ-QR-5`): each open reservation's operation reference, account,
>   asset, amount, deadline, the principal that made it, and its client key where that principal is
>   the caller.
> - **The view's position.** The answer names the last entry its view includes, so that a later
>   movement, notification or report can be placed before or after it.
> - **Who sees what.** A principal sees the reservations it made. A holder of authority over an account
>   sees every reservation against it (`REQ-QR-4`). Nothing else.
> - **Bounded.** A request MUST state its subject. An unbounded one is refused with
>   `ERR-QueryUnbounded` (`REQ-CS-5`).
>
> "Open" means `Reserved`, `Submitted`, `Observed`, `Orphaned` or `Expired`: the value is held. An
> operation in `Needs-Review` with its value still held is open, and the answer says it is under review.

### `REQ-EV-8` (new)

> **`REQ-EV-8`** **Reservation reports.** A recipient (`REQ-EV-4`) MAY register a cadence at which it
> receives a **reservation report**: the aggregate of `REQ-QR-7` for the reservations its principal made,
> at one consistent view.
>
> - The cadence is the recipient's to choose, between one hour and one day. It registers, changes and
>   removes it through the command surface, under its own authority, like its destination.
> - A report carries: the period it closes; the view's position (`REQ-QR-7`); the aggregate per
>   (account, asset); and the latest final anchor, as every notification does (`REQ-EV-1`).
> - A report is delivered and authenticated like every notification (`REQ-EV-2`, `REQ-EV-4`,
>   `REQ-EV-6`). Its identity derives from (recipient, period), never from a time of attempt
>   (`REQ-EV-3`).
> - A report is a summary, not an outcome. A caller still learns each outcome from its operation's
>   notifications (`REQ-EV-7`). A report never replaces one, and a missed report is recovered by the
>   query of `REQ-QR-7`.

### `REQ-EV-3` (revised)

> **`REQ-EV-3`** Every notification MUST carry a stable identity: derived from (operation, transition)
> for a notification of a transition, and from (recipient, period) for a reservation report
> (`REQ-EV-8`). This makes a redelivery recognisable. A notification identity MUST NOT contain a
> timestamp or anything else that varies between attempts.

### §8.2 — one row

> | `open_reservations(subject, [asset], [before], cursor)` | Open reservations of an account, an identity, or the calling principal: aggregated per (account, asset) and itemised, from one consistent view that names its position. Only what the caller may see (`REQ-QR-7`, `REQ-QR-4`) |

### §8.1 — one row

> | `set_report_cadence` | The recipient's own authority; a cadence between one hour and one day, or none | Reservation reports delivered at that cadence from the next period, or no longer (`REQ-EV-8`) |

### §6 — `US-C6` (new)

> **`US-C6`** — As a **caller that reserves**, I know at any moment, and on a schedule, exactly what I
> hold reserved, per holder and asset.
> - `AC-C6.1` Given open reservations I made on several accounts and assets, When I read my open
>   reservations, Then I get, per (account, asset), their count, their sum and their earliest and
>   latest deadline, and each one itemised with its reference, amount and deadline, all from one view
>   that names its position (`REQ-QR-7`, `REQ-QR-2`).
> - `AC-C6.2` Given a reservation captured, released or expired and released, Then it is not in the
>   answer of any view that includes that closing (`REQ-QR-7`).
> - `AC-C6.3` Given reservations another principal made on an account I hold no authority over, Then
>   they are not in my answer, and their amounts are not in my aggregate (`REQ-QR-7`, `REQ-QR-4`).
> - `AC-C6.4` Given I registered a report cadence, When a period closes, Then I receive one report for
>   it, delivered at least once under one identity, carrying the same aggregate a query at its view
>   would return, and the latest final anchor (`REQ-EV-8`, `REQ-EV-3`).

### §15

Epic C: `US-C1`–`US-C6`; requirements add `REQ-QR-7`, `REQ-EV-8`. Release scope: V0 gains `US-C6`.

---

## R3 — Identities without a credential, and enrolment

**The problem.** A deployment that brings holders across from a prior system may know who they are and
which accounts they own, but hold no credential of theirs. The same is true of a new holder the
integrating party signs up before the holder has a device to enrol. `open_identity` requires a first
credential, so the operator must invent one, and then hold it, for every such holder. And once such an
identity exists, `REQ-AU-20` lets a credential join its set only through an existing member, a recovery
or an import: a holder with no credential has no way to add their own.

**The ruling.**

- An identity whose accounts are all in delegated custody MAY be opened with an empty credential set,
  but only by a principal holding operator authority. Any other principal that opens an identity gives
  it a first credential, as before.
- An identity with an empty set gains its first credential through **enrolment**: a path the
  integrating party drives from its own client, so its holders can add a credential later.

### `REQ-ID-8` (new)

> **`REQ-ID-8`** **An identity without a credential.** An identity MAY be opened with an empty credential
> set, only by a principal holding the operator's authority to open identities (`REQ-ID-6`, `REQ-OS-1`).
> Any other principal that opens an identity MUST give it a first credential.
>
> While an identity's set is empty:
>
> - every account it owns MUST be in delegated custody. Opening an owned-custody account for it, or
>   changing one of its accounts to owned custody, MUST be refused with `ERR-NotAuthorised`
>   (`REQ-AU-6`, `REQ-AU-13`);
> - no one exercises owner authority over its accounts. They are operated under operator authority and
>   under the authorities granted over them (`REQ-AU-5`, §5.9);
> - its first credential joins the set only by enrolment (`REQ-AU-22`), by import (`REQ-MG-7`), or by a
>   completed recovery.
>
> An identity opened with a credential, or whose set has ever held one, never returns to an empty set
> (`REQ-AU-20`).

### `REQ-AU-20` (revised)

The sentence on adding and removing becomes:

> Adding or removing a credential MUST require an existing credential of the set, or a completed
> recovery (`REQ-AU-15`, `REQ-AU-16`). A credential MAY also be added by an import (`REQ-MG-7`), and the
> first credential of an empty set by enrolment (`REQ-AU-22`). An identity's set is empty only from an
> opening under `REQ-ID-8` until its first credential joins.

The rest of `REQ-AU-20` is unchanged.

### `REQ-AU-22` (new)

> **`REQ-AU-22`** **Enrolment.** An identity whose credential set is empty (`REQ-ID-8`) gains its first
> credential through an enrolment, in two steps.
>
> 1. **The opener authorises it.** A principal holding the operator's authority to enrol opens an
>    enrolment for the identity, with a justification (`REQ-OS-1`). It states which kinds of credential
>    the enrolment accepts, among those the deployment accepts, and the accounts the credential will be
>    scoped to, if not all the identity's accounts. aledgent returns an **enrolment secret**, shown once
>    and never again (`REQ-AU-8`). The integrating party hands it to the holder through its own client,
>    after establishing who the holder is: that is the integrating party's authority, not aledgent's
>    (§4.2).
> 2. **The holder completes it.** From their own device, the holder presents the enrolment secret and a
>    proof of possession of the new credential, over a challenge aledgent issued for this enrolment.
>    This step authenticates by what it carries. The holder needs no other credential.
>
> On completion the credential joins the set, marked as enrolled, with the scope the enrolment stated.
> The completion is attributed to the opener's authority and to the new credential (`REQ-AU-7`),
> audited (`REQ-AT-1`), notified to the opener (`REQ-EV-1`) and visible to every authority holder of
> the identity's accounts (`INV-20`, `REQ-OS-4`).
>
> **Limits.**
>
> - An enrolment is single-use. It expires at a time the deployment states, no later than one hour after
>   it opens. An expired, used or cancelled enrolment completes nothing.
> - An identity has at most one open enrolment. Opening another cancels the first.
> - Each enrolment allows a bounded number of failed completions, stated by the deployment. Past it, the
>   enrolment is cancelled.
> - Opening or completing an enrolment MUST be refused with `ERR-NotAuthorised` once the identity's set
>   holds any credential. From then on, credentials are added as `REQ-AU-20` says.
> - A secret-shaped credential that aledgent itself draws is not enrolled: enrolment carries only
>   credentials whose holder proves possession of a key aledgent never sees.
> - The opener MAY cancel an open enrolment.
> - An enrolled credential proves nothing about custody by being enrolled. Before it takes an account
>   into owned custody, it MUST be proven to work (`REQ-AU-13`).
>
> A refusal names `ERR-NotAuthorised` (no authority, a set not empty, an identity not active),
> `ERR-CredentialInvalid` (a secret or proof that does not verify, a credential of a kind the enrolment
> does not accept, an expired, used or cancelled enrolment) or `ERR-JustificationRequired`.
>
> *Trade-off, stated plainly.* The first credential carries the trust of the enrolment's opener, as an
> imported one carries the import's (`REQ-MG-8`). aledgent proves that whoever completed the enrolment
> holds the credential. It cannot prove that this person is the holder the integrating party meant: the
> integrating party is the authority for that, and the enrolment's record names who opened it.

### `REQ-OS-2` (one item added)

> - open and cancel an enrolment (`REQ-AU-22`);

### §8.1 — rows

`open_identity`, revised:

> | `open_identity` | Kind stated. For an agent, the identity it acts for. A first credential, unless the opener holds the operator's authority to open identities, which MAY open it with an empty set (`REQ-ID-8`) | Identity exists (`REQ-AG-1` for agents). Its set holds the first credential, or is empty. Otherwise `ERR-NotAuthorised` or `ERR-CredentialInvalid` |

`open_account` gains, in its pre-conditions: "Owned custody only for an identity whose credential set
is not empty (`REQ-ID-8`)".

Added:

> | `open_enrolment` / `cancel_enrolment` | Operator authority to enrol; a justification; the identity active, with an empty credential set; accepted credential kinds among the deployment's | An enrolment open until it expires, with its secret returned once; or cancelled. Audit record (`REQ-AU-22`). Otherwise `ERR-NotAuthorised` or `ERR-JustificationRequired` |
> | `complete_enrolment` | An open enrolment's secret; a proof of possession of a credential of an accepted kind, over the enrolment's challenge | The credential a member of the set, marked enrolled, with the enrolment's scope. The enrolment closed. Attributed, audited, notified and shown to every authority holder (`REQ-AU-22`). Otherwise `ERR-CredentialInvalid` or `ERR-NotAuthorised` |

Operation kinds: **credential change** is `register_credential` / `remove_credential`,
`import_credentials`, and `open_enrolment` / `cancel_enrolment` / `complete_enrolment`.

### §6 — `US-A9` (new)

> **`US-A9`** — As an **identity holder** brought in without a credential, I enrol my first credential
> from my own device, through the integrating party's client.
> - `AC-A9.1` Given a principal with the operator's authority to open identities, When it opens an
>   identity with no credential, Then the identity exists with an empty set. Given any other principal,
>   Then the opening without a credential is refused with `ERR-NotAuthorised` (`REQ-ID-8`).
> - `AC-A9.2` Given an identity with an empty set, When an owned-custody account is opened for it, or one
>   of its accounts is changed to owned custody, Then it is refused with `ERR-NotAuthorised`
>   (`REQ-ID-8`).
> - `AC-A9.3` Given an open enrolment, When I present its secret and a valid proof of possession over its
>   challenge, Then my credential is a member of the set, marked enrolled, with the stated scope. The
>   enrolment is closed, and the completion is attributed, audited, notified to the opener and shown to
>   every authority holder (`REQ-AU-22`, `REQ-AT-1`, `INV-20`).
> - `AC-A9.4` Given an enrolment that is expired, used or cancelled, or a wrong secret or proof, When I
>   complete it, Then it is refused with `ERR-CredentialInvalid` and the set is unchanged. Given failed
>   completions past the deployment's limit, Then the enrolment is cancelled (`REQ-AU-22`).
> - `AC-A9.5` Given an identity whose set holds a credential, When an enrolment is opened or completed
>   for it, Then it is refused with `ERR-NotAuthorised` (`REQ-AU-22`, `REQ-AU-20`).
> - `AC-A9.6` Given an enrolled credential, When it reclaims custody, Then the reclaim needs its proof
>   that it works, as for any other credential (`REQ-AU-13`, `REQ-AU-22`).

### §15

Epic A: `US-A1`, `US-A2`, `US-A4`–`US-A9`; requirements `REQ-AU-11`–`REQ-AU-22` (as listed, with
`REQ-AU-22` added) and `REQ-ID-8` within `REQ-ID-*`. Release scope: V0 gains `US-A9`.

---

## No invariant is affected

| Invariant | Why it holds |
|---|---|
| `INV-11` | The answering setting changes when a decision is replied, not who decides it or from what. |
| `INV-16`, `INV-20` | The open-reservations query shows a caller only what it made or holds authority over. Enrolment is shown to every authority holder. |
| `INV-22` | Enrolment adds a credential. It changes no existing credential's kind or scope. |
| `INV-17` | An enrolment secret is a credential, never a ledger fact or an identifier. |

## What this costs, stated plainly

- **One more way to answer.** `REQ-CS-8`'s two ways stay two. A principal may choose which of them its
  transfers and reservations get. The forwarder, the operation and its notifications are unchanged.
- **Identities whose accounts nobody owns yet.** Until enrolment, an identity's accounts are operated by
  the operator and by the authorities granted over them. That is delegated custody, as `REQ-AU-6`
  defines it, with no owner credential behind it.
- **A new no-bearer path.** Completing an enrolment authenticates by what it carries, as a passkey
  sign-in does. It is bounded by its secret, its expiry, its single use and its failure limit.

## Carried to the plan

- **`F-009`** builds `set_answering` in ingress (`T-009-18`, in this PR), then lets another principal of
  the same integrating party set it, once party registration (amendment 0011 R4, `T-009-16`) is on
  `main` (`T-009-19`). The reservation reports of `REQ-EV-8` are delivered by ingress's worker
  (`T-009-20`).
- **`F-004`** answers `open_reservations` from the open-reservation index, with deadlines and the
  reserving principal (`T-004-09`).
- **`F-028`** builds `REQ-ID-8` and enrolment: an empty set at `open_identity` under operator authority,
  `open_enrolment` / `cancel_enrolment` on the operator surface, and `complete_enrolment` as a no-bearer
  route with an enrolment challenge (`T-028-10` to `T-028-12`).
- **Integrators.** Card authorisations reserve, capture at clearing and release on reversal, with a
  deadline of the clearing window (30 days). That uses the commands as they are.
