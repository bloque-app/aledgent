<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0037 — A credential approves each command that matters, one command at a time

**Status:** **awaiting the owner's approval.** The direction was ruled on 2026-10-19 (the ruling that a passkey
approves one transaction, not a session) and on 2026-10-20 and 2026-10-21 (owner answers on the Decisions form:
the work is split in two features; this amendment and its design are public in this repository, ruling `w12` (a)).
The owner answered the first draft's ten questions on 2026-10-21 and ruled on the core options on the same day
(`w14`). An independent review of the second draft found four blocking faults, and a second review of the third draft found
two more (the exemption of configured tokens, and the plain-English questions); this fourth draft fixes them. The
text below is the drafter's reading of the rulings, worked through to rules and byte formats. It is **still not
approved**. Nothing in `SPEC.md` or `PLAN.md` changes with this file: the proposed spec text is marked as such, and
the plan sites will cite `[PENDING AMENDMENT 0037]` once the owner approves. The features that build it
(`F-040`, `F-041`) merge no code before that.
**Target:** `SPEC.md` v0.4.28 as on `develop`. Amendment 0036 (PR #50, also awaiting approval) touches `REQ-RT-20`
and `US-K7` and no clause named here. Amendment 0034 (the upgrade authority is held with a credential) is cited
where the two meet (R1, "Upgrade routes"). `PLAN.md` `DEC-19`, plus a new decision (see "Plan").
**Date:** 2026-10-21
**Number:** 0036 is taken by an open pull request (#50); this is the next free one.
**Adds:** `REQ-AU-23`, `US-A10` with `AC-A10.1` to `AC-A10.16`, one refusal in §10 (`ERR-CommandApprovalRequired`),
and three glossary entries (**Approval**, **Session**, **Bearer secret**).
**Amends** (each with its proposed text in R5): `REQ-AU-20` (one sentence), `REQ-CS-8` (one sentence), `limits()` in
§8.2, and `REQ-AR-5` (one sentence). Nothing is withdrawn. Nothing existing is renumbered.
**Serves:** `REQ-AU-1`, `REQ-AU-7`, `REQ-AU-20`, `REQ-AU-21`, `REQ-CS-4`, `REQ-OP-10`, `NFR-1`, `NFR-7`.
**Rationale in one line:** today a sign-in gives a session that serves every action for fifteen minutes, so whoever
holds a stolen session token can move the holder's value and even add their own device-bound credential; one
approval per command, made with the holder's own credential, takes that away.

---

## In plain words

Some terms, once, so the rest reads without a glossary.

- **Approval.** A proof of possession, made with one of the caller's own credentials, that covers *this one
  command*. It is not a sign-in and it is not a session. For a device-bound credential (a passkey) it is made at the
  person's device and works once. For a key it is a signature the key's holder makes over the command, which proves
  the key was used, not that a person was present.
- **Command digest.** A short fixed-length fingerprint of the command: the deployment, the client key, who asks, and
  everything the command says, amounts and destinations included. Change any of those and the fingerprint changes.
  It is what the credential signs, so the signature belongs to one command and cannot be moved to another.
- **Approval challenge.** The random-looking bytes ingress hands a device to sign. They carry the command digest, an
  expiry and a one-time number, sealed with a secret only ingress holds, so ingress can check later that it issued
  them without keeping a copy.
- **Approval record.** What Feature B would write into the book for each approved command, so the book itself shows
  a credential approved it. This amendment fixes its bytes now, so that B cannot change what A froze.
- **Session.** The short-lived proof that stands for a caller after a successful sign-in with a device. It signs nothing
  itself, and it is the thing a thief steals.
- **Bearer secret.** A secret that proves who a caller is simply by being presented, as an issued secret (an API key)
  does, or a **configured token**: a secret an operator set up in ingress's configuration for a service, a partner or
  an agent. Whoever holds it is the caller. A session is not one: it stands for a sign-in made with a device, and it
  is the thing a bearer secret is contrasted with below.
- **Payload digest.** The fingerprint of extra material a command brings, such as the registration of a new device,
  so that the approval covers that material and not only the command's words.
- **The current credential list.** The list of credentials an identity holds, as the book keeps it now.
- **Ingress.** The component that receives commands (`REQ-AU-21`). Feature A changes ingress only.

## The problem

A sign-in at ingress with a device-bound credential returns a session token that lasts fifteen minutes and serves
any number of commands (`DEC-19`). The token is held by whoever presents it: whoever holds it acts as the holder. So, within
those minutes, a stolen or leaked token can:

- move value out of the holder's accounts;
- grant authority over them;
- add the thief's own device-bound credential to the holder's credential set, which outlives the session. An earlier
  design left this as an accepted risk; the ruling of 2026-10-19 removes the acceptance.

The device proves possession when it signs in. After that, nothing asks the device again.

## The rulings this serves

1. **A passkey assertion approves one transaction, not a session** (ruling of 2026-10-19).
2. **Two features** (owner, 2026-10-20/21). **Feature A** is ingress only: it asks for the approval, checks it and
   refuses a command without one; it touches no core code and ships first. **Feature B** is the core recording the
   approval in the book; it waits for A and for a throughput measurement. Both core alternatives are measured
   before one is chosen (`w14`), with the non-functional requirements preserved.
3. **Scope.** Approval is required for every command that moves or reserves value, changes who may act, or changes a
   credential. Reads and sign-in need none.
4. **Callers.** Partner API keys, service tokens and the core's own timers and settlement are exempt, because no
   person is behind them. This draft keeps that ruling whole: exemption follows how the caller proved who it is
   (a bearer secret presented as it is, or a session or a signature), not the scheme of a credential a token happens
   to name.
5. **Sessions** stay for sign-in and reads. Every write, and adding a passkey, asks for an approval.
6. **A retry** of an approved command under the same client key is answered from the first result and needs no new
   approval.
7. **Place.** The amendment and the design are public (`w12` (a)).
8. **Schemes.** Only credentials of a scheme that *signs* (`key` and passkey, `webauthn-es256`) can approve. An
   `api-key` signs nothing, so it cannot approve and needs no approval (2026-10-21).
9. **Any of the identity's passkeys may approve**, not only the one that signed in (2026-10-21).
10. **A `key` credential's approval is a signature over the command digest** (the owner accepted the proposal,
    2026-10-21). No caller at ingress authenticates by such a signature today (see "How the door decides"), so this
    format is frozen for use by a credential in a set that approves for a session, and by a deployment that later
    accepts direct signatures.

## What this amendment does not decide

Plainly, so nobody reads a choice into silence. Each is also in "Questions for the owner (still open)".

- Which design Feature B takes (Q-A).
- Whether the approving credential's own limits count, and where that is checked (Q-C).
- How the closed list of refusals is kept true until the next core (Q-D).
- Which slowdown rule governs Feature B's measurement (Q-F).
- Which borderline commands need approval (Q-G).
- What happens to approval-needing writes while the core cannot be reached (Q-H).
- Whether a bearer secret may change who may act or a credential (Q-I).
- Whether a withdrawal's destination is approved with `submit_outbound` or moves into `withdraw` (Q-J).
- Anything about Feature B beyond its record's bytes (F3): its signature scheme and the confirmation of its
  threshold.

---

## R1 — `REQ-AU-23` (new), proposed text for `SPEC.md` §5.9, after `REQ-AU-22`

> **Proposed text. Applied nowhere.**
>
> **`REQ-AU-23`** **A credential approves each command that matters, one command at a time.** An **approval** is a
> proof of possession by a credential of the initiating principal's identity whose scheme signs (a key, or a
> device-bound credential), over one command: the deployment, the client key, the initiating principal, the
> principal it acts for where different, and everything the command carries that decides what it does. For a
> command that adds a verifier to a credential set, that includes the verifier. An approval approves that command
> and no other, once.
>
> - **Which commands.** The commands that move or reserve value, name where value goes, change who may act on an
>   account, or change a credential. The default list is the table of amendment 0037, "Which commands". A
>   deployment MAY add commands and MUST NOT remove one. It publishes the list with its limits (§8.2).
> - **Which callers.** Decided by how the caller proved who it is, and by nothing else: not the kind of principal, and
>   not the route. A caller whose proof of who it is rests on a credential that signs needs an approval: a
>   session, which stands for a sign-in made with a device-bound credential, or a signature by a key. A caller that
>   presents a bearer secret as it is (an issued secret, or a token an operator configured for a principal) needs
>   none, and nor does an input the book gives itself (a deadline, a settlement observation). Approvals themselves
>   can be made only by a credential of a scheme that signs.
> - **Who approves.** Any active credential of the initiating principal's set whose scheme signs, not only the one
>   the caller presented. The principal acted for never approves. Exception: the command that schedules a new version
>   of the code (`upgrade`) MUST be approved by the credential the upgrade-authority mode (`REQ-RT-18`) names for the
>   caller. The commands that move or cancel a scheduled upgrade take no approval (amendment 0034, ruling `w6` (B)).
> - **What the approval carries.** A device-bound credential's approval MUST carry the device's own check that the
>   person using it is its owner (a PIN, a fingerprint or a face), whatever the deployment asks of sign-in. A key's
>   approval is a signature by that key, valid for a stated, short time.
> - **A session never approves.** A session proves who is calling, for sign-in and for reads. It does not approve a
>   write.
> - **No approval.** A command that needs an approval and carries none MUST be refused with the refusal of R2, and
>   nothing is taken in: no operation reference, no row, no state.
> - **A bad approval.** An approval that does not verify, that was already used, that has expired, that was made for
>   another command, another identity or another deployment, that lacks the device's check, or that a credential
>   outside the initiating principal's set made, MUST be refused with `ERR-CredentialInvalid`, whichever of those it
>   is, with nothing taken in (`REQ-ER-3`).
> - **The current credential list.** Whether the approving credential is still a member of the set MUST be decided
>   against the list as the book holds it now, at most as old as the bound of `NFR-20`. If it cannot be read, the command is refused with `ERR-DecisionUnavailable`. A stored copy
>   of a credential never stands in for an approval.
> - **Once, everywhere.** A device-bound approval is valid for a stated, short time and is accepted once, by any
>   instance of the component that receives commands, and also after a restart.
> - **A retry.** A repeat of a command that was approved and taken in, under the same client key, returns the first
>   result and needs no new approval (`REQ-OP-10`). The same client key with a different command is
>   `ERR-ClientKeyReused`, as always.
> - **Order of checks.** A shape fault is found first (`REQ-ER-3`), then whether a first result exists for the key
>   (a retry), then whether an approval is needed and valid. A command with a shape fault never reaches the
>   question of approval.
> - **Two steps, one command.** Obtaining what a credential must sign takes nothing in. The command is taken in, and
>   `REQ-CS-2`, `REQ-CS-8` and `NFR-1` apply to it, from the request that carries its approval.
> - **Where it is enforced.** By the component that receives commands. This requirement does not say whether the
>   book also records the approval.

