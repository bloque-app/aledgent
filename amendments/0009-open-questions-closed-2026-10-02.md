<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0009 — eight open questions closed

**Status:** ruled by the owner on 2026-10-02; applied to `SPEC.md` v0.4.3
**Target:** `SPEC.md` v0.4.2 as on `main` (amendment 0008); this one takes it to v0.4.3
**Date:** 2026-10-02
**Issue:** part of #25
**Rationale in one line:** the owner ruled on eight open questions (`OQ-6`, `OQ-11`, `OQ-12`, `OQ-21`,
`OQ-35`, `OQ-51`, `OQ-52`, `OQ-58`); this amendment records each ruling, the exact rewording it causes,
and the four questions the rulings raised (`OQ-64` to `OQ-67`). The owner answered three of those the same
day (`OQ-64`, `OQ-66`, `OQ-67`), and the points put to the owner on applying: "confirmed" means `Final` (R7),
the disclosure trigger (R8) and the derived reading of R3. `OQ-65` stays open, to be discussed. R9 lists
the answers.

Nothing here resolves a question beyond what was ruled. Where a ruling leaves a gap, an open question
says so, and the requirement cites it.

---

## R1 — `OQ-21`: who opens identities and accounts

**The ruling.** The operator holds the authority that opens identities and accounts. The integrating
party also stores its own share of the identity and account identifiers: it knows those identifiers on
its side. The integrating party owns the credentials of its own agents, and may delegate them for specific
tasks, such as purchases.

**Applied.** The opening authority is a new requirement in §5.1; the integrating party's agents are a new
requirement in §7.G; §3 and §4.2 say the same in their tables.

New:

> **`REQ-ID-6`** Identities MUST be opened only by the **identities principal**: the operator, or the
> principal that plays that role in a deployment. A ledger account is opened by the identities principal,
> for any identity, or by the identity that will own it: an identity MAY open further accounts of its own,
> under its own authority (`US-A2`). The authority to open identities is split by kind, attributed and
> audited, like every operator authority (`REQ-OS-1`, `REQ-AU-7`, `REQ-AT-1`). An integrating party keeps its own record of the
> identifiers of the identities and ledger accounts it serves, and maps them to its customer facts (§1.3).
> aledgent does not hold or need that record, and nothing in it decides whether an identity or an account
> exists (`REQ-ID-7`).

New:

> **`REQ-AG-4`** An integrating party holds the credentials of its own agents. It MAY delegate authority to
> one of them for a specific task, a purchase for example, under the bounds of `REQ-AG-2`. The agent still
> acts under its own credential, never under the integrating party's (`REQ-AG-1`).

§3, the operator's row, column "Authority":

Before:

> | Operator authority over delegated-custody accounts, bounded by kind (§5.9). The operator surface (§7.D) |

After:

> | Operator authority over delegated-custody accounts, bounded by kind (§5.9). The identities principal's role, unless another principal plays it: it alone opens identities, and it may open accounts (`REQ-ID-6`). The deployment's own configuration, which is its alone (`REQ-ID-7`). The operator surface (§7.D) |

