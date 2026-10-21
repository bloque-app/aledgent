<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0037 — A person approves each command that matters, one command at a time

**Status:** **awaiting the owner's approval.** The direction was ruled on 2026-10-19 (the ruling that a passkey
approves one transaction, not a session) and on 2026-10-20 and 2026-10-21 (owner answers on the Decisions form:
the work is split in two features; this amendment and its design are public in this repository, ruling `w12` (a)).
The owner answered the first draft's ten questions on 2026-10-21; this version applies those answers (see "Rulings
applied", near the end). The text below is the drafter's reading of the rulings, worked through to rules and byte
formats. It is **still not approved**. Nothing in `SPEC.md` or `PLAN.md` changes with this file: the proposed spec text is marked as such, and
the plan sites will cite `[PENDING AMENDMENT 0037]` once the owner approves. The features that build it
(`F-040`, `F-041`) merge no code before that.
**Target:** `SPEC.md` v0.4.28 as on `develop`. Amendment 0036 (PR #50, also awaiting approval) touches `REQ-RT-20`
and `US-K7` and no clause named here. `PLAN.md` `DEC-19`, plus a new decision (see "Plan").
**Date:** 2026-10-21
**Number:** 0036 is taken by an open pull request (#50); this is the next free one.
**Adds:** `REQ-AU-23`, `US-A10` with `AC-A10.1` to `AC-A10.11`, one refusal in §10 (`ERR-CommandApprovalRequired`), and one sentence in `REQ-CS-8`. Nothing is withdrawn. Nothing existing is renumbered.
**Serves:** `REQ-AU-1`, `REQ-AU-7`, `REQ-AU-20`, `REQ-AU-21`, `REQ-CS-4`, `REQ-OP-10`, `NFR-1`, `NFR-7`.
**Rationale in one line:** today a passkey sign-in buys a session that serves every action for fifteen minutes, so
whoever holds a stolen session token can move the holder's value and even add their own passkey; one approval per
command, made on the holder's device, takes that away.

---

## In plain words

Some terms, once, so the rest reads without a glossary.

- **Approval.** A proof, made by a person at their own device, that they agree to *this one command*. It is not a
  sign-in and it is not a session. It works once.
- **Command digest.** A short fixed-length fingerprint of the command: amount, destination, who asks, the keys that
  name the operation. Change any of those and the fingerprint changes. It is what the passkey is asked to sign, so
  the signature belongs to one command and cannot be moved to another.
- **Approval challenge.** The random-looking bytes ingress hands the device to sign. They carry the command digest, an
  expiry and a one-time number, and they are sealed with a secret only ingress holds, so ingress can check later
  that it issued them without keeping a copy.
- **Approval record.** What Feature B will write into the book for each approved command, so the book itself shows a
  person approved it. This amendment fixes its bytes now, so that B cannot change what A frozen.
- **Ingress.** The component that receives commands (`REQ-AU-21`). Feature A changes ingress only.

## The problem

A passkey sign-in at ingress returns a session token that lasts fifteen minutes and serves any number of commands
(`DEC-19`). The token is a bearer secret: whoever holds it acts as the holder. So, within those minutes, a stolen or
leaked token can:

- move value out of the holder's accounts;
- grant authority over them;
- add the thief's own passkey to the holder's credential set, which outlives the session. An earlier
  design left this as an accepted risk; the ruling of 2026-10-19 removes the acceptance.

The passkey proves possession of a device when it signs in. After that, nothing asks the device again.

## The rulings this serves

1. **A passkey assertion approves one transaction, not a session** (ruling of 2026-10-19).
2. **Two features** (owner, 2026-10-20/21). **Feature A** is ingress only: it asks for the approval, checks it and
   refuses a command without one; it touches no core code and ships first. **Feature B** is the core recording the
   approval in the book; it waits for A and for a throughput measurement.
3. **Scope.** Approval is required for every command that moves or reserves value, changes who may act, or changes a
   credential. Reads and sign-in need none.
4. **Callers.** Partner API keys, service tokens and the core's own timers and settlement are exempt, because no
   person is behind them. A deployment may also require approval for every human credential.
5. **Sessions** stay for sign-in and reads. Every write, and adding a passkey, asks for an approval.
6. **A retry** of an approved command under the same client key is answered from the first result and needs no new
   approval.
7. **Place.** The amendment and the design are public (`w12` (a)).
8. **Schemes.** The per-transaction rule applies only to credentials of a scheme that *signs*: `key` and passkey
   (`webauthn-es256`). It does not apply to `api-key`, which signs nothing (2026-10-21).
9. **Any of the identity's passkeys may approve**, not only the one that signed in (2026-10-21).

## What this amendment does not decide

Plainly, so nobody reads a choice into silence. Each is also in "Questions for the owner (still open)" or "Gaps".

- Which design Feature B takes: the core trusting a digest of ingress's approval, or the core recording an
  ingress-signed record per command (the fork in "Questions for the owner (still open)", Q-A).
- The bytes of the approval a `key` credential gives (G9).
- Whether the approving passkey's own scope must cover the accounts a command touches (G2).
- How the closed list of refusals is kept true between Feature A and the next core (R2, Q-D).
- Anything about Feature B beyond its record's bytes (F3): its signature scheme and its threshold's confirmation.

---

## R1 — `REQ-AU-23` (new), proposed text for `SPEC.md` §5.9, after `REQ-AU-22`

> **Proposed text. Applied nowhere.**
>
> **`REQ-AU-23`** **A person approves each command that matters, one command at a time.** An **approval** is a
> proof of possession, made with a credential of the initiating identity whose scheme signs (a key, or a
> device-bound credential), over one command: who initiates it, which operation it acts under, its client key, and
> everything it carries that decides what it does. An approval approves that command and no other, once. A session, a token or an issued
> secret never stands in for it.
>
> - **Which commands.** Every command that moves or reserves value, changes who may act on an account, or changes
>   a credential. A deployment MAY add commands to that list and MUST NOT remove one. It publishes the list with
>   its limits (§8.2).
> - **Which callers.** A command submitted by a caller that authenticated with a credential of a scheme that signs
>   (a key, or a device-bound credential) needs an approval. A credential of a scheme that signs nothing, such as
>   an issued secret, is exempt, and so is a configured service token and any input the book gives itself (a
>   deadline, a settlement observation): no person is behind them.
> - **Who approves.** Any active credential of the initiating identity's set that is of a scheme that signs, not
>   only the one the caller signed in with. A device-bound approval MUST carry the device's user verification (its
>   PIN, fingerprint or face), whatever the deployment asks of sign-in.
> - **Sessions.** A session proves who is calling, for sign-in and for reads. It does not approve a write. Adding a
>   device-bound credential to a set needs an approval from a credential already in that set, in addition to the
>   ceremony of `REQ-AU-20`.
> - **No approval.** A command that needs an approval and carries none MUST be refused with the refusal of R2, and
>   nothing is taken in: no operation reference, no row, no state.
> - **A bad approval.** An approval that does not verify, that was already used, that has expired, that was
>   issued for another command or for another identity, or that a credential outside the initiating identity's set
>   made, or that lacks user verification, MUST be refused with `ERR-CredentialInvalid`, whichever of those it is, with nothing taken in
>   (`REQ-ER-3`).
> - **Once, everywhere.** An approval is valid for a stated, short time and is accepted once, by any instance of the
>   component that receives commands and also after a restart.
> - **A retry.** A repeat of a command that was approved and taken in, under the same client key, returns the first
>   result and needs no new approval (`REQ-OP-10`). The same client key with a different command is
>   `ERR-ClientKeyReused`, as always.
> - **Order of checks.** A shape fault is found first (`REQ-ER-3`), then whether a first result exists for the key
>   (a retry), then whether an approval is needed and valid. A command with a shape fault never reaches the
>   question of approval.
> - **Two steps, one command.** Obtaining what a device must sign takes nothing in. The command is taken in, and
>   `REQ-CS-2`, `REQ-CS-8` and `NFR-1` apply to it, from the request that carries its approval.
> - **Where it is enforced.** By the component that receives commands. This requirement does not say whether the
>   book also records the approval.
> - **Declared.** A deployment that does not require approvals says so with its other declarations (`REQ-AR-5`).

*Trade-off, stated plainly.*

- **One more round trip per write.** The device must be asked for each approval, so every command in scope costs
  two calls instead of one, and a person at a device. A client that submits many commands for a person (a
  batch import, a payroll run) can no longer do it on one session. That is the point, and it is also the cost.
- **A tap proves presence, not understanding.** The device's prompt does not show the amount or the destination.
  The approval proves that someone with the device was present and that the application asked the device to
  sign *this* digest. It does not prove the person read the amount. An application that is itself compromised can
  show one amount and ask for another. What the approval stops is a stolen or leaked session, a copied token and
  a program that submits commands on its own. It does not stop malicious code in the holder's own browser.
  Showing the person what they approve is the client's duty; this amendment gives it the digest and the command
  to show, and no more.
- **Exempt callers are exempt.** An API key (it signs nothing, so it cannot approve) and a service token still move
  value with no approval. The
  identities principal can still add a credential to the set of an identity it manages (`REQ-AU-20`, clause R2 of
  amendment 0025), with no approval, because it is a service. Trust in those callers is the integrating party's
  and the operator's, as it is today.
- **Until Feature B, the book trusts ingress.** The core is not told about the approval. A compromised ingress
  could log a command with no approval, just as it could log any command. The first feature makes a *stolen
  session* useless; it does not make a *compromised ingress* harmless. The second feature narrows that, and
  does not close it either (F3).
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
> | `ERR-CommandApprovalRequired` | A command that needs a person's approval of that one command (`REQ-AU-23`) was submitted without one. A presented approval that does not verify is `ERR-CredentialInvalid` |

**When the core learns it (ruled 2026-10-21).** `Refusal` is an enum in `ledger-core`, compiled into the core program,
and a test checks it against §10 in both directions (`REQ-ER-1`), so §10 and the enum cannot differ. The owner ruled
that the core's side of the new line ships with **the next core**, after the throughput measurement of `F-041`, and not
with the next re-genesis. Feature A ships first, so for a time ingress answers a refusal the core's list does not yet
have. The drafter's reading, for the owner to confirm (Q-D): the line enters §10 when this amendment is applied;
until the next core carries it, ingress holds the refusal in its own type and the two-way test is told about this one
named exception, which is removed in the same change that adds the line to the core. Nothing else about the core
changes in Feature A, and Feature A never sends the core this refusal.

## R3 — `US-A10` and its acceptance criteria (new), proposed text for `SPEC.md` Epic A

> **Proposed text. Applied nowhere.**
>
> **`US-A10`** — As an **identity holder**, nobody holds my value, my authority or my credentials with only a stolen
> session: I approve each command that matters myself, on my own device, one command at a time.
> - `AC-A10.1` Given a command that needs an approval and none attached, Then it is refused with
>   `ERR-CommandApprovalRequired`, nothing is taken in, and no operation reference exists for it (`REQ-AU-23`).
> - `AC-A10.2` Given a valid approval of that command, Then it is taken in and decided as any command is
>   (`REQ-AU-23`, `REQ-CS-8`).
> - `AC-A10.3` Given an approval of one command, When it accompanies a command that differs in amount, destination,
>   client key, operation or initiator, Then it is refused with `ERR-CredentialInvalid` and nothing is taken in
>   (`REQ-AU-23`).
> - `AC-A10.4` Given an approval already used, or expired, or made by a credential of another identity, or made over
>   a challenge issued for another identity, Then it is refused with `ERR-CredentialInvalid`, on any instance and
>   after a restart (`REQ-AU-23`, `REQ-ER-3`).
> - `AC-A10.5` Given a command approved and taken in, When it is repeated under the same client key, Then the first
>   result is returned and no approval is asked for; and When the same key carries a different command, Then it is
>   `ERR-ClientKeyReused` (`REQ-AU-23`, `REQ-OP-10`).
> - `AC-A10.6` Given a command that needs an approval, When it comes with an issued secret (a scheme that signs
>   nothing) or a service token, Then it is taken in without one (`REQ-AU-23`).
> - `AC-A10.11` Given an identity with two passkeys and a session made with the first, When the second approves a
>   command, Then it is accepted, and the record names the second as the approver and the first as the caller
>   (`REQ-AU-23`, `REQ-AU-7`); and When an approval lacks user verification, Then it is refused with
>   `ERR-CredentialInvalid`.
> - `AC-A10.7` Given a session, When it signs in or reads, Then no approval is asked for (`REQ-AU-23`).
> - `AC-A10.8` Given a session and no approval, When a device-bound credential is registered, Then the command is
>   refused with `ERR-CommandApprovalRequired`; and Given the approval of an existing credential in the
>   set, and the registration ceremony of `REQ-AU-20`, Then the credential joins the set (`REQ-AU-23`).
> - `AC-A10.9` Given two independent implementations of the command digest, When both are given one command,
>   Then they produce the same digest, byte for byte (`REQ-AU-23`; the vector of F4).
> - `AC-A10.10` Given a command that needs an approval and has a shape fault, Then it is `ERR-MalformedCommand`, not
>   a refusal for the missing approval (`REQ-AU-23`, `REQ-ER-3`).

## R4 — `REQ-CS-8`, one sentence

Add at the end of the paragraph that begins "The account and security commands MUST always be answered with their
decision":

> **Proposed text. Applied nowhere.**
>
> A command that needs a person's approval (`REQ-AU-23`) is answered, or acknowledged, only once its approval has
> been verified; before that it is refused and nothing is taken in.

---

## Which commands: the table behind R1

The default list is a floor (R1: a deployment may add, not remove). Classified by the three tests of the ruling,
from the commands as ingress has them today (`CommandBody`, `OperatorCommand`). "A person's session" means the caller
authenticated with a credential of a scheme that signs (`key`, passkey); for any other caller the callers rule of R1
applies and the command is exempt.

| Needs an approval when a person's session submits it | Why |
|---|---|
| `transfer`, `reserve`, `withdraw`, `capture`, `release`, `issue`, `redeem` | Move or reserve value |
| `grant_authority`, `revoke_authority`, `delegate_custody`, `name_successor`, `claim_succession`, `cancel_recovery` | Change who may act on an account |
| `register_credential`, `remove_credential` | Change a credential |
| The operator surface, where a person's session submits it: reversals, `resolve`, issuance and operator grants, `import_credentials`, `initiate_recovery` | Same three tests. The operator surface is exempt as a class when a service principal holds it, which is the usual case |

| Exempt by the callers rule, or no test applies | Why |
|---|---|
| Reads, sign-in, `/v1/auth/*` | No write |
| Settlement adapter inputs, ticks, deliveries, `submit_outbound` by the settlement path, `compare_settlement`, `anchor_checkpoint`, `issue_disclosure` | Service callers or the book's own inputs; none moves a holder's value on a person's say |
| `register_asset`, `deprecate_asset`, `bind_asset`, `declare_adapter_support`, `freeze_account`, `unfreeze_account`, `resume`, `submit_evidence` | Operator administration; none of the three tests |
| `complete_enrolment` | No credential of the set exists yet to approve it (`REQ-AU-22`) |
| `register_credential` by the identities principal acting for an identity it manages (`REQ-AU-20`, amendment 0025 R2) | A service is the caller. See Gap G5 |

**Ruled 2026-10-21:** of `release`, `open_account`, `close_account`, `set_answering`, `open_identity` and
`reclaim_custody`, only `release` needs an approval (it changes a reservation the person made). The other five do
not.

A new kind of command must be put on one side of this table on purpose, the way `intake()` already forces every
command into one of two answers. A command nobody classified must not default to exempt (task `T-040-04`).

---

## The formats (carried to the plan; not for `SPEC.md`)

These are what the plan will bind (`PLAN.md`, a new decision) and what the features build. They name the
technology the spec leaves out, as the "Carried to the code" section of amendment 0027 does.

Three formats, each versioned. A and B share the first two. Feature A freezes the first two when it ships, and B
must not change them. The third is fixed here so that B stores what this amendment promised. Where one is
open, F3 says so.

**Conventions for all three.** A *field* is written as four bytes (big-endian) giving its length, then its bytes. This
is the repository's existing framing for request digests (`Canon` in `ledger-core`). Numbers are big-endian.
Text is UTF-8. The version is one byte, `0x01`, and is a field of its own, so a later version changes the
digest instead of reading as an old one.

### F1 — The command digest, version 1

The digest names *the command as the core would receive it* (`CoreInput`): what the core decides on, nothing it
never sees. It is the hash of these eight fields, in this order, framed as above:

| # | Field | Bytes |
|---|---|---|
| 1 | Tag | The ASCII text `aledgent/approval/command` (25 bytes). Separates this digest from every other the repository hashes |
| 2 | Version | `0x01` |
| 3 | Client key | The key as ingress holds it: the bytes of the string the caller sent, or, when the caller sent none, the 64 hexadecimal characters ingress derived and returns (`REQ-OP-11`) |
| 4 | Operation reference | The 16 bytes of the input key: the reference ingress minted for this command (`REQ-OP-10`) |
| 5 | Operation | The text of the core input's `operation` field, a `did:` form. It is the minted reference's `did:` form for most commands, and the reservation's for `capture` and `release` |
| 6 | Initiator | The text of the initiating identity, `did:aledgent:id:…`, as in the core input's `caller.principal` |
| 7 | On behalf of | One byte, `0x00` if the command acts for nobody else; or `0x01` and then a second field with the text of the identity it acts for |
| 8 | Command | The command's *canonical text*, below |

**Canonical text.** The command body (`CommandBody`) after ingress has normalised it, written as JSON with these
rules, so that an implementation in any language writes the same bytes:

- no whitespace outside strings;
- object members in ascending order of their names, compared as UTF-8 bytes (every name is ASCII);
- strings with only the escapes JSON requires (`"` as `\"`, `\` as `\\`, control characters as `\u00xx` in lower case,
  with the short forms `\b \t \n \f \r`), and every character above U+007F as a lower-case `\uxxxx` escape (a surrogate pair
  above U+FFFF);
- numbers as integers in plain decimal, with no sign for a positive one, no leading zeros, no fraction and no
  exponent. No command body holds a non-integer (`INV-7`); one that did would not be approvable;
- amounts, identifiers and enumerated values as ingress normalises them already: amounts are decimal strings
  without leading zeros (`"010"` and `"10"` are one request), identifiers are `did:` forms in upper-case canonical
  base32;
- a member that is absent stays absent. `null` never appears.

This is close to RFC 8785 (JSON canonicalisation) for the values a command holds, and not the same: RFC 8785 writes
a non-ASCII character as raw UTF-8, and this writes it as an escape, so that no implementation can disagree about
byte order or normalisation. Against today's `Command::canonical()` the difference is that the members come
sorted, where today they come in the order the Rust types declare them. Today's bytes are therefore not the
digest's.

**Left out on purpose.**

- *The two times* the core input carries, `received_at` and `now`: they do not exist when the approval is made.
- *The credential* in `caller.credential`: who approved is the approval record's field (F3), and the digest
  says what was approved.
- *Pass-through data* (`REQ-PO-5`): the core never receives it (`REQ-PO-4`), so Feature B could not check it. A
  command's pass-through data is therefore not approved, only the command is. Consequence, stated plainly: a
  holder's approval of a transfer does not cover the free-form note the application attached. A repeat of the
  same client key with another note is still `ERR-ClientKeyReused`.
- *Defaults.* A deadline the command does not carry is not filled in; the core applies the deployment's default at
  decision. The digest covers the command as sent.
- *The surface.* It follows from the command's name.

**The hash (ruled 2026-10-21): SHA-256.** WebAuthn itself hashes the client data with SHA-256, and browsers and
hardware support it natively, so the same function serves the digest, the challenge and the device. The cost is on
Feature B: the core's own program links only BLAKE2b-256, so B adds SHA-256 to the core (a small dependency).
Feature A freezes this choice when it ships.

### F2 — The approval challenge, version 1

What the passkey signs: a *challenge* of 121 bytes, in the same family as the registration challenge of amendment
0027, with its own purpose and its own seal.

| Bytes | Field |
|---|---|
| 0 to 15 | A one-time number, 16 random bytes from the operating system |
| 16 to 23 | The expiry, in milliseconds since the Unix epoch, big-endian: ingress's clock (`Time`) at issue plus the deployment's challenge lifetime (120 000 milliseconds by default) |
| 24 | The purpose: `0x03`, approve. (`0x01` is sign-in, `0x02` registration, as today) |
| 25 to 56 | The identity digest: SHA-256 of the text of the initiating identity, as `target_digest` already does (amendment 0027) |
| 57 to 88 | The command digest of F1 |
| 89 to 120 | The seal: HMAC-SHA-256 under the deployment's authentication key, over the domain `aledgent/approval-challenge/v1` (the text, not framed), then each of the five fields above (number, expiry, purpose, identity digest, command digest), each preceded by its length as eight bytes, big-endian. This is the existing `AuthnKey::mac` framing |

How it differs from what exists, so nothing is confusable: the length (121; a sign-in challenge is 57 and a bound
registration 89), the purpose byte, and the seal's domain. `check_challenge` refuses a 121-byte challenge today
because no length matches; the checker for approvals refuses any other length in the same way.

The browser puts the challenge, in base64url, in `clientDataJSON` as it does today. An approval is an ordinary
assertion over it: same origin check, same relying-party check, same signature check, same sign count in the same durable
store as sign-in, so a cloned authenticator is caught across both.

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
the command digest, the assertion digest and the key id. It is the bytes themselves that are signed, not a hash of them; a
scheme that hashes does so inside.

**What is open.** The signature's length (64 above) and scheme belong to B. `DEC-19`'s reasoning (no P-256 in the
core program) and the key boundary (`F-035`) lead to the proposal, in `F-041`, of CipherClerk's Schnorr
signature over Ristretto255, which the core already verifies for `key` credentials. The version byte pins whichever
is chosen; changing it makes a version 2. The key id is a number assigned by the trusted set, so that
rotating means adding the new key, then removing the old; how the set is held is B's.

**What the assertion digest is for.** The record does not hold the assertion (`REQ-AU-8`: nothing of a credential
reaches a log). Anyone who kept the assertion, such as the client application, can show it matches the record.

**What the record proves, stated plainly.** It proves that ingress said a given credential approved a given digest,
and that the core accepted that statement. It does not prove, to a third party who sees only the book, that a
passkey signed; that needs the assertion and the credential's public key. The core does not verify the passkey
itself (`DEC-19`).

### F4 — A worked test vector

One command, computed by the script below, which is standard-library Python and reproduces every value shown. The
script is here, not in the repository, so that it carries no code to maintain; a pinned copy of the values is the
test `T-040-02` adds.

The command: a `transfer` of 2500 `USD` from one ledger account to another, initiated by one identity, with client key
`pay-2026-10-21-0001`, acting for nobody else. The identities are `0x11…` (the initiator), the accounts `0x33…` and
`0x44…`, and the operation reference `0x22…`, each repeated for 16 bytes and shown as `did:` forms.

```python
import base64, hashlib, hmac, json, struct

ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"          # Crockford base32, upper case

def did(kind, body16):                                 # did:aledgent:<kind>:<26 characters>
    v = int.from_bytes(body16, "big")
    return f"did:aledgent:{kind}:" + "".join(ALPHABET[(v >> (5 * (25 - i))) & 31] for i in range(26))

def frame(*fields):                                    # each field: 4-byte big-endian length, then the bytes
    return b"".join(struct.pack(">I", len(f)) + f for f in fields)

sha = lambda b: hashlib.sha256(b).digest()

# The command as the core receives it.
client_key = b"pay-2026-10-21-0001"
opref      = bytes([0x22]) * 16                        # the operation reference ingress minted
operation  = did("op", opref)                          # the `operation` field of the core input
principal  = did("id", bytes([0x11]) * 16)             # the initiating identity
body = {"command": "transfer",
        "legs": [{"from": did("acct", bytes([0x33]) * 16), "to": did("acct", bytes([0x44]) * 16),
                  "asset": "USD", "amount": "2500"}]}
text = json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()

# 1. The command digest.
pre = frame(b"aledgent/approval/command", b"\x01", client_key, opref, operation.encode(),
            principal.encode(), b"\x00", text)         # b"\x00": no on-behalf-of
command_digest = sha(pre)

# 2. The approval challenge.
authn_key, nonce, expires_at = bytes(range(32)), bytes([0x55]) * 16, 1_800_000_120_000   # milliseconds
identity_digest = sha(principal.encode())
exp = struct.pack(">Q", expires_at)
m = hmac.new(authn_key, b"aledgent/approval-challenge/v1", hashlib.sha256)
for part in (nonce, exp, b"\x03", identity_digest, command_digest):
    m.update(struct.pack(">Q", len(part)) + part)      # the seal frames with 8-byte lengths
challenge = nonce + exp + b"\x03" + identity_digest + command_digest + m.digest()

# 3. The assertion digest (stand-in bytes: they pin the arithmetic; they are not a real assertion).
raw_id    = bytes(range(0x80, 0x90))
auth_data = sha(b"example.com") + b"\x05" + struct.pack(">I", 7)      # rpIdHash, flags UP+UV, count 7
b64 = base64.urlsafe_b64encode(challenge).rstrip(b"=").decode()
client_data = ('{"type":"webauthn.get","challenge":"%s","origin":"https://auth.example.com"}' % b64).encode()
signature = bytes([0xA5]) * 70
assertion_digest = sha(frame(b"aledgent/approval/assertion", b"\x01", raw_id, auth_data, client_data, signature))

# 4. The bytes ingress signs for the approval record.
credential = bytes([0x66]) * 16
record_message = frame(b"aledgent/approval/record", b"\x01", credential, command_digest,
                       assertion_digest, struct.pack(">I", 1))

for name, value in [("command text", text.decode()), ("preimage length", len(pre)),
                    ("command digest", command_digest.hex()),
                    ("identity digest", identity_digest.hex()), ("challenge", challenge.hex()),
                    ("assertion digest", assertion_digest.hex()), ("record message", record_message.hex())]:
    print(f"{name}: {value}")
```

Output, run with `python3 -I`:

```text
command text: {"command":"transfer","legs":[{"amount":"2500","asset":"USD","from":"did:aledgent:acct:1K6CSK6CSK6CSK6CSK6CSK6CSK","to":"did:aledgent:acct:248H248H248H248H248H248H24"}]}
preimage length: 347
command digest: 09f6b859498227fcfdb151604aa51ad6ecd98b771685030df1eda83014a2bc91
identity digest: 073c7ff1e2f11c45844b0a9df8966d57d7b4f4bcf5acdddb8655f35911445a57
challenge: 55555555555555555555555555555555000001a3185e24c003073c7ff1e2f11c45844b0a9df8966d57d7b4f4bcf5acdddb8655f35911445a5709f6b859498227fcfdb151604aa51ad6ecd98b771685030df1eda83014a2bc915b50b69349e9be01e7d34ecc4e530b9385e46b10911aaea57d59121a6cebd137
assertion digest: f48a07b4e2a97acba012ab26b18ac151b0da0c67ccafe1c9e41e121ea7aef7c2
record message: 00000018616c656467656e742f617070726f76616c2f7265636f7264000000010100000010666666666666666666666666666666660000002009f6b859498227fcfdb151604aa51ad6ecd98b771685030df1eda83014a2bc9100000020f48a07b4e2a97acba012ab26b18ac151b0da0c67ccafe1c9e41e121ea7aef7c20000000400000001
```

The 347 bytes of the preimage are the eight framed fields: 25 + 1 + 19 + 16 + 42 + 42 + 1 + 169 = 315 bytes of
content plus 32 of lengths. No signature value is shown: the record's signature is randomised, so it cannot be a
fixed vector; the signed message is. The assertion is not real, and its digest shows only the arithmetic. The
vector is a *drafting* check, computed once by the drafter's script: it pins nothing until `T-040-02` writes
it into a test and two implementations agree.

---

## How one command is approved (what Feature A does)

1. The client authenticates (session) and sends the command to a route that **takes nothing in** and asks for its
   approval challenge. Ingress reads the caller from the session, validates and normalises the command exactly
   as it would for submission (a shape fault is `ERR-MalformedCommand` here, as there), mints the operation
   reference, derives or reads the client key, and computes the command digest of F1 from *its own* copy.
2. If the command needs no approval, it says so and the client submits normally.
3. Otherwise ingress issues the approval challenge of F2 and returns it with the digest, so the client can show
   the person what is being approved.
4. The client has the device sign the challenge. The browser's list of allowed credentials is the initiating identity's active passkeys, any of them (a phone's passkey may approve while the person browses with another), and user verification is required.
5. The client submits the command with the assertion attached. Ingress recomputes the digest from the command in
   *this* request (it never trusts one the client sends), then checks, in this order and with one refusal for
   every failure: the shape; whether a first result already exists for the client key (a retry: answered, done);
   whether the command needs an approval; the challenge's seal, purpose, length and expiry; that its command
   digest is this command's and its identity digest is the initiator's; that the credential is an active passkey
   of the initiator's set (any of them); that the assertion verifies, as sign-in checks it, with user verification; that the
   challenge's number was never used; and the sign count rises.
6. Only then does ingress log the command and pass it on. Nothing before this point leaves a row.

A challenge whose digest or identity does not match is refused *before* its number is spent, so a misrouted
attempt does not burn the right person's challenge (as amendment 0027 does). Any other failure after the number is
spent leaves it spent; the client asks again.

**How the two steps meet the spec.** Step 1 is not a command: it takes nothing in, mints no row, changes no state.
`REQ-CS-2` and `REQ-CS-8` apply to the command from step 5. `NFR-1`'s fifty milliseconds at p99 are measured inside
aledgent from step 5, and now include work they did not: one assertion verification, and two durable writes (the
number's spending and the sign count's raising). That is a risk to the bound, not a change of it. A task of
`F-040` shows the bound holds with approvals on (`T-040-07`).

---

## Gaps this amendment names and does not decide

- **G2 — The approving passkey's own scope.** Any passkey of the identity may approve (ruling 9), so the passkey that
  approves can differ from the one the command is attributed to (`REQ-AU-7`). A credential may be limited to some of
  the identity's accounts (`REQ-AU-20`). The core's scope check covers the caller's credential. Whether the approving
  credential's scope must also cover the accounts the command touches is not ruled. Proposal: yes, a credential
  limited away from an account cannot approve what touches it. Q-C.
- **G3 — The device shows no amount.** See the trade-off of R1. Nothing in this amendment makes the device display
  what it approves.
- **G4 — Declaring "not enforced".** `REQ-AR-5` makes a deployment declare which *invariants* it enforces. An approval is not an
  invariant. R1's last point asks a deployment that does not require approvals to say so, and `REQ-AR-5` does not
  carry that. Feature A's rollout switch (off, report only, enforce) is how a deployment moves; the requirement binds
  a deployment that enforces. Proposal: extend `REQ-AR-5` by a sentence when this amendment is applied.
  Not decided here.
- **G5 — A credential added by the identities principal.** The identities principal is a service and exempt, and
  `REQ-AU-20` lets it add a credential to a managed identity's set. A person who wants the approval to cover that
  path needs the integrating party to stop using it. The spec does not say it should.
- **G6 — An identity with no signing credential.** An identity of kind person whose only credential is an API key
  is exempt by ruling 8, and so can still move value with no approval. The first draft's stricter deployment setting
  ("require approval for every human credential") has no meaning left for it, since an API key cannot approve, so
  this version drops it. Q-E.
- **G7 — The core input lacks the client key.** F1's third field is the client key; the core receives only the
  operation reference derived from it. Feature A needs nothing from the core. For B to recompute the digest, its
  input must carry the client key. That is a change to what the core receives, and part of `F-041`.
- **G9 — What a `key` credential's approval is.** A `key` credential signs, so ruling 8 puts it under the rule, and
  the core already verifies `key` proofs itself. F2 and F3 are written for a passkey (a challenge the device signs).
  A `key` approval would be a signature by that key over a message that holds the command digest of F1; its bytes
  are not fixed here. Proposal: the framed fields of the tag `aledgent/approval/key`, `0x01` and the command digest,
  signed with the credential's scheme, and, if the fork in Q-A chooses the second design, no ingress record is
  needed for it. Q-B.
- **G8 — Today the record of approvals is ingress's, if anyone's.** Between A and B, no approval is
  written anywhere durable unless ingress keeps a note. Ruled: it does (`T-040-09`).

## Choices the drafter made, for review to check

These are not rulings. Each is something the brief did not state and the drafter had to fix to write exact bytes.

1. **Canonical text is sorted-key JSON, not a binary encoding per command.** A binary encoding for each of about
   two dozen command kinds would need a table that grows with every new command and would leave a command with no
   entry unprotected. JSON text needs no table, covers every command at once and is reproducible in any language.
   The cost is on B: the core must not re-serialise to hash; the proposal in `F-041` is that ingress sends the
   canonical text and the core hashes exactly the bytes it was given, then parses them.
2. **The digest holds both the client key and the operation reference.** The key names *what the caller meant*; the
   reference is what the core sees. Holding both costs nothing and pins the pair (G7).
3. **The record holds the credential and digests only, not a time.** The book already stamps the command's input
   with the clock ingress gave the core.
4. **The approval's expiry is the deployment's existing challenge lifetime, 120 seconds.** Not a new setting.
5. **A bad approval is `ERR-CredentialInvalid`, with no detail**, like every failure of a sign-in, so the route is
   not an oracle (`REQ-AU-8`).

## Rulings applied (the owner's answers of 2026-10-21 to the first draft's ten questions)

| # | Question | Answer, as applied |
|---|---|---|
| 1 | The refusal's name | `ERR-CommandApprovalRequired` (R2) |
| 2 | When the core learns the refusal | With the next core, after the throughput measurement, not the next re-genesis (R2) |
| 3 | Which passkey approves | Any passkey of the identity, not only the one that signed in. The challenge binds the identity; the record names the credential that signed (F2, F3) |
| 4 | The six borderline commands | Only `release` needs approval (the table behind R1) |
| 5 | People without a passkey | API keys stay exempt. The rule applies only to schemes that sign: `key` and passkey (R1, G6, G9) |
| 6 | The hash | SHA-256. Reasons: WebAuthn hashes with it and browsers and hardware support it natively. Cost: the core links only BLAKE2b, so Feature B adds SHA-256 (F1) |
| 7 | User verification | Required on every approval (R1) |
| 8 | A note of who approved | Yes: ingress keeps the credential and the two digests beside the command, never the assertion (`T-040-09`) |
| 9 | Acceptable slowdown for Feature B | Throughput with approval records must not fall more than 1.5 standard deviations of the baseline's run-to-run variation below the baseline mean (`T-041-00`; see Q-F) |
| 10 | When Feature B ships | In the next core, as a core upgrade, not in the next re-genesis |

## Questions for the owner (still open)

Plain English. None of them blocks reading the amendment; Q-A and Q-D change what is built.

- **Q-A. How does the core learn that a person approved? (A design fork, not decided here.)** The owner ruled
  (`w14`) that Feature A goes first and that both ways below are measured, for security and throughput, before one is
  chosen, with the non-functional requirements preserved; the measurement design is in `F-041`'s plan. Two ways:
  1. *The core trusts a digest of ingress's approval.* The core keeps accepting direct proofs only from `key`
     credentials. Every other scheme, present or future, is approved by ingress, which is a principal of its own,
     and the core trusts a digest of that approval, as it already trusts the passkey check ingress did for
     `reclaim_custody`.
  2. *The core records an ingress-signed approval record for each command* (Feature B as drafted, F3).

  What each gives, plainly. In both, ingress vouches for the passkey, because the core does not verify the passkey
  (`DEC-19` rejects that). So neither detects a compromised ingress: a record signed by ingress is only as honest as
  ingress. The record of 2 adds evidence in the book of who approved what, and protects against a path that bypasses
  ingress (a command that reaches the core with no record is refused). Only the core verifying the assertion itself
  would protect against a compromised ingress, and that is the choice `DEC-19` rejected.
- **Q-B. What is a `key` credential's approval?** G9 proposes a signature over the command digest. Confirm, or say
  `key` credentials stay as they are today.
- **Q-C. Must the approving passkey's own scope cover the accounts the command touches?** G2 proposes yes.
- **Q-D. Is this the right way to keep the closed list true until the next core?** R2: the line enters §10 when the
  amendment is applied, ingress holds the refusal itself, and the two-way test gets one named exception that goes
  away with the next core.
- **Q-E. Is dropping the "require approval for every human credential" setting right?** G6: with API keys exempt it
  would change nothing, so the draft drops it.
- **Q-F. Confirm the threshold before the run.** 1.5 standard deviations is close to "no measurable slowdown". See
  `T-041-00` and the plan of `F-041`.

## What does not change

`REQ-AU-1` to `REQ-AU-22` and every acceptance criterion of `US-A1` to `US-A9`, apart from the new clause on adding
a device-bound credential. The session and its fifteen minutes. The core's own check, at decision, that a
credential is an active member of its identity's set and covers the account (`REQ-AU-3`, `REQ-AU-20`): ingress's
approval is in addition to it, not instead. Idempotency (`REQ-OP-10`, `REQ-OP-11`). Amendment 0027's registration
challenge, unchanged.

## Plan

On approval, `PLAN.md` gets: a new decision (`DEC-28`), recording F1 to F3, the order of checks and the delivery of
A through ingress; `DEC-19` gains one sentence ("A passkey assertion buys a session for sign-in and reads; a write
needs an approval, `DEC-28`"); and two rows in §4, `F-040` and `F-041`. Those edits wait for the approval, so they do
not collide with the open pull requests that edit `PLAN.md`. Until then the features cite `[PENDING AMENDMENT 0037]`.

`F-039` is already taken, by the documentation feature of `PLAN.md` §4 and `M-3D`, and `F-038`'s tasks cite it.
The ingress feature is therefore **`F-040`** and the core feature **`F-041`**. The handover that drafted them called them
`F-039` and `F-040` provisionally, "until the feature gets its number".

- `features/F-040-passkey-approval-ingress/` is Feature A: `plan.md`, `tasks.md`.
- `features/F-041-passkey-approval-core/` is Feature B, a stub: blocked by A and by the measurement of `T-041-00`.