*Trade-offs, stated plainly.*

- **One more round trip per write.** The credential must be asked for each approval, so every command in scope costs
  two calls instead of one, and a person at a device. A client that submits many commands for a person (a batch
  import, a payroll run) can no longer do it on one session. That is the point, and it is also the cost.
- **A withdrawal is two approvals.** `withdraw` fixes the payer and the amount; `submit_outbound` fixes the
  destination. Each is a command that needs an approval, so a withdrawal costs two (Q-J asks whether that is
  right).
- **A tap proves presence, not understanding.** The device's prompt does not show the amount or the destination.
  The approval proves that someone with the device was present and that the application asked the device to sign
  *this* digest. It does not prove the person read the amount. An application that is itself compromised can show
  one amount and ask for another. What the approval stops is a stolen or leaked session, a copied token and a program
  that submits commands on its own. It does not stop malicious code in the holder's own browser. Showing the
  person what they approve is the client's duty; this amendment gives it the digest and the command to show, and no
  more.
- **A key's approval is not a person.** A program that holds a key approves its own commands. The gain is against the
  theft of something that only stands for the key.
- **A bearer secret still moves value, and can change a credential set.** An issued secret or a configured token is
  exempt, as the owner ruled, because no person is behind it. That also means a leaked one can call
  `register_credential`, `remove_credential`, `grant_authority`, `delegate_custody` and `name_successor` with no
  approval. For an identity that holds both a device-bound credential and a bearer secret, a leaked secret can add the
  attacker's device-bound credential, and that credential then approves everything. This is the threat the amendment
  is written to end, reached through a longer-lived bearer. It is the one gap this design leaves, and Q-I puts it to
  the owner.
- **A stolen session can hold back an approved upgrade.** The commands that move or cancel a scheduled upgrade take no
  approval, as amendment 0034 (ruling `w6` (B)) wants: any one member of the upgrade-authority mode may take a way out
  alone, with no second approval and no delay. So a stolen session of a member can cancel or keep postponing an
  approved upgrade (a security update, say) with no device. Amendment 0034 already records that one member can do it;
  what is new is that a thief with only a session can. Q-G asks whether to exempt them still.
- **The device's own check excludes some authenticators.** An authenticator with no PIN or biometric cannot approve
  any write, `remove_credential` included. A person whose only such device is lost recovers through
  `REQ-AU-15`.
- **Exempt callers are exempt.** A configured token, an issued secret and the adapter's inputs are not approved. Trust
  in them is the integrating party's and the operator's, as today. The identities principal can still add a credential
  to the set of an identity it manages (`REQ-AU-20`, clause R2 of amendment 0025), because it is a service.
- **Until Feature B, the book trusts ingress.** The core is not told about the approval. A compromised ingress
  could log a command with no approval, just as it could log any command. The first feature makes a *stolen
  session* useless; it does not make a *compromised ingress* harmless. The second feature narrows that, and does not
  close it either.
- **An unreachable core refuses approved writes.** The approving credential is checked against the current credential
  list, so while no core can be read these writes are refused with `ERR-DecisionUnavailable` instead of acknowledged on a
  stored copy, as `F-038` does for other commands. This is a loss of availability the amendment names plainly; Q-H
  asks whether to accept it.
- **A synced passkey has no sign count.** The counter that detects a cloned authenticator is zero for passkeys that
  sync between devices (as at sign-in today). For those, the one-time number and the short expiry are the only
  protection against a replayed approval.

## R2 — §10: the refusal

The owner named the new refusal `ERR-ApprovalRequired` and then ruled (2026-10-21) that it is called
**`ERR-CommandApprovalRequired`**, because `ERR-ApprovalRequired` is already in §10 with another meaning, a second
principal approving a reversal. That one is not touched.

> **Proposed text. Applied nowhere.**
>
> Add to the table of §10, after `ERR-ApprovalRequired`:
>
> | `ERR-CommandApprovalRequired` | A command that needs a credential's approval of that one command (`REQ-AU-23`) was submitted without one. A presented approval that does not verify is `ERR-CredentialInvalid` |

**The status code.** Proposal: the same status as `ERR-NotAuthorised`, set in the refusal table of the OpenAPI
document, which `REQ-CS-4` tests. The owner may prefer another.

**When the core learns it (ruled 2026-10-21).** `Refusal` is an enum in `ledger-core`, compiled into the core program,
and a test checks it against §10 in both directions (`REQ-ER-1`), so §10 and the enum cannot differ. The owner ruled
that the core's side of the new line ships with **the next core**, after the throughput measurement of `F-041`, and not
with the next re-genesis. Feature A ships first, so for a time ingress would answer a refusal the core's list does not
yet have. The drafter's reading is that the line enters §10 when this amendment is applied; until the next core
carries it, ingress holds the refusal in its own type and the two-way test is told about this one named exception.
Plainly, that exception **weakens `REQ-ER-1`** for a window of unknown length: the test is the thing that keeps the
closed list closed. The alternative (Q-D) is to keep the refusal unreachable: the rollout switch is never put to
"enforce" anywhere until the next core carries the line, and the exception only covers report-only and off. Nothing
else about the core changes in Feature A, and Feature A never sends the core this refusal.

## R3 — `US-A10` and its acceptance criteria (new), proposed text for `SPEC.md` Epic A