§4.2, two rows changed and one added, with a paragraph after the table (the paragraph is R2's):

Before:

> | Existence of an identity and its accounts | aledgent | Everything else attaches to it (`OQ-58`) |

After:

> | Existence of an identity and its accounts: the authoritative registry | aledgent | Everything else attaches to it (`REQ-ID-7`) |
> | Opening identities and accounts | aledgent. An identity only under the identities principal's authority; an account under that principal's, or under the authority of the identity that will own it | The operator is the identities principal unless another principal plays that role (`REQ-ID-6`) |

Before:

> | How an account is presented, who a holder is as a legal person, whether they may transact, and prices and fees | The integrating party | Customer facts (§1.3) |

After:

> | How an account is presented, who a holder is as a legal person, whether they may transact, and prices and fees | The integrating party | Customer facts (§1.3) |
> | Its own record of the identifiers of the identities and accounts it serves | The integrating party | It maps them to its customer facts. It has no standing on existence (`REQ-ID-6`) |

**The gap, and its answer.** The ruling said who holds the opening authority, not whether it can be
delegated. `US-A2` has a holder open a second account of their own ("When I open a second account"), and
the plan built `open_identity` under operator authority that the operator may grant (`DEP-8`). That was
raised as `OQ-64`, and the owner answered it the same day: identities are opened only by the identities
principal (the operator, or whoever plays that role); accounts are opened by the identity itself, its own
further accounts, and by the identities principal. `REQ-ID-6` above says so, a glossary entry names the
role, and `US-A2` is reworded:

Before:

> **`US-A2`** — As an **identity holder**, I hold several accounts under one identity. I move value between
> them instantly.
> - `AC-A2.1` Given one identity, When I open a second account, Then both are distinct and separately
>   addressable (`REQ-LA-1`).

After:

> **`US-A2`** — As an **identity holder**, I hold several accounts under one identity. I open the further
> ones myself, under my own authority, and I move value between them instantly.
> - `AC-A2.1` Given one identity, When I open a second account under my own authority, Then both are
>   distinct and separately addressable (`REQ-LA-1`, `REQ-ID-6`).

New glossary entry:

> | **Identities principal** | The principal that opens identities (`REQ-ID-6`): the operator, or the principal that plays that role in a deployment. It may also open ledger accounts, for any identity. |

`OQ-21` in §14:

Before:

> | `OQ-21` | Where does the authority that opens identities and accounts live? Is a single such authority acceptable at launch? | `REQ-AU-5`, `NFR-8`, `OQ-58` |

After:

> | `OQ-21` | *Closed by amendment 0009:* the identities principal opens identities, and is the operator unless another principal plays that role; accounts are also opened by the identity that owns them (`REQ-ID-6`, `OQ-64`). The integrating party keeps its own record of the identifiers it serves, and holds its own agents' credentials (`REQ-AG-4`). | — |

## R2 — `OQ-58`: aledgent is its own authority for identity

**The ruling.** aledgent is its own authority for identity. In practice the operator controls
it, but aledgent is the authoritative registry for existence. The system is not permissionless: many
operations are carried out by their own users, and the deployer's and operator's operations (registering
integrating parties, identity-provider matters) are solely the operator's.

**Reconciled with §4.2.** §4.2 gave existence to aledgent and "who a holder is as a legal person" to the
integrating party. The ruling keeps both: they are two facts with two authorities, and the identity
aledgent holds is aledgent's own, not a registration of one the integrating party owns. The integrating
party's own record of the identifiers (R1) is an index into aledgent's registry, with no standing on
existence.

New (§5.1):

> **`REQ-ID-7`** aledgent is its own authority for identity: its book is the authoritative registry of which
> identities and ledger accounts exist (§4.2). The integrating party remains the authority for who a holder
> is (§4.2, §13). A deployment is not permissionless. Most operations are carried out by the principals they
> concern, under their own authority (§5.9). The operations that configure the deployment itself are the
> operator's alone, and no grant can give them to another principal: registering an integrating party, and
> deciding which identity providers the deployment accepts and on what terms. What registering an
> integrating party records, and which commands carry these operations, is `OQ-65`.

§4.2, after the table:

New:

> **Existence and who a holder is are two facts, with two authorities.** aledgent is the authority for an
> identity's existence: that it exists, its identifier, its kind and status, and the accounts it owns
> (`REQ-ID-7`). The integrating party is the authority for who that identity is as a person or an
> organisation, and whether it may transact. Each side keeps the identifiers; neither fact can be read
> from the other (`REQ-PV-3`).

New glossary entry (the spec used the term without defining it):

New:

> | **Integrating party** | An organisation that integrates aledgent to hold and move value for the identities it serves, or for itself. The operator registers it (`REQ-ID-7`). Like any organisation it holds an identity in the book, and it keeps its own record of the identifiers it serves (`REQ-ID-6`). |

**The gap.** The spec has no command that registers an integrating party, no record of what one is, and
no way to tie a principal to one, which R3 needs. "Identity-provider matters" names a class of operations
the spec does not list. New `OQ-65`.

`OQ-58` in §14:

Before:

> | `OQ-58` | Is the identity aledgent holds the integrating party's identity itself, with one registry and aledgent authoritative for existence? Or is it a ledger-side registration of an identity the integrating party owns? §4.2 gives existence to aledgent and "who a holder is" to the integrating party. The seam between the two decides who may open an identity. | `REQ-ID-1`, §4.2, `OQ-21` |

After:

> | `OQ-58` | *Closed by amendment 0009:* aledgent is its own authority for identity and the authoritative registry of existence; the integrating party is the authority for who a holder is (`REQ-ID-7`, §4.2). It is not permissionless: the deployment's own configuration is the operator's alone. What registering an integrating party records is `OQ-65`. | — |

## R3 — `OQ-6`: the client key's domain

**The ruling.** The client key's uniqueness domain is per integrating party, and the key never expires.

Before:

> **`REQ-OP-10`** Idempotency uses **two distinct keys that MUST NOT be conflated**. The first is a
> caller-facing **client key**. It is unique within (initiating principal, surface, key), permanent, and stored
> with the canonical response. The second is a service-derived **operation reference**. It is permanently unique
> across the whole deployment. Callers never supply it, and nothing parses it. A caller-supplied key MUST NOT be
> able to collide with another caller's key. A repeat of the same key with a different body MUST be refused.

After:

> **`REQ-OP-10`** Idempotency uses **two distinct keys that MUST NOT be conflated**. The first is a
> caller-facing **client key**. Its uniqueness domain is the **integrating party**: it is unique within
> (integrating party, surface, key), where the integrating party is the one the initiating principal belongs
> to (`OQ-65`). It never expires, and it is stored with the canonical response. The second is a
> service-derived **operation reference**. It is permanently unique across the whole deployment. Callers never
> supply it, and nothing parses it. A client key MUST NOT be able to collide with another integrating party's
> key. Within one integrating party, the request a key identifies includes its initiating principal: the same
> key from another of its principals is a different request. A repeat of the same key with a different
> request MUST be refused (`ERR-ClientKeyReused`); from another principal of the same integrating party it is
> refused too, and never answered with the first principal's reply. Such a collision counts as an integration
> failure of the integrating party, which keeps its principals' keys apart.

**A derived reading, confirmed by the owner.** With one domain per integrating party, two principals of
the same integrating party could send the same key. The rewording treats the initiating principal as part
of the request the key identifies, so that the second is refused (`ERR-ClientKeyReused`) and never
answered with the first principal's stored reply, which would break `REQ-PV-1`. The surface stays in the
domain, as before. The owner confirmed the reading, and added that such a collision counts as an
integration failure: the integrating party's to prevent, not aledgent's to absorb.

**The gap.** Tying a principal to its integrating party is `OQ-65`. Until that is ruled, the plan's
domain (initiating principal, surface; `DEP-10`) stays in the code.

`OQ-6` in §14:

Before:

> | `OQ-6` | What is the uniqueness domain of the client key (`REQ-OP-10`)? Is it per initiating principal, per integrating party, or per deployment? Does it ever expire? | `REQ-OP-10` |

After:

> | `OQ-6` | *Closed by amendment 0009:* the client key's uniqueness domain is the integrating party, and it never expires. The same key from two principals of one integrating party is refused, and counts as an integration failure (`REQ-OP-10`). | — |

## R4 — `OQ-12`: holder and agent credentials reach aledgent

**The ruling** (the owner's review of #110, 2026-10-02). Holder and agent credentials may reach aledgent.
The component that receives commands authenticates both service principals and holder credentials. The
owner wants it that way because direct access by any other route is hard to secure: everything enters
through that one authentication.

New:

> **`REQ-AU-21`** A credential held by an identity holder or by an agent MAY reach aledgent directly, as a
> service principal's may. Every call, whoever holds its credential, enters through the one component that
> receives commands and queries (`REQ-AR-7`) and is authenticated there (`NFR-7`, `REQ-AU-1`). There is no
> other way in.

`OQ-12` in §14:

Before:

> | `OQ-12` | May a credential held by a holder or an agent reach aledgent directly? Or must every call arrive through an integrating party's service principal? | `REQ-AG-1`, Epic G, §7 |

After:

> | `OQ-12` | *Closed by amendment 0009:* yes. Holder and agent credentials may reach aledgent directly, and everything enters through the one authenticated entry (`REQ-AU-21`). | — |

## R5 — `OQ-51`: notifications are relayed, authenticated with a shared secret

**The ruling.** Notifications are relayed to the destination the caller supplies. They are authenticated
in the manner of a webhook: a message authentication code (HMAC) over the body, with a secret shared with
each recipient. aledgent does not sign notifications with a per-recipient verification credential: in the
owner's view, signing every notification request costs more than it protects.

Before:

> **`REQ-EV-4`** A notification MUST be authenticatable by its recipient and MUST NOT be replayable. If a
> recipient has no configured verification secret, that MUST cause delivery to fail. Delivery never
> proceeds unverified.

After:

> **`REQ-EV-4`** aledgent relays each notification to the destination its recipient registered. A recipient
> registers its destination, and the secret it shares with aledgent, through the command surface (§7.A),
> under its own authority, and replaces either the same way. A notification MUST be authenticatable by its recipient and MUST NOT be replayable. It is authenticated by a
> message authentication code over its body, keyed with a secret that aledgent shares with that recipient
> alone. The body carries the notification's identity (`REQ-EV-3`), so a replay is recognised as one and
> changes nothing. aledgent does not sign notifications: it holds no signing credential for them, per
> recipient or otherwise. The shared secret is the only credential a notification involves, and
> `REQ-AU-8` binds it. If a recipient has registered no destination, or shares no secret, delivery MUST fail.
> Delivery never proceeds unauthenticated.

**What the delivery code does today, and what must change** (`services/ingress/src/delivery.rs`):

- It already authenticates every attempt with HMAC-SHA256 under the recipient's own secret, over a domain
  tag, the notification's identity, the attempt's time and the body, each part length-prefixed. That is a
  code over the body, keyed with a shared secret. Covering the identity and the attempt's time as well is
  what lets the reference verifier reject a stale capture as well as recognise a repeat. **No change.**
- A recipient without a secret already gets no delivery, and the notification stays visible with its age.
  **No change.**
- Nothing is signed with a key of aledgent's. The plan's alternative (one Ed25519 deployment key) was never
  built, and is now withdrawn. **No code change**; the module documentation and `F-009`'s plan said
  `OQ-51` was open, and now say it is ruled.
- **What must change:** the destination and the secret are registered today by the operator, per principal,
  with `aledgent-ingress recipient`. The ruling has the caller supply its destination, and the answer to
  `OQ-66` has it register the destination and the secret through the API. Building that is a command for
  recipients: recorded as `T-009-15` in `F-009`'s `tasks.md`, not built here.

**The gap, and its answer.** Whether a destination is supplied once per principal or with each command,
through which surface, and which side generates the secret and rotates it, was raised as `OQ-66`. The
owner answered it the same day: the recipient registers its destination and its secret through the API.
`REQ-EV-4` above says so; replacing them the same way is how a secret is rotated.

`OQ-51` in §14:

Before:

> | `OQ-51` | Does aledgent sign its outbound notifications and hold a verification credential per recipient? Or does it relay to a destination the caller supplies, holding none? | `REQ-EV-4` |

After:

> | `OQ-51` | *Closed by amendment 0009:* aledgent relays to the destination the caller supplies, authenticated by a code over the body under a secret shared with each recipient, and signs nothing (`REQ-EV-4`). The recipient registers both through the command surface (`OQ-66`). | — |

## R6 — `OQ-52`: every kind of principal needs a push contract

**The ruling.** Every kind of principal needs a push contract. Polling is not an acceptable primary
contract: it would put the systems under load with no policy to absorb it.

New:

> **`REQ-EV-7`** Every kind of principal MUST have a push contract. A person, an organisation, an agent
> and an integrating party's service principals all receive their notifications pushed to their destination
> (`REQ-EV-1`, `REQ-EV-4`). Polling is not an acceptable primary contract for any kind of principal: it
> would put the system under load with no policy to absorb it. A query of one operation
> (`operation(reference)`) remains available to recover or to confirm an outcome. It is never the way a
> caller is expected to learn of one.

Before:

> **`REQ-CS-6`** A caller learns that an operation completed by notification (`CTR-EVT`) or by query
> (`operation(reference)`). It never infers

After:

> **`REQ-CS-6`** A caller learns that an operation completed by notification (`CTR-EVT`), its primary
> contract (`REQ-EV-7`), or by query (`operation(reference)`), to recover or to confirm. It never infers

**A client that polls today.** A client that learns of each operation's outcome by polling
`GET /v1/operations/{reference}` must, under this ruling, move to push: register its destination and
receive the notifications. Recorded in the plan's notes, not here, because `SPEC.md` names no client.

`OQ-52` in §14:

Before:

> | `OQ-52` | Is polling acceptable for agent principals, or does every kind of principal need a push contract? | `CTR-EVT`, Epic G |

After:

> | `OQ-52` | *Closed by amendment 0009:* every kind of principal needs a push contract; polling is not an acceptable primary contract (`REQ-EV-7`). | — |

## R7 — `OQ-35`: the non-terminal states callers see

**The ruling.** Callers see at least these non-terminal states: accepted (`Received`), reserved, and
confirmed.

**Mapping "confirmed": the owner's answer.** The vocabulary keeps observation, confirmation and finality
apart (`REQ-IN-7`). A **confirmation** (§2) is an observation establishing that an external leg was
included on the settlement layer, not final and not settled; the state that records it is `Observed`. The
first draft of this amendment mapped "confirmed" there. The owner ruled otherwise: **"confirmed" means
`Final`**, finality having passed, not `Observed`. The plain name in `REQ-OP-12` is therefore not the
glossary's *confirmation*, and the requirement says so, so the two are not read as one. "Accepted" in the
ruling is the acknowledgement, the state `Received`, and not the state `Accepted`, as amendment 0007 (R1)
already said of the word.

New:

> **`REQ-OP-12`** Callers MUST see at least these three non-terminal states, by notification (`REQ-EV-1`,
> `REQ-EV-7`) and by query; `Received` is also the acknowledgement's state (`REQ-CS-2`). For each, aledgent
> states what has and has not happened, and the caller is expected to act as the table says.
>
> | State | Its plain name | What has happened, and what has not | What the caller is expected to do |
> |---|---|---|---|
> | `Received` | accepted | The command is durably held (`REQ-CS-2`). Nothing is decided, and no balance has changed | Wait for the decision. Do not act on the operation: no capture, release or submission. A retry under the same client key is safe and changes nothing |
> | `Reserved` | reserved | The value is held for this operation and is no longer available to spend (`REQ-BA-4`). Nothing has reached its destination | Proceed: capture or release it, or hand off its external leg. Do not treat it as a payment made |
> | `Final` | confirmed | The settlement layer's finality point has passed the external leg's observation, and the canonical record still matches (`REQ-IN-3`): the leg can no longer be revised. The settling posting is not yet appended, so the operation is not yet `Settled` | Treat the external leg as executed and irreversible. Do not release it, or submit again on it. Wait for `Settled`, which follows |
>
> "Accepted" here means acknowledged, not the state `Accepted` of the table above. "Confirmed" here is
> the plain name for finality having passed, the state `Final`. It is not a confirmation in §2's sense: that
> is what `Observed` records, and it can still be withdrawn (`REQ-IN-7`). Every other transition is pushed
> too (`REQ-EV-1`), and every state stays visible by query (`REQ-OP-7`); this table states what a caller is
> expected to do for these three only.

**The gap, and its answer.** The ruling named three states as a floor. Whether the other states are
pushed was raised as `OQ-67`, and the owner answered it the same day: **every state transition is
pushed**. `REQ-EV-1` is reworded to say so:

Before:

> **`REQ-EV-1`** aledgent MUST emit a notification on every terminal outcome and on every transition a
> caller cannot otherwise observe. This includes `Received → Accepted` and `Received → Rejected`. A
> notification MUST carry:

After:

> **`REQ-EV-1`** aledgent MUST emit a notification on every state transition of an operation (§5.6),
> terminal or not, `Received → Accepted` and `Received → Rejected` included: every state a caller can query
> is also pushed to it (`REQ-EV-7`, `REQ-OP-12`). A notification MUST carry:

The core already records one notification with every transition it commits, so this needs no code
change. What a caller is expected to do with each state outside `REQ-OP-12`'s three was not ruled, and
the requirement does not say.

`OQ-35` in §14:

Before:

> | `OQ-35` | Which non-terminal states can callers see, and what is a caller expected to do with each? | `REQ-OP-7` |

After:

> | `OQ-35` | *Closed by amendment 0009:* at least `Received` (accepted), `Reserved` and `Final` (confirmed), each with what the caller is expected to do (`REQ-OP-12`). Every other transition is pushed as well (`OQ-67`). | — |

## R8 — `OQ-11`: hourly anchoring, and what triggers an anchor on demand

**The ruling.** Anchoring is hourly.

**On demand.** `REQ-PR-1` already required anchoring on demand, and §8.1 already has `anchor_checkpoint`,
which `F-017` built as an operator command with a justification. The spec never said what triggers it.
This amendment states that trigger, and a second one, which the owner confirmed: a disclosure about
entries that no final anchor covers yet. A disclosure names the checkpoint it is anchored against
(`REQ-DS-2`), and only a final anchor establishes anything (`REQ-PR-8`). Without the second trigger, such
a disclosure would wait for the next hourly anchor.

Before:

> **`REQ-PR-1`** aledgent MUST anchor the state of its book on the settlement layer at a bounded interval
> and on demand. The anchored value commits to every entry recorded up to that point, and to nothing else.

After:

> **`REQ-PR-1`** aledgent MUST anchor the state of its book on the settlement layer **every hour**, and on
> demand. The anchored value commits to every entry recorded up to that point, and to nothing else. An hour
> is therefore the longest window in which a revision cannot be detected (`REQ-PR-3`). An on-demand anchor
> MUST be taken on either of two triggers: when the operator asks for one, with a justification
> (`anchor_checkpoint`, `REQ-OS-1`); and when a disclosure is requested whose assertion concerns entries
> that no final anchor covers yet. That disclosure is then issued against the new anchor, once it is final
> (`REQ-DS-2`, `REQ-PR-8`).

§4.3, "Can the operator revise history after the fact?":

Before:

> Between anchors, a revision is not detectable. |

After:

> Between anchors, at most an hour apart (`REQ-PR-1`), a revision is not detectable. |

`OQ-11` in §14:

Before:

> | `OQ-11` | What is the anchoring cadence, and what triggers an on-demand anchor? The interval is exactly the window in which a revision cannot be detected (`REQ-PR-3`). | `REQ-PR-1`, `NFR-11` |

After:

> | `OQ-11` | *Closed by amendment 0009:* anchoring is hourly, and on demand when the operator asks or when a disclosure concerns entries that no final anchor covers yet (`REQ-PR-1`). | — |

---

## R9 — the owner's answers on applying (2026-10-02)

Applying R1 to R8 raised four questions and three points for confirmation. The owner answered all but one
the same day; the text of R1 to R8 above is the answered text.

| Raised | Answer | Applied in |
|---|---|---|
| "Confirmed" mapped to `Observed` (R7) | **"Confirmed" means `Final`**, not `Observed` | `REQ-OP-12`, `REQ-EV-1`, `OQ-35` |
| The disclosure as an on-demand anchor trigger (R8) | **Confirmed**: a disclosure about entries that no final anchor covers yet takes an anchor | `REQ-PR-1`, `OQ-11` |
| The derived reading of `OQ-6` (R3) | **Confirmed**: a client-key collision across principals of one integrating party is refused, and counts as an integration failure | `REQ-OP-10`, `OQ-6` |
| `OQ-64` (R1) | **Closed**: identities are opened only by the identities principal (the operator, or whoever plays that role); accounts by the identity itself, its own further accounts, and by the identities principal | `REQ-ID-6`, `US-A2`, `AC-A2.1`, §2, §3, §4.2 |
| `OQ-66` (R5) | **Closed**: the recipient registers its notification destination and secret through the API | `REQ-EV-4`, `OQ-51` |
| `OQ-67` (R7) | **Closed**: every state transition is pushed | `REQ-EV-1`, `REQ-OP-12` |
| `OQ-65` (R2, R3) | **Open**, to be discussed | — |

## New open questions

Raised by this amendment, as they now stand in §14. They take the numbers after `OQ-63`, which amendment
0008 raised.

> | ID | Question | Blocks |
> |---|---|---|
> | `OQ-64` | *Closed by amendment 0009:* identities are opened only by the identities principal, the operator or whoever plays that role; ledger accounts by the identities principal and by the identity itself, for its own further accounts (`REQ-ID-6`, `US-A2`). | — |
> | `OQ-65` | What does registering an integrating party record (`REQ-ID-7`)? How is each principal tied to exactly one integrating party, as the client key's domain needs (`REQ-OP-10`), and what of a principal tied to none, such as the operator's own? Which identity-provider matters are the operator's alone, and which commands of §7.D carry them? | `REQ-ID-7`, `REQ-OP-10`, §7.D |
> | `OQ-66` | *Closed by amendment 0009:* the recipient registers its notification destination and the secret it shares with aledgent through the command surface, and replaces them the same way (`REQ-EV-4`). | — |
> | `OQ-67` | *Closed by amendment 0009:* every state transition is pushed (`REQ-EV-1`). | — |

As raised, before the owner's answers, they asked: `OQ-64`, may the operator delegate the opening
authority, and to whom; `OQ-66`, how a recipient supplies its destination and secret, and which side
generates and rotates the secret; `OQ-67`, whether the non-terminal states `REQ-OP-12` does not name are
pushed, and what a caller does with each.

## §15

Before:

> | G — Agents | `US-G1`–`US-G4` | `REQ-AG-1`–`REQ-AG-3`, `REQ-AU-1`–`REQ-AU-4`, `REQ-AU-7` | `INV-22` |

After:

> | G — Agents | `US-G1`–`US-G4` | `REQ-AG-1`–`REQ-AG-4`, `REQ-AU-1`–`REQ-AU-4`, `REQ-AU-7`, `REQ-AU-21`, `REQ-EV-7` | `INV-22` |

Before:

> `REQ-EV-1`–`REQ-EV-6`, `REQ-CS-1`–`REQ-CS-7` | `INV-8` |

After:

> `REQ-EV-1`–`REQ-EV-7`, `REQ-CS-1`–`REQ-CS-7` | `INV-8` |

Before:

> `REQ-OP-6`–`REQ-OP-8`, `REQ-OP-10`, `REQ-OP-11`,

After:

> `REQ-OP-6`–`REQ-OP-8`, `REQ-OP-10`–`REQ-OP-12`,

## What this costs, stated plainly

- **A closed registry.** aledgent is the authority for existence, and only the identities principal
  opens identities. An integrating party cannot onboard a customer without that principal's authority
  behind the call, unless it plays the role itself. That is the price of one authoritative registry. A
  holder opens its own further accounts.
- **The client key spans an integrating party.** Two of its systems must not reuse each other's keys
  for different requests; a collision is a refusal and an integration failure, never a merged reply. A
  key never expires, so the record of every key is kept for the life of the deployment.
- **Holder credentials at the front door.** Admitting them puts the authentication of people's and
  agents' credentials in aledgent's entry, with what that entails for abuse and rate limits. The owner
  chose it over the alternative, where every integrating party relays its users' calls.
- **A shared secret per recipient.** aledgent holds one secret per recipient, sealed at rest, and a
  recipient can forge a notification to itself, which a signature would prevent. That only harms the
  recipient. The owner chose it over signing.
- **No polling as a contract.** Every client must run an endpoint that receives notifications, including
  a client that would rather poll, and receives one for every transition. A client that polls today must change.
- **"Confirmed" waits for finality.** A caller hears "confirmed" only once the settlement layer's finality
  point has passed, which can be much later than the leg's first observation. `Observed` is still pushed
  (`REQ-EV-1`), but it is not "confirmed".
- **An hour of undetectable revision.** Hourly anchoring means a revision inside the last hour cannot be
  detected by anyone holding an anchor (`REQ-PR-3`).

## Versions

- Amendment 0008 (#114) took `SPEC.md` to v0.4.2. This amendment sets v0.4.3.
- Amendment **0010** (another agent's: a Migration epic that imports credentials into an identity's set,
  for `F-028`) is the next number.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-ID-6`, `REQ-ID-7`, `REQ-AU-21`, `REQ-AG-4`, `REQ-EV-7`, `REQ-OP-12`; glossary entries *integrating party* and *identities principal* |
| Revised | `REQ-OP-10`, `REQ-PR-1`, `REQ-EV-1`, `REQ-EV-4`, `REQ-CS-6`; `US-A2` and `AC-A2.1`; §3 (operator's authority), §4.2 (rows and a paragraph), §4.3 (one answer), §15 (G and M rows, cross-cutting) |
| Closed | `OQ-6`, `OQ-11`, `OQ-12`, `OQ-21`, `OQ-35`, `OQ-51`, `OQ-52`, `OQ-58` |
| Raised and closed | `OQ-64` (who opens identities and accounts), `OQ-66` (how a destination and its secret are registered), `OQ-67` (every transition is pushed) |
| Raised, open | `OQ-65` (registering an integrating party) |
| Unchanged | Every invariant and error, and every acceptance criterion but `AC-A2.1`, whose text is reworded: the conformance catalogue changes that text and its version |

## Carried to the plan

- `DEP-7` (`OQ-12`): resolved. The first production deployment turns the switch on: `allow_holder_credentials: true` in
  `deploy/templates/ingress.json`. `PLAN.md`'s row is left for #110, which edits the same row.
- `DEP-8` (`OQ-21`, `OQ-58`, `OQ-64`): the identities principal opens identities (the operator, or whoever
  plays that role); an identity opens its own further accounts. `open_account` already lets an owner open
  a customer account for itself.
- `DEP-10` (`OQ-6`): the domain becomes the integrating party once `OQ-65` ties principals to one;
  `T-009-16` in `F-009`'s `tasks.md`. Until then ingress dedups on (initiating principal, surface, key).
- `F-009`: notification authenticity is ruled as built; the recipient registering its destination and
  secret through the API is `T-009-15`. The core already records a notification with every transition
  (`OQ-67`). A client that polls today moves to push (`REQ-EV-7`).
- `F-017`: the interval is one hour (the default already, and the deployment's). The disclosure trigger is
  `T-017-09`.
- `F-015`: the catalogue at v0.4.3 (`T-015-12`).