> **Proposed text. Applied nowhere.**
>
> **`US-A10`** — As an **identity holder**, nobody moves my value, my authority or my credentials with only a stolen
> session: each command that matters is approved with one of my own credentials, one command at a time.
> - `AC-A10.1` Given a command that needs an approval and none attached, Then it is refused with
>   `ERR-CommandApprovalRequired`, nothing is taken in, and no operation reference exists for it (`REQ-AU-23`).
> - `AC-A10.2` Given a valid approval of that command, Then it is taken in and decided as any command is
>   (`REQ-AU-23`, `REQ-CS-8`).
> - `AC-A10.3` Given an approval of one command, When it accompanies a command that differs in amount, destination,
>   client key, initiator or acted-for principal, Then it is refused with `ERR-CredentialInvalid` and nothing is taken
>   in (`REQ-AU-23`).
> - `AC-A10.4` Given an approval already used, or expired, or made by a credential of another identity, or made over
>   a challenge issued for another identity or another deployment, Then it is refused with `ERR-CredentialInvalid`,
>   on any instance and after a restart (`REQ-AU-23`, `REQ-ER-3`).
> - `AC-A10.5` Given a command approved and taken in, When it is repeated under the same client key, Then the first
>   result is returned and no approval is asked for; and When the same key carries a different command, Then it is
>   `ERR-ClientKeyReused` (`REQ-AU-23`, `REQ-OP-10`).
> - `AC-A10.6` Given a command that needs an approval, When it comes with a bearer secret presented as it is (an
>   issued secret, or a token an operator configured for a principal), Then it is taken in without one
>   (`REQ-AU-23`).
> - `AC-A10.7` Given a session, When it signs in or reads, Then no approval is asked for (`REQ-AU-23`).
> - `AC-A10.8` Given a session and no approval, When a device-bound credential is registered, Then the command is
>   refused with `ERR-CommandApprovalRequired`; and Given the approval of an existing credential in the set, and the
>   registration ceremony of `REQ-AU-20`, Then the credential joins the set (`REQ-AU-23`).
> - `AC-A10.9` Given two independent implementations of the command digest, When both are given one command, Then
>   they produce the same digest, byte for byte (`REQ-AU-23`; the vector of F5).
> - `AC-A10.10` Given a command that needs an approval and has a shape fault, Then it is `ERR-MalformedCommand`, not
>   a refusal for the missing approval (`REQ-AU-23`, `REQ-ER-3`).
> - `AC-A10.11` Given an identity with two device-bound credentials and a session made with the first, When the
>   second approves a command, Then it is accepted, and the audit names the second as the approver and the first as
>   the caller (`REQ-AU-23`, `REQ-AU-7`); and When an approval lacks the device's own check, Then it is refused with
>   `ERR-CredentialInvalid`, also in a deployment that does not ask for that check at sign-in.
> - `AC-A10.12` Given the approval of a registration of one device-bound credential, When the command arrives
>   carrying the registration of another, Then it is refused with `ERR-CredentialInvalid` and the set is unchanged
>   (`REQ-AU-23`).
> - `AC-A10.13` Given a credential that was removed from its set, When it approves a command, Then it is refused with
>   `ERR-CredentialInvalid`; and When the current list cannot be read, Then the command is refused with
>   `ERR-DecisionUnavailable` and a stored copy of the credential is not used (`REQ-AU-23`, `NFR-20`).
> - `AC-A10.14` Given a command that needs an approval, When it comes under a session, by a principal of any kind, Then
>   it needs an approval; and Given a key credential in the initiating principal's set, When its key signs the digest
>   within its validity, Then that is an approval and the command is taken in (`REQ-AU-23`).
> - `AC-A10.15` Given an approval made for one deployment, When it is presented to another that shares the
>   authentication key, Then it is refused with `ERR-CredentialInvalid` (`REQ-AU-23`).
> - `AC-A10.16` Given the command that schedules an upgrade, When a credential other than the one the mode names
>   approves it, Then it is refused with `ERR-CredentialInvalid`; and Given a command that moves or cancels a
>   scheduled upgrade, Then it takes no approval (`REQ-AU-23`, `REQ-RT-18`, amendment 0034).

## R4 — `REQ-CS-8`, one sentence

Add at the end of the paragraph that begins "The account and security commands MUST always be answered with their
decision":

> **Proposed text. Applied nowhere.**
>
> A command that needs a credential's approval (`REQ-AU-23`) is answered, or acknowledged, only once its approval has
> been verified; before that it is refused and nothing is taken in.

## R5 — The other clauses this amends

> **Proposed text. Applied nowhere.**
>
> **§2, glossary, two entries.** *Approval:* a proof of possession by a credential, over one command, that the command
> may be taken in (`REQ-AU-23`). *Session:* a short-lived proof that a sign-in succeeded; it stands for the caller until
> it expires and signs nothing. *Bearer secret:* a secret that proves who a caller is by being presented as it is; a session is not one, because it stands for a sign-in made with a credential that signs.
>
> **`REQ-AU-20`, one sentence at the end of the first paragraph:** Adding or removing a credential is a command that
> needs an approval (`REQ-AU-23`); adding a device-bound credential needs it from a credential already in the set, in
> addition to the ceremony above.
>
> **§8.2, `limits()`, add to the description:** and which commands need a credential's approval, whether the deployment
> enforces that, and how long an approval is valid (`REQ-AU-23`).
>
> **`REQ-AR-5`, one sentence:** A deployment MUST also declare whether it enforces approvals (`REQ-AU-23`): enforced,
> reporting only, or off.

---

## Which commands: the table behind R1

The default list is a floor (R1: a deployment may add, not remove). It was read against every command route of
ingress (`services/ingress/src/surface.rs`) and the core inputs a route can cause. "Needs approval" means: when the
command is submitted under a session or by a signature (the callers rule of R1); a command submitted with a bearer
secret never needs one. The reasons use the three tests of the ruling: it moves or
reserves value (or names where value goes), it changes who may act, or it changes a credential.

| Command | Approval? | Why |
|---|---|---|
| `transfer`, `reserve`, `withdraw`, `capture`, `release`, `issue`, `redeem` | Yes | Move or reserve value. `withdraw` fixes the payer and the amount, not the destination |
| `submit_outbound` | Yes | Names the destination of a reserved outbound leg. The route is on the command surface and is decided under the payer's authority, so a stolen session can call it; the digest covers the destination (Q-J) |
| `propose_reversal`, `approve_reversal`, `resolve_operation`, `resume` | Yes | Each can move value: a reversal posts, a resolution settles or releases, a resume restarts an operation |
| `grant_authority`, `revoke_authority`, `grant_issuance`, `revoke_issuance`, `grant_operator`, `revoke_operator` | Yes | Change who may act |
| `delegate_custody`, `name_successor`, `claim_succession`, `cancel_recovery`, `initiate_recovery`, `complete_recovery` | Yes | Change who may act on an account |
| `register_credential`, `remove_credential`, `import_credentials` | Yes | Change a credential. The digest covers the verifier being added (F1, field 9) |
| `register_alias` | Yes | Names the account a payee's alias resolves to, so it decides where value sent to the alias goes |
| `bind_asset` | Yes | Fixes where the asset's value leaves and enters, on the settlement layer |
| `configure_settlement` | Yes | Names the adapter whose facts the book accepts: changes who may act |
| `register_integrating_party`, `tie_principal` | Yes | Decide whose client keys a principal shares and who sees its pass-through data. The drafter's reading of "who may act"; Q-G |
| `upgrade` | Yes, by the credential the mode names | Replaces the code the book runs: the widest change of who may act. See "Upgrade commands" |
| `reschedule_upgrade`, `cancel_upgrade` | No | A way out. Amendment 0034 (ruling `w6` (B)) lets any one member take it alone, with no second approval and no delay; amendment 0035 requires every way out to be decided. It can stop a change of code, never make one. Q-G |
| `open_identity`, `open_account`, `close_account`, `set_answering` | No | Ruled 2026-10-21. `open_identity` carries its first verifier in its body, so the command text covers it if a deployment adds it |
| `reclaim_custody` | No | Ruled 2026-10-21. It carries its own proof of possession over the account and its epoch. That proof's device check follows the deployment's sign-in setting, not "always" (Q-G) |
| `freeze_account`, `unfreeze_account`, `register_asset`, `deprecate_asset`, `declare_adapter_support`, `submit_evidence` | No | None of the three tests: they move no value, change nobody's authority and no credential |
| `issue_disclosure`, `anchor_checkpoint`, `compare_settlement` | No | None of the three tests. `issue_disclosure` produces a signed statement about the book for a third party, so it can reveal confidential facts; the three tests do not cover confidentiality, and the drafter left it exempt for that reason. Q-G |
| The adapter's inputs: `declare_adapter`, `ack_instruction`, `record_signing`, `record_observation`, `record_check`, `record_head`, `record_gap`, `record_rescan`, `evidence_checked`, `attest_asset`, `ack_anchor`, `anchor_not_executed`, `record_positions` | No | Facts the settlement adapter reports; only the principal that `configure_settlement` names may submit them (`INV-11`). They move no value on a person's say |
| Ticks and delivery records | No | The book's own inputs |
| Reads, `/v1/auth/challenge`, `/v1/auth/passkey` | No | No write |

**There is no ingress route for `complete_enrolment`** (`F-028` `T-028-11` and `T-028-12` are not built), so it is
not in the table. **Some core inputs have no ingress route.** Four are authority inputs (`SetAuthority`, `ProposeUpgrade`,
`ApproveUpgrade`, `ProposeMigration`): they are decided in the core under the mode in force and are out of Feature A;
whether Feature B approves them is for its amendment. Others only the host or the book's own start-up makes (for
example loading the opening balances, importing a book, the chainspec's chunks): no person submits them, and they
need no approval.

**Pairs, re-audited.** Where one command fixes the amount and another the recipient, both are in the table:
`withdraw` and `submit_outbound`; `reserve` and `capture` (`capture` names the destination); `grant_authority` and
the commands that use the grant; `register_alias` and a `transfer` to the alias. No pair has one side exempt.

**Upgrade commands.** Under amendment 0034 a mode names each principal with one credential, and "a principal holds
the authority only when it acts with that credential... nor [does] any other credential of the set". Ruling 9 (any
credential of the set may approve) would let another credential approve an `upgrade` attributed to the named one,
which contradicts that. So `upgrade` MUST be approved by the credential the mode names, which is the one the caller
presented. This is the only place ruling 9 does not apply. R1's sentence names `upgrade` alone. The commands that move
or cancel a scheduled upgrade stay exempt, because amendment 0034 (ruling `w6` (B)) says any one member may take a way
out alone, with no second approval; amendment 0035 only requires that every way out be decided, and is untouched. The
consequence for a stolen member session is in the trade-offs of R1 and in Q-G.

**Scope of the approving credential.** A credential may be limited to some of an identity's accounts
(`REQ-AU-20`). Ingress today sees a credential's identity, kind, scheme and verifier, and not its limits. Of the
commands in the table, these name the accounts they touch in the command itself: `transfer`, `reserve`, `withdraw`,
`issue`, `redeem`, `close`, `grant_authority`, `delegate_custody`, `name_successor`, `claim_succession`,
`register_alias`, `capture` (the destination). These do not, and name only an identifier whose accounts are in the
book: `release` and `capture` (the reservation), `revoke_authority` (the grant), `cancel_recovery` and
`complete_recovery` (the pending recovery), `remove_credential` (the credential), `resolve_operation`, `resume`,
`propose_reversal` and `approve_reversal` (the operation). Checking the approving credential's scope is therefore
(a) a check at ingress for the first group, if the lookup returns the credential's limits (a change to what ingress
reads from the core), and (b) a read of the book per approval for the second group, on the `NFR-1` path, or a check
in the core (Feature B). Q-C asks whether the limits count at all; this paragraph is the cost of each way.

A new kind of command must be put on one side of this table on purpose, the way `intake()` already forces every
command into one of two answers. A command nobody classified must not default to exempt (task `T-040-04`).

---

## How the door decides who needs an approval

`REQ-AU-23` keys on how the caller proved who it is. Ingress knows four ways, and the decision needs no read of any
credential's scheme:

| What the request presented | Approval needed? |
|---|---|
| A session (it stands for a sign-in with a device-bound credential) | Yes, whatever the kind of the principal, the route or the time since sign-in. A session of a principal configured as a service needs one too; a person at the operator console who signed in with a device is such a session |
| A signature by a key credential over the command (F4) | Yes, and it is the approval itself. Ingress accepts no such request today; the row says what a deployment that adds one would owe |
| An issued secret (an API key), presented as it is | No (ruling 4 and ruling 8) |
| A configured token, presented as it is | No (ruling 4), including a token whose principal's only credential is a `key` |

Exemption is **by how the caller proved who it is**, never by the kind of principal (`kind` = service, holder or
agent), never by route, and never by the scheme of a credential a token happens to name. Agents authenticate with a
bearer secret and are exempt like any holder of one.

**The gap this leaves, plainly.** A principal whose only credential is a `key` and who reaches ingress with a
configured token cannot approve and is not asked to. A person's account reachable only by a bearer secret has no
approval at all. This is what ruling 4 says; Q-I asks whether it holds for the commands that change who may act or a
credential.

**Approvals and the credential list.** The approving credential's membership is read from the book, through the same
cache ingress already keeps for issued secrets (`T-031-13`), only if an entry is never older than the bound of
`NFR-20` (1 second); a stored copy of a credential never stands in for an approval (R1).

## The formats (carried to the plan; not for `SPEC.md`)

These are what the plan will bind (`PLAN.md`, a new decision) and what the features build. They name the technology
the spec leaves out, as the "Carried to the code" section of amendment 0027 does.

Four formats, each versioned, and a vector (F5). A and B share F1 and F2. Feature A freezes F1 and F2 when it ships,
and B must not change them. F3 is fixed here so that B stores what this amendment promised. F4 is a key's approval.
Where one is open, the text says so.

**Conventions for all.** A *field* is written as four bytes (big-endian) giving its length, then its bytes. This is
the repository's existing framing for request digests (`Canon` in `ledger-core`). Numbers are big-endian. Text is
UTF-8. The version is one byte, `0x01`, and is a field of its own, so a later version changes the digest instead of
reading as an old one. A field may be empty (length zero).

### F1 — The command digest, version 1

The digest names the command as the caller meant it, built only from things the caller supplies or the deployment
holds. **It does not hold the operation reference.** Ingress mints that reference at the moment it logs the command
(`service::mint`, sixteen random bytes), `REQ-OP-10` says callers never supply it, and ingress cannot reproduce it
when the approval arrives. The client key already names the request uniquely within the initiator's key domain, and
the command text already holds every reservation, grant or credential a command points at. So no change to
`REQ-OP-10` is needed.

It is the SHA-256 of these fields, in this order, framed as above:

| # | Field | Bytes |
|---|---|---|
| 1 | Tag | The ASCII text `aledgent/approval/command` (25 bytes). Separates this digest from every other the repository hashes |
| 2 | Version | `0x01` |
| 3 | Deployment | The book's identity, 16 bytes: the `deployment` of the genesis configuration. It separates two books with different genesis files. **It does not separate a restore or a clone of the same book**, which carry the same value |
| 4 | Environment | The text of a new ingress setting, `environment`: a name the operator gives each running environment (for example `prod-eu`, `test`, a restore drill). It is configuration, not book state, so a restore does not bring it. Nothing in ingress's configuration does this today, so this amendment proposes adding it; ingress refuses to start with approvals on and no `environment`. **A copied or duplicated name is not detected:** two environments given the same name are not separated. All ingress instances of one environment must hold identical values, and the roster check of `F-038` is where to compare them. The core never sees `environment`, so Feature B must carry it (G4) |
| 5 | Client key | The key as ingress holds it: the bytes of the string the caller sent, or, when the caller sent none, the 64 hexadecimal characters ingress derives and returns (`REQ-OP-11`). The derivation reads the request only, so step 1 and step 5 derive the same key. Two deliberately identical payments sent with no key are therefore one request, as today, and the second is answered with the first's result |
| 6 | Initiator | The text of the initiating principal, `did:aledgent:id:…` |
| 7 | Acted-for flag | `0x00` if the command acts for nobody else; `0x01` if it acts for another identity |
| 8 | Acted-for | **Present only if field 7 is `0x01`:** the text of the identity it acts for. Without it the digest has seven fields before the payload digest; with it, eight |
| 9 | Payload digest | Empty, or 32 bytes: for a command whose caller supplies material from which ingress derives a verifier (a registration of a device-bound credential), the SHA-256 of the framed fields: the tag `aledgent/approval/payload`, `0x01`, the registration's client data bytes, then its attestation bytes, decoded from base64url. Empty for every other command, because every other verifier is in the command text |
| 10 | Command | The command's *canonical text*, below |

Counting the fields as framed: a command that acts for nobody else has nine, one that acts for another has ten.

**What the payload digest is for.** For a registration of a device-bound credential, the request ingress turns into a
command holds an empty verifier, and the real one comes out of the attestation at submit. Without the payload
digest the approval would not say which public key joins the set, and an attacker who can alter the submit request
could swap in an attestation from their own device (a fault the independent review found). With it, the approval of device *A*
cannot add device *B* (`AC-A10.12`). An issued secret's verifier is the hash of a secret ingress draws after the
approval; there is nothing of the caller's to bind, and the approval covers the target identity, the scope and the
marker. The registration challenge of amendment 0027 must still be valid when the command is submitted, so a client
asks for the approval at once after the device creates the credential.

**Canonical text.** The command body (`CommandBody`) after ingress has normalised it, written as JSON with these
rules, so that an implementation in any language writes the same bytes:

- no whitespace outside strings;
- object members in ascending order of their names, compared as UTF-8 bytes (every name is ASCII);
- strings with only these escapes: `"` as `\"`, `\` as `\\`, and the control characters U+0008, U+0009, U+000A,
  U+000C and U+000D as `\b \t \n \f \r`. **Every other character outside U+0020 to U+007E** (the other control
  characters, **U+007F included**, and everything above) as a lower-case `\uxxxx` escape, with a surrogate pair for a
  character above U+FFFF;
- numbers as non-negative integers in plain decimal, with no sign, no leading zeros, no fraction and no exponent. An
  integer above 2^53 − 1 is written as a decimal *string* instead, so that no implementation rounds it. No command
  body holds a non-integer (`INV-7`); one that did would not be approvable;
- amounts, identifiers and enumerated values as ingress normalises them already: amounts are decimal strings without
  leading zeros (`"010"` and `"10"` are one request), identifiers are `did:` forms in upper-case canonical base32;
- a member that is absent stays absent. `null` never appears.

This is close to RFC 8785 (JSON canonicalisation) for the values a command holds, and not the same: RFC 8785 writes
a non-ASCII character as raw UTF-8, and this writes it as an escape, so that no implementation can disagree about
byte order or normalisation. Against today's `Command::canonical()` the members come sorted, where today they come
in the order the Rust types declare them. The text is built from the parsed command, not from the caller's bytes, so
duplicate keys and alternative number forms cannot reach it at ingress. In Feature B they matter: if the core is
handed the text to hash, it MUST parse the bytes it hashed, write them again by these rules and refuse any
difference, or a buggy or compromised ingress could send text that differs from the command the core decides.

**How ingress recomputes it at step 5.** From the genesis configuration (field 3) and the `environment` setting (field 4), the request's client key
or the one derived from the request (field 5), the authenticated initiator (field 6), the acted-for identity in the
request (7, 8), the registration payload in the request (9) and the command parsed and written again (10). It uses
nothing the client states about the digest, and nothing minted.

**Left out on purpose.**

- *The two times* the core input carries, `received_at` and `now`: they do not exist when the approval is made.
- *The credential* in `caller.credential`: who approved is the approval record's field (F3), and the digest says what
  was approved.
- *The operation reference*, above.
- *Pass-through data* (`REQ-PO-5`): the core never receives it (`REQ-PO-4`), so Feature B could not check it. A
  command's pass-through data is therefore not approved. When the caller sends no client key, the key ingress derives
  hashes the request including its pass-through data, so in that case the data is indirectly covered; for a caller
  that sends its own key it is not, and a repeat of the same key with another note is `ERR-ClientKeyReused`.
- *Defaults.* A deadline the command does not carry is not filled in; the core applies the deployment's default at
  decision. The digest covers the command as sent.

**What the deployment and environment fields protect against.** Two environments that share an authentication key
(a mistake) would otherwise accept each other's challenges. With these two fields an approval made for `test` is
refused by `prod-eu`, and one made for a book with another genesis is refused too. The cleanest protection is still a
stated deployment rule: **never share the authentication key between environments**. A restore drill of the same
book, started with its own `environment` name, is separated from production by that name.

**The hash (ruled 2026-10-21): SHA-256.** WebAuthn itself hashes the client data with SHA-256, and browsers and
hardware support it natively, so the same function serves the digest, the challenge and the device. The cost is on
Feature B: the core's own program links only BLAKE2b-256, so B adds SHA-256 to the core (a small dependency).
Feature A freezes this choice when it ships.

### F2 — The approval challenge, version 1 (a device-bound credential)

What the device signs: a *challenge* of 121 bytes, in the same family as the registration challenge of amendment
0027, with its own purpose and its own seal.

| Bytes | Field |
|---|---|
| 0 to 15 | A one-time number, 16 random bytes from the operating system |
| 16 to 23 | The expiry, in milliseconds since the Unix epoch, big-endian: ingress's clock (`Time`) at issue plus the deployment's challenge lifetime (120 000 milliseconds by default) |
| 24 | The purpose: `0x03`, approve. (`0x01` is sign-in, `0x02` registration, as today) |
| 25 to 56 | The identity digest: SHA-256 of the text of the initiating principal, as `target_digest` already does (amendment 0027). It names the identity, not a credential, so any credential of the identity may approve |
| 57 to 88 | The command digest of F1 |
| 89 to 120 | The seal: HMAC-SHA-256 under the deployment's authentication key, over the domain `aledgent/approval-challenge/v1` (the text, not framed), then each of these seven fields, each preceded by its length as eight bytes, big-endian: the number, the expiry, the purpose, the identity digest, the command digest, **the book's identity and the environment name** (they are sealed but not carried, because the digest holds them too). This is the existing `AuthnKey::mac` framing |

How it differs from what exists, so nothing is confusable: the length (121; a sign-in challenge is 57 and a bound
registration 89), the purpose byte, and the seal's domain. `check_challenge` refuses a 121-byte challenge today
because no length matches; the checker for approvals refuses any other length in the same way.

The browser puts the challenge, in base64url, in `clientDataJSON` as it does today. An approval is an ordinary
assertion over it, with two differences from sign-in: **the device's own check is required** (ingress builds the
approval's policy with it set, whatever `require_user_verification` says for sign-in), and the credential may be any
active device-bound credential of the initiating identity. Same origin check, same relying-party check, same
signature check, same sign count in the same durable store as sign-in, so a cloned authenticator is caught across
both.

**Spent numbers are kept with a margin.** A spent number is kept until its expiry plus a margin that covers the clock
difference between instances, and the clock used is the database's, as for the feed lease (`F-038`). Without the
margin an instance whose clock runs ahead could forget a number another instance still accepts. The cleanup runs in
every approved write's `NFR-1` budget, so it is measured (`T-040-07`).

### F3 — The approval record, version 1 (what Feature B stores)

One record per approved command, 149 bytes, written by the core into the book next to the command it approves.

| Bytes | Field |
|---|---|
| 0 | Version: `0x01` |
| 1 to 16 | The credential that actually signed the approval: its 16-byte identifier body. It may differ from the credential the caller signed in with |
| 17 to 48 | The command digest of F1 |
| 49 to 80 | The assertion digest: SHA-256 of these framed fields, in order, the tag `aledgent/approval/assertion`, `0x01`, the credential's raw id, the authenticator data, the client data JSON and the signature, each as the device returned them |
| 81 to 84 | The key id: an unsigned number, big-endian, naming the ingress signing key in the trusted set the core holds |
| 85 to 148 | Ingress's signature over the *signed message* below |

**The signed message** is the framed concatenation of: the tag `aledgent/approval/record`, `0x01`, the credential,
the command digest, the assertion digest and the key id. It is the bytes themselves that are signed, not a hash of
them; a scheme that hashes does so inside.

**What is open.** The signature's length (64 above) and scheme belong to B. `DEC-19`'s reasoning (no P-256 in the
core program) and the key boundary (`F-035`) lead to the proposal, in `F-041`, of CipherClerk's Schnorr signature
over Ristretto255, which the core already verifies for `key` credentials. The version byte pins whichever is chosen;
changing it makes a version 2. The key id is a number assigned by the trusted set, so that rotating means adding the
new key, then removing the old; how the set is held is B's. For a `key` credential's approval (F4) the record's
assertion digest is the digest of its signature message, and the record's credential is the key's.

**What the assertion digest is for.** The record does not hold the assertion (`REQ-AU-8`: nothing of a credential
reaches a log). Anyone who kept the assertion, such as the client application, can show it matches the record.

**What the record proves, stated plainly.** It proves that ingress said a given credential approved a given digest,
and that the core accepted that statement. It does not prove, to a third party who sees only the book, that a
passkey signed; that needs the assertion and the credential's public key. The core does not verify the passkey
itself (`DEC-19`).

### F4 — A `key` credential's approval, version 1

The owner accepted the proposal (2026-10-21): the key signs the command digest.

The message is the framed concatenation of: the tag `aledgent/approval/key`, `0x01`, the command digest of F1, and
the expiry as eight bytes, big-endian, in milliseconds since the Unix epoch. **The key signs the 32-byte SHA-256 of
that message**, because the library's signature takes a 32-byte message (`verify_signature`, as for a reclaim proof).
The signature is CipherClerk's Schnorr signature over Ristretto255. Step 1 returns the digest and an expiry
(ingress's clock plus the approval lifetime, 120 000 milliseconds by default); the key's holder signs; step 5 carries
the signature and the expiry, and ingress refuses an expiry that has passed or that is further away than the
lifetime. Ingress verifies the signature against the credential's verifier from the current credential list. The vector
(F5) includes one such signature, computed with the library.

A key's approval has no one-time number: replaying it within its short validity is a repeat of the same command
under the same client key, and returns the first result. If the first attempt was refused before the command was
logged, the repeat is the retry the caller wanted.

### F5 — A worked test vector

Five commands and one challenge, computed by the script below, which is standard-library Python and reproduces every
value shown. The script is here, not in the repository, so that it carries no code to maintain; a pinned copy of the
values is the test `T-040-02` adds. The command bodies are written as the amendment expects ingress to write them
(`T-040-02` replaces them with ingress's real canonical output and fails if a member name or a form differs). Case 1 has nine framed fields and a preimage of 316 bytes.

The cases: (1) a one-leg `transfer`; (2) a two-leg `transfer` with a deadline, acting for another identity;
(3) a `capture`, where the reservation is in the command text; (4) an operator `approve_reversal` whose
justification holds a quote, a backslash, U+007F, an accented letter and a character above U+FFFF; (5) a
`register_credential` of a device-bound credential, where the payload digest carries the registration. The
challenge, the assertion digest, the record's signed message and the key approval are for case 1.

```python
import base64, hashlib, hmac, json, struct

ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"          # Crockford base32, upper case

def did(kind, byte):                                   # did:aledgent:<kind>:<26 characters> from 16 equal bytes
    v = int.from_bytes(bytes([byte]) * 16, "big")
    return f"did:aledgent:{kind}:" + "".join(ALPHABET[(v >> (5 * (25 - i))) & 31] for i in range(26))

def frame(*fields):                                    # each field: 4-byte big-endian length, then the bytes
    return b"".join(struct.pack(">I", len(f)) + f for f in fields)

sha = lambda b: hashlib.sha256(b).digest()
def canon(body):                                       # the canonical text of F1
    return json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()

DEPLOYMENT = bytes([0xD9]) * 16                        # the book's identity (genesis)
ENVIRONMENT = b"prod-eu"                               # the new ingress setting: a name a restore does not copy
ME, OTHER = did("id", 0x11), did("id", 0x12)
A1, A2, A3 = did("acct", 0x33), did("acct", 0x44), did("acct", 0x55)

def command_digest(client_key, initiator, on_behalf_of, payload_digest, text):
    obo = [b"\x00"] if on_behalf_of is None else [b"\x01", on_behalf_of.encode()]
    pre = frame(b"aledgent/approval/command", b"\x01", DEPLOYMENT, ENVIRONMENT, client_key.encode(),
                initiator.encode(), *obo, payload_digest, text)
    return pre, sha(pre)

cases = {}
# 1. one-leg transfer, no deadline, acting for nobody else
cases["1 transfer, one leg"] = (b"pay-2026-10-21-0001", ME, None, b"", canon(
    {"command": "transfer", "legs": [{"from": A1, "to": A2, "asset": "USD", "amount": "2500"}]}))
# 2. two legs, a deadline, acting for another identity
cases["2 transfer, two legs, deadline, on behalf of"] = (b"pay-2026-10-21-0002", ME, OTHER, b"", canon(
    {"command": "transfer", "deadline": 1800000600000, "legs": [
        {"from": A1, "to": A2, "asset": "USD", "amount": "2500"},
        {"from": A1, "to": A3, "asset": "EURC", "amount": "10"}]}))
# 3. capture: the reservation is in the command text
cases["3 capture"] = (b"cap-0003", ME, None, b"", canon(
    {"command": "capture", "reservation": did("op", 0x22), "destination": A2, "cleared": "1000"}))
# 4. an escaped string: quote, backslash, DEL, e-acute, and a character above U+FFFF
just = 'ok "x" \\ \x7f é \U0001F600'
cases["4 approve_reversal, escapes"] = (b"rev-0004", ME, None, b"", canon(
    {"command": "operator", "op": "approve_reversal", "proposal": did("op", 0x66), "justification": just}))
# 5. adding a device-bound credential: the registration payload is bound by its digest
client_data, attestation = b'{"type":"webauthn.create"}', bytes(range(0x40, 0x60))
payload = sha(frame(b"aledgent/approval/payload", b"\x01", client_data, attestation))
cases["5 register_credential, device-bound"] = (b"cred-0005", ME, None, payload, canon(
    {"command": "register_credential", "marker": "ordinary", "scheme": "webauthn-es256", "verifier": ""}))

out = []
digests = {}
for name, (ck, who, obo, pay, text) in cases.items():
    pre, d = command_digest(ck.decode(), who, obo, pay, text)
    digests[name] = d
    out += [f"case {name}", f"  text: {text.decode()}", f"  preimage bytes: {len(pre)}", f"  command digest: {d.hex()}"]
out.append(f"  (case 5) payload digest: {payload.hex()}")

# The approval challenge, version 1, for case 1 (the seal covers the deployment too).
d1 = digests["1 transfer, one leg"]
authn_key, nonce, expires_at = bytes(range(32)), bytes([0x55]) * 16, 1_800_000_120_000      # milliseconds
identity_digest = sha(ME.encode())
exp = struct.pack(">Q", expires_at)
m = hmac.new(authn_key, b"aledgent/approval-challenge/v1", hashlib.sha256)
for part in (nonce, exp, b"\x03", identity_digest, d1, DEPLOYMENT, ENVIRONMENT):
    m.update(struct.pack(">Q", len(part)) + part)       # the seal frames with 8-byte lengths
challenge = nonce + exp + b"\x03" + identity_digest + d1 + m.digest()
out += [f"identity digest: {identity_digest.hex()}", f"challenge (121 bytes): {challenge.hex()}"]

# The assertion digest and the record's signed message (stand-in assertion bytes).
raw_id    = bytes(range(0x80, 0x90))
auth_data = sha(b"example.com") + b"\x05" + struct.pack(">I", 7)       # rpIdHash, flags UP+UV, count 7
b64 = base64.urlsafe_b64encode(challenge).rstrip(b"=").decode()
client_data_json = ('{"type":"webauthn.get","challenge":"%s","origin":"https://auth.example.com"}' % b64).encode()
signature = bytes([0xA5]) * 70
assertion_digest = sha(frame(b"aledgent/approval/assertion", b"\x01", raw_id, auth_data, client_data_json, signature))
credential = bytes([0x66]) * 16
record_message = frame(b"aledgent/approval/record", b"\x01", credential, d1, assertion_digest, struct.pack(">I", 1))
out += [f"assertion digest: {assertion_digest.hex()}", f"record signed message: {record_message.hex()}"]

# A `key` credential's approval: the key signs the 32-byte SHA-256 of this framed message.
key_message = frame(b"aledgent/approval/key", b"\x01", d1, struct.pack(">Q", expires_at))
out.append(f"key approval framed message: {key_message.hex()}")
out.append(f"key approval signed value (SHA-256 of it): {sha(key_message).hex()}")
print("\n".join(out))
```

Output, run with `python3 -I`:

```text
case 1 transfer, one leg
  text: {"command":"transfer","legs":[{"amount":"2500","asset":"USD","from":"did:aledgent:acct:1K6CSK6CSK6CSK6CSK6CSK6CSK","to":"did:aledgent:acct:248H248H248H248H248H248H24"}]}
  preimage bytes: 316
  command digest: c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de
case 2 transfer, two legs, deadline, on behalf of
  text: {"command":"transfer","deadline":1800000600000,"legs":[{"amount":"2500","asset":"USD","from":"did:aledgent:acct:1K6CSK6CSK6CSK6CSK6CSK6CSK","to":"did:aledgent:acct:248H248H248H248H248H248H24"},{"amount":"10","asset":"EURC","from":"did:aledgent:acct:1K6CSK6CSK6CSK6CSK6CSK6CSK","to":"did:aledgent:acct:2NANANANANANANANANANANANAN"}]}
  preimage bytes: 524
  command digest: bdb0423d7c5556735790fd26ce46885507bb30be9de7162214ee5efc55b050cd
case 3 capture
  text: {"cleared":"1000","command":"capture","destination":"did:aledgent:acct:248H248H248H248H248H248H24","reservation":"did:aledgent:op:1248H248H248H248H248H248H2"}
  preimage bytes: 294
  command digest: bf20de62f3858c88cbe57a6af41d18ee7fa84ce5c7485f059be42debfe52c689
case 4 approve_reversal, escapes
  text: {"command":"operator","justification":"ok \"x\" \\ \u007f \u00e9 \ud83d\ude00","op":"approve_reversal","proposal":"did:aledgent:op:36CSK6CSK6CSK6CSK6CSK6CSK6"}
  preimage bytes: 295
  command digest: 95ba018a974a400dad4235a9ea3bd7285f23a20d19b1fb042e13ca831c09b8d4
case 5 register_credential, device-bound
  text: {"command":"register_credential","marker":"ordinary","scheme":"webauthn-es256","verifier":""}
  preimage bytes: 262
  command digest: 19c7a71988ef273bcb06529c74089aa18812dcc8e9cd5fab26870908c1552f61
  (case 5) payload digest: 186a51ad1b6bbf474d2cbb0d92933e1d9beb63c8a25205f9b223f112fe407e50
identity digest: 073c7ff1e2f11c45844b0a9df8966d57d7b4f4bcf5acdddb8655f35911445a57
challenge (121 bytes): 55555555555555555555555555555555000001a3185e24c003073c7ff1e2f11c45844b0a9df8966d57d7b4f4bcf5acdddb8655f35911445a57c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de87dddea4e18379c680c22878caa522b8b607178f8ed04d50cd5acd6593b0e7fa
assertion digest: b3839496408b93ebc48701d29ea062231557b19969add218999e24e922a1e9b9
record signed message: 00000018616c656467656e742f617070726f76616c2f7265636f72640000000101000000106666666666666666666666666666666600000020c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de00000020b3839496408b93ebc48701d29ea062231557b19969add218999e24e922a1e9b90000000400000001
key approval framed message: 00000015616c656467656e742f617070726f76616c2f6b6579000000010100000020c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de00000008000001a3185e24c0
key approval signed value (SHA-256 of it): 246aa66842214fd544fd859f6744f1ad273c5946eca4ef47bc614525e2764773
```

**The key approval's signature** is not computed by the Python script. It was computed by a short Rust program
that calls CipherClerk's own `SecretKey::sign` at the revision this repository pins (`e9a7b08`), with a test secret
(thirty-one bytes `0x2A`, then `0x00`) and a nonce stream of the bytes 0, 1, 2, and so on, used only here (real
signing draws a random nonce). It signs the value printed above as "signed value":

```rust
let sk = SecretKey::from_bytes({ let mut b = [0x2A; 32]; b[31] = 0; b }).unwrap();
let sig = sk.sign(&signed_value, &mut Fixed(0)); // Fixed: bytes 0, 1, 2, ... for the 64-byte nonce
```

```text
public key: 908999f7ad86c97588fc3c44e74cbeecc89df62fddd29a5b763464e28d7d5467
signature r: 7c107ed2840904ea12ce0be6d4d774a14c00b91c21f71dc96c1de2b087a33228
signature s: b01981c1f9ace19f02b50e4606c2740b863e2aea66372465e433d25b188eeb08
verifies: true
```

The assertion is not real, and its digest shows only the arithmetic. The record's signature (Feature B's) is not a
fixed value and is not shown; its signed message is. The vector is a *drafting* check,
computed once by the drafter's script: it pins nothing until `T-040-02` writes it into a test and a second
implementation agrees.

---

## How one command is approved (what Feature A does)

1. The client authenticates and sends the command to a route that **takes nothing in** and asks for its approval.
   Ingress reads how the caller proved who it is (the table above), validates and normalises the command
   exactly as it would for submission (a shape fault is `ERR-MalformedCommand` here, as there), reads or derives the
   client key, and computes the command digest of F1 from *its own* copy.
2. If the command needs no approval (a bearer secret, or a command outside the table), it says so and the client submits normally.
3. Otherwise, for a device-bound credential, ingress issues the approval challenge of F2 and returns it with the
   digest, so the client can show the person what is being approved. For a key, it returns the digest and the
   expiry of F4.
4. The client has the credential sign. For a device, the browser's list of allowed credentials is the initiating
   identity's active device-bound credentials, any of them (a phone's may approve while the person browses with
   another), and the device's own check is required.
5. The client submits the command with the approval attached. Ingress recomputes the digest from the command in
   *this* request (it never trusts one the client sends), then checks, in this order and with one refusal for every
   failure of the proof: the shape; whether a first result already exists for the client key (a retry: answered,
   done); whether the command needs an approval; the challenge's seal, purpose, length, deployment and expiry; that
   its command digest is this command's and its identity digest is the initiator's; that the approving credential is
   an active member of the initiator's set, **read from the current credential list** (an unreadable list is
   `ERR-DecisionUnavailable`); the credential rule for upgrade commands; that the assertion verifies, as sign-in
   checks it but with the device's own check required; that the challenge's number was never used; and the sign
   count rises. For a key: the credential is a member, the expiry, the signature.
6. Only then does ingress log the command and pass it on. Nothing before this point leaves a row.

A challenge whose digest or identity does not match is refused *before* its number is spent, so a misrouted attempt
does not burn the right person's challenge (as amendment 0027 does). Any other failure after the number is spent
leaves it spent; the client asks again.

**How the two steps meet the spec.** Step 1 is not a command: it takes nothing in, mints no row, changes no state.
`REQ-CS-2` and `REQ-CS-8` apply to the command from step 5. `NFR-1`'s fifty milliseconds at p99 are measured inside
aledgent from step 5, and now include work they did not: one assertion verification, a read of the current credential list, and two
durable writes (the number's spending and the sign count's raising). That is a risk to the bound, not a change of
it. A task of `F-040` shows the bound holds with approvals on (`T-040-07`).

---

## Gaps this amendment names and does not decide

- **G1 — The device shows no amount.** See the trade-off of R1. Nothing in this amendment makes the device display
  what it approves.
- **G2 — The approving credential's scope.** Any credential of the identity may approve (ruling 9), so the credential
  that approves can differ from the one the command is attributed to (`REQ-AU-7`). "Which commands" explains which
  scope checks ingress can make and which need a read of the book or the core. Q-C.
- **G3 — A credential added by the identities principal.** The identities principal is a service and presents a
  bearer secret, so it is exempt, and `REQ-AU-20` lets it add a credential to a managed identity's set. A person
  who wants the approval to cover that path needs the integrating party to stop using it, or Q-I.
- **G4 — The core input lacks the client key and the environment.** F1's fifth field is the client key; the core
  receives only the operation reference derived from it, and it never sees `environment` (the fourth field). Feature A
  needs nothing from the core. For B to recompute the digest, its input must carry the client key, the environment,
  the acted-for flag, the canonical text and the payload digest. That is a change to
  what the core receives, and part of `F-041`.
- **G5 — Today the record of approvals is ingress's, if anyone's.** Between A and B, an approval is written durably
  only in ingress's note (`T-040-09`, ruled).

## Choices the drafter made, for review to check

These are not rulings. Each is something the brief did not state and the drafter had to fix to write exact bytes.

1. **Canonical text is sorted-key JSON, not a binary encoding per command.** A binary encoding for each of about
   fifty command kinds would need a table that grows with every new command and would leave a command with no
   entry unprotected. JSON text needs no table, covers every command at once and is reproducible in any language.
   The cost is on B: the core must not trust text it did not check (F1).
2. **The digest holds the client key and the deployment, not the operation reference.** See F1.
3. **The record holds the credential and digests only, not a time.** The book already stamps the command's input
   with the clock ingress gave the core.
4. **The approval's expiry is the deployment's existing challenge lifetime, 120 seconds.** Not a new setting.
5. **A bad approval is `ERR-CredentialInvalid`, with no detail**, like every failure of a sign-in, so the route is
   not an oracle (`REQ-AU-8`).
6. **An unreadable credential list refuses the command** rather than falling back to a stored copy (R1). The alternative
   is Q-H.
7. **A key's approval is not single-use**, because a repeat is a retry (F4).

## Rulings applied (the owner's answers of 2026-10-21 to the first draft's ten questions)

| # | Question | Answer, as applied |
|---|---|---|
| 1 | The refusal's name | `ERR-CommandApprovalRequired` (R2) |
| 2 | When the core learns the refusal | With the next core, after the throughput measurement, not the next re-genesis (R2) |
| 3 | Which passkey approves | Any passkey of the identity, not only the one that signed in. The challenge binds the identity; the record names the credential that signed (F2, F3). Exception: `upgrade` (R1) |
| 4 | The six borderline commands | Only `release` needs approval (the table behind R1) |
| 5 | People without a passkey | API keys stay exempt, and so do configured tokens (ruling 4). Only credentials of a scheme that signs (`key`, passkey) can approve (R1, "How the door decides") |
| 6 | The hash | SHA-256. Reasons: WebAuthn hashes with it and browsers and hardware support it natively. Cost: the core links only BLAKE2b, so Feature B adds SHA-256 (F1) |
| 7 | User verification | Required on every approval (R1, F2) |
| 8 | A note of who approved | Yes: ingress keeps the credential and the two digests beside the command, never the assertion (`T-040-09`) |
| 9 | Acceptable slowdown for Feature B | The owner's rule is kept visible in `F-041` (1.5 standard deviations of the baseline's run-to-run variation) next to a sounder proposal; see Q-F |
| 10 | When Feature B ships | In the next core, as a core upgrade, not in the next re-genesis |
| Q-B | What a `key` credential's approval is | A signature over the command digest (F4); the vector includes one |

## Questions for the owner (still open)

Plain English, one decision each. Terms first: the **digest** is the short fingerprint of a command that the approval
signs. **Ingress** is the front door that receives commands. **The core** is the part that keeps the book and decides.
**The next core** is the next release of that part. The **credential list** is the devices and keys an account's holder has registered. The **upgrade mode** is the rule that says who may schedule a new version of the book's program (amendment 0034). An **idempotency key** (the `client key` in the spec) is the label a caller chooses so that repeating a request is safe. Q-E of the earlier drafts was withdrawn: it asked about a setting
that no longer exists. Nothing here blocks reading the amendment; Q-A is not needed to approve it.

**Q-A. How should the book itself learn that a person approved? (You asked for both ways to be measured first.)**
- *Asked.* Today ingress checks the person's device and the book is told nothing. Later the book can be told, in one
  of two ways. **Way 1:** ingress tells the core "approved" with the command's digest, and the core trusts ingress,
  as it already does for one recovery command. **Way 2:** ingress also signs a small record for each command, and
  the core stores it in the book.
- *Why it matters.* Way 2 leaves proof in the book of who approved what, and refuses commands that skip ingress. It
  costs space and time on every command. Way 1 costs less and leaves less proof.
- *What neither does.* If ingress itself is taken over, it can say "approved" for anything in both ways. Only the core
  checking the device's proof itself would stop that, and that was ruled out earlier (the core carries no code for it).
- *If you say nothing.* Both are measured and nothing is built.
- *Proposal.* Measure both, then choose with the numbers.

**Q-C. When a second device approves a command, should that device's own limits count?**
- *Asked.* Some devices are limited to certain accounts (a card-only passkey, say). Should a device limited to
  account X be able to approve a transfer out of account Y?
- *Example.* A phone limited to account X taps "approve" on a transfer from account Y.
- *Cost of "yes".* For commands that name their accounts, ingress can check this, but it must first learn each
  device's limits (a small change to what it reads from the core). For commands that only name a reservation, a
  grant or a pending recovery, the account is known only to the book, so only an unlimited device could approve them
  until the core can check.
- *If you say nothing.* It waits (`T-040-13`) and limited devices may approve anything.
- *Proposal.* Yes, the limits count, with that rule for the second group.

**Q-D. How do we keep the spec's list of errors matching the program's list until the next core?**
- *Asked.* An automatic test insists the spec's list of error names and the program's list are the same. The new error
  (`ERR-CommandApprovalRequired`) will be in the spec before the next core, which carries it in its list, exists.
- *Choices.* (a) Allow one named exception in the test until the next core. It weakens the guarantee that the two
  lists always match, for a while. (b) Switch the new rule on nowhere until the next core is out. No exception, but
  the protection comes later. Also, which web response code (the number a web server answers with, such as 403 for "forbidden") should the new error carry? Proposal: the same as "not
  authorised".
- *If you say nothing.* (a).
- *Proposal.* (a).

**Q-F. Which rule decides whether approvals slow the core too much? (Feature B's measurement.)**
- *Asked.* We will run the same speed test many times with and without the records and compare. Your rule: acceptable
  if the speed with records is no more than 1.5 "standard deviations" below normal, where a standard deviation is how
  much the speed varies between repeated identical runs.
- *The problem.* With ten runs, a real slowdown equal to one such variation (say 5%) still passes about 87 times in
  100, and the noisier the machine, the more slowdown it lets through.
- *The alternative.* You state the largest slowdown you accept as a plain percentage (say 3%), from what capacity you
  need. We then work out how many runs are needed to see a slowdown of that size, compare speeds as a ratio, and pass
  only if we are 95% sure the loss is below your percentage (a standard statistical test for a small number of runs, "one-sided" because only a loss matters, with the number of
  warm-up runs also fixed in advance).
- *If you say nothing.* Both are reported; your 1.5 rule decides.
- *Proposal.* The percentage rule decides. Which percentage?

**Q-G. Which of these commands should need an approval?** One line each on what the command does and the proposal.
- `register_integrating_party` records a partner company in the book; `tie_principal` links one of that partner's
  service accounts to it, which decides whose idempotency keys (the labels callers choose to make repeats safe) they share and who sees their
  attached notes. *Proposal: yes.*
- `resume` restarts a stalled payment so it can go on to the outside world. *Proposal: yes.*
- `upgrade` schedules a new version of the book's program. *Proposal: yes, by the credential the upgrade mode names for the caller.*
- `reschedule_upgrade` postpones and `cancel_upgrade` calls off a scheduled upgrade. *Proposal: no*, because
  amendment 0034 lets any one member do it alone and quickly in an emergency. The cost: a stolen session of a member
  could keep postponing a security update with no device.
- `reclaim_custody` is an account holder taking control of their account back; it already carries its own device
  proof. *Proposal: no approval, but its device check should always be on.*
- `issue_disclosure` produces a signed statement about the book for a third party, so it can reveal confidential
  facts. *Proposal: no approval (it moves nothing), but say if confidentiality should count.*
- *If you say nothing.* The proposals stand.

**Q-H. While the core cannot be reached, should writes that need an approval be refused?**
- *Asked.* To accept an approval, ingress must check that the approving device is still on the person's current
  credential list. Today ingress keeps accepting transfers when the core is down, using a saved copy of that list.
- *Choices.* (1) Refuse these writes until the core returns ("try again", `ERR-DecisionUnavailable`). Safe, but
  during an outage transfers and withdrawals are refused instead of queued. (2) Accept using the saved copy. Nothing
  is refused, but a device the person has just removed (a stolen phone) could still approve during the outage.
- *If you say nothing.* (1).
- *Proposal.* (1).

**Q-I. May an API key or a configured token change who may act, or change an account's credentials?** (The one gap
the design leaves.)
- *Asked.* Your ruling exempts API keys and tokens an operator configured, because no person is behind them. But a
  stolen one can add the thief's own device to an account's credentials (and remove others), and that device then
  approves everything.
- *Choices.* **No:** the gap stays and is documented. **Yes, for credential and authority changes only** (add or
  remove a credential, import credentials, grant or revoke authority): such a change needs an approval from a person's
  device or key whenever the account has one, even when it comes with an API key. The cost: integrators that use API
  keys could no longer rotate keys or change credentials without a person at a device; and ingress would have to read
  the account's credential list on each such command, an extra read.
- *If you say nothing.* No (as ruled).
- *Proposal.* Yes, for those changes only.

**Q-J. Should a withdrawal take two approvals?**
- *Asked.* A withdrawal is two commands: `withdraw` reserves the money, and `submit_outbound` says where it goes.
  Each needs an approval, so you tap once to reserve 2500 and again to name the destination.
- *Choices.* Keep two taps (no change to how withdrawals are requested), or put the destination into the first
  command so one tap covers both (a larger change to the public way of requesting a withdrawal).
- *If you say nothing.* Two taps.
- *Proposal.* Two taps.

## What does not change

`REQ-AU-1` to `REQ-AU-22` and every acceptance criterion of `US-A1` to `US-A9`, apart from the sentence R5 adds to
`REQ-AU-20`. The session and its fifteen minutes. The core's own check, at decision, that a credential is an active
member of its identity's set and covers the account (`REQ-AU-3`, `REQ-AU-20`): ingress's approval is in addition to
it, not instead. Idempotency (`REQ-OP-10`, `REQ-OP-11`). Amendment 0027's registration challenge, unchanged.
Amendments 0034 and 0035: the upgrade authority is still held with the named credential, and every way out is still
decided and still takes no approval.

## Plan

On approval, `PLAN.md` gets: a new decision (`DEC-28`), recording F1 to F4 and the vector, the order of checks, the table of how the door decides, the words the spec leaves to the plan (passkey, configured token, relying
party, environment) and the delivery of A through ingress; `DEC-19` gains one sentence ("A passkey assertion buys a session for
sign-in and reads; a write needs an approval, `DEC-28`"); and two rows in §4, `F-040` and `F-041`. Those edits wait
for the approval, so they do not collide with the open pull requests that edit `PLAN.md`. Until then the features cite
`[PENDING AMENDMENT 0037]`.

`F-039` is already taken, by the documentation feature of `PLAN.md` §4 and `M-3D`, and `F-038`'s tasks cite it.
The ingress feature is therefore **`F-040`** and the core feature **`F-041`**. The handover that drafted them called them
`F-039` and `F-040` provisionally, "until the feature gets its number".

- `features/F-040-passkey-approval-ingress/` is Feature A: `plan.md`, `tasks.md`.
- `features/F-041-passkey-approval-core/` is Feature B, a stub: blocked by A and by the measurement of `T-041-00`.
