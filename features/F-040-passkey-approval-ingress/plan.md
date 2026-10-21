<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-040 — Per-transaction approval at ingress (Feature A)

`[PENDING AMENDMENT 0037]`. The amendment is awaiting the owner's approval. Until it is approved, this feature
is a plan only: no task below starts, and no code merges.

| | |
|---|---|
| Milestone | Not assigned by `PLAN.md`. Ships first, before `F-041` (owner's ruling). The core's side of its refusal ships with the next core, not with this feature (amendment 0037, R2) |
| Home | `services/ingress` (`credentials`, `command`, `auth`, `config`, `surface`, `wire`, `store`, `openapi`, a new migration for `T-040-09`), `crates/authn` (`token`: the approval challenge and the key approval's message), `site/` (the integrators' page), `e2e/` (the end-to-end flow). **Not** the core: no core code changes in this feature, with one exception that amendment 0037 names (R2): the refusal's line in `ledger-core`'s list, which ships with the next core (`T-040-10`) |
| Depends on | `F-028` (passkeys, challenges, durable sign counts and single-use challenge numbers: `T-028-04`), `F-038` (ingress as a fleet: any instance must refuse a spent approval), amendment 0037 |
| Unblocks | `F-041` (the core records the approval), `M-3D`'s page for integrators |
| Realises | `REQ-AU-23` and `US-A10` `[PENDING AMENDMENT 0037]`; `REQ-AU-1`, `REQ-AU-20`, `REQ-AU-21`, `REQ-CS-2`, `REQ-CS-8`, `REQ-OP-10`, `NFR-1`, `NFR-7` |

## Goal

A stolen session token can no longer move value, change who may act on an account, or add a credential. Each
command that matters is approved with one of the caller's own credentials (a device for a passkey, a signature for
a key), once, and ingress refuses it without that approval before it logs anything. The core is told nothing yet (that is `F-041`).

## Spec traces

Existing requirements this feature must keep true, and the proposed ones it adds. Proposed ones are marked.

- `REQ-AU-23` `[PENDING AMENDMENT 0037]`: an approval is a proof by a credential whose scheme signs, over one command,
  accepted once, everywhere; a command that needs one and has none is refused with nothing taken in; a bad approval is
  `ERR-CredentialInvalid`; a retry needs none.
- `US-A10`, `AC-A10.1` to `AC-A10.16` `[PENDING AMENDMENT 0037]`: the acceptance criteria the tests name.
- `REQ-AU-20`, `REQ-AU-21`: credentials are a set; every call enters through ingress. The approval is in addition
  to the core's own check that the credential is an active member of its identity's set and covers the account
  (`REQ-AU-3`), never instead of it.
- `REQ-CS-2`, `REQ-CS-8`, `NFR-1`: the approval step takes nothing in; the fifty milliseconds run from the approved
  submission and now include an assertion check and two durable writes.
- `REQ-OP-10`, `REQ-OP-11`: the client key is in the digest and the operation reference is not (ingress mints it at logging and cannot reproduce it); a retry is answered
  from the first result before any approval is asked for.
- `REQ-ER-1`, `REQ-ER-3`, `REQ-CS-4`: the refusal list is closed; a proof that does not verify is
  `ERR-CredentialInvalid`; a refusal's runtime shape is tested against its published description.
- `NFR-7`: every route is default-deny; the approval route is authenticated by the session, not open.
- `REQ-AU-8`: no assertion, challenge or credential reaches a log, a URL or an error.

## Design

Amendment 0037 holds the rules and the byte formats; this section orders the build. In build order:

1. **The rules, applied.** The amendment's text goes into `SPEC.md` and `PLAN.md` (`DEC-28`, `DEC-19`'s sentence,
   two feature rows) in one change, with the owner's rulings and answers written in. `T-040-01`.
2. **The formats, frozen.** The command digest (F1, with the deployment, the client key and the payload digest and
   without the operation reference), the approval challenge (F2, sealed with the deployment), the signed message of
   the approval record (F3) and a key's approval message (F4), pinned by a vector (F5) whose values the amendment's
   script computed and a second, independent implementation reproduces. After this task ships, B cannot change F1 to
   F3. `T-040-02`.
3. **One-time use, durable.** Ingress already keeps spent challenge numbers in a table shared by every instance
   (`used_challenges`, `use_challenge`). The approval uses the same store and adds what is missing: the margin before a
   spent number is forgotten, on the database's clock. `T-040-03`.
4. **The policy.** Every command route and core input classified on purpose, by an exhaustive match, with the default
   list of the amendment's table. `T-040-04`.
5. **The door.** How the caller proved who it is decides whether an approval is needed: a session, yes; an issued
   secret or a configured token (a bearer secret presented as it is), no. `T-040-11`.
6. **The two steps for a device-bound credential**, with the device's own check forced and the current credential list read.
   `T-040-05`. **A key's approval**, for callers whose credential is a key. `T-040-12`. **The approving credential's
   scope**, when the owner has ruled where it is checked. `T-040-13`.
7. **The refusals tested alone.** Each way an approval can be wrong, each broken on its own. `T-040-06`.
8. **The rollout, and the latency.** A switch with three positions and the check that the fifty milliseconds still
   hold with approvals on. `T-040-07`.
9. **The page for integrators.** `T-040-08`.
10. **Two more tasks.** Ingress's note of who approved (`T-040-09`) and the refusal, held at ingress until the next core
    carries it (`T-040-10`).

### The rules, in short (the amendment has them whole)

- **Scope.** The commands in the amendment's table. A deployment may add commands, never remove one.
- **Callers.** Decided by how the caller proved who it is: a session (or a signature by a key) needs an approval; an
  issued secret or a configured token, presented as it is, does not (ruling 4). Never by the kind of principal or the
  route: a session of a service principal needs one. Only credentials of a scheme that signs can approve.
- **Sessions** stay for sign-in and reads. Every write, and adding a credential, asks for an approval.
- **A retry** of an approved, taken-in command under the same client key is answered from the first result and asks
  for no approval.
- **Which credential.** Any active credential of the initiating principal's set whose scheme signs, not only the one
  that signed in; for an upgrade, the credential the mode names. The device's own check is required on every
  device-bound approval. The set is read live; if it cannot be read the command is refused with
  `ERR-DecisionUnavailable`.
- **The refusal** is `ERR-CommandApprovalRequired`. A bad approval is `ERR-CredentialInvalid`.
- **The hash** is SHA-256, which WebAuthn itself uses and browsers and hardware support natively.

### What ingress keeps, and does not

- The received log keeps the command as before. It never keeps an assertion, an attestation or a secret
  (`REQ-AU-8`).
- Spent challenge numbers are kept until their expiry plus a margin, in the table `F-028` made.
- One more column or table keeps the approving credential and the two digests beside the command (`T-040-09`, ruled
  yes), so that before `F-041` there is a trace of who approved.

## Trade-offs, stated plainly

- **Two calls per write, and a credential asked each time.** A client that submits many commands for one principal,
  such as a payroll run, cannot do it on one session any more. A service principal's issued secret is the route for
  that, and it is exempt by design (and Q-I of the amendment asks whether it should stay so for credential changes).
- **A withdrawal is two approvals** (`withdraw`, then `submit_outbound`).
- **A tap is not an understanding.** The device does not show the amount. A compromised application can ask for the
  wrong command. The approval stops a stolen session and a program acting alone; it does not stop malicious code
  in the holder's own browser.
- **A key's approval is not a person.** It proves the key signed.
- **Ingress is trusted until `F-041`.** The core does not check the approval. A compromised ingress can log what it
  likes.
- **Work added to a bounded path.** An assertion check, a read of the current credential list and two durable writes now sit inside
  `NFR-1`'s fifty milliseconds for a transfer, a reservation and a withdrawal. `T-040-07` shows the bound still holds.
- **Availability.** While no core can be read, approved writes are refused (`ERR-DecisionUnavailable`), where `F-038`
  would have acknowledged them. Amendment Q-H asks the owner to accept it.
- **An unclassified command must not be exempt by default.** The cost is that adding a command also means classifying
  it, on purpose, in the same change.

## Out of scope

- **Anything in the core.** Recording the approval, the trusted-key set and the core's own check are `F-041`. The
  one exception is the refusal's line (`T-040-10`), which is a change to the core's *library*, not to what the
  core does, and ships with the next core.
- **The client SDK.** It lives outside this repository. This feature specifies the two calls it will wrap and runs
  a reference client in `e2e/`; changing the SDK is its owners' work, and the integrators' page names it as
  pending.
- **Showing the person what they approve.** The digest and the command are returned so a client can show them;
  nothing here makes a device display them.
- **Making a principal that holds only an issued secret hold a signing credential.** Exempt by ruling; Q-I.
- **Approval by an issued secret.** It signs nothing and cannot approve.
- **The four core inputs with no ingress route** (`SetAuthority`, `ProposeUpgrade`, `ApproveUpgrade`,
  `ProposeMigration`).
- **Changes to the registration ceremony** of amendment 0027, which still runs in addition to the approval.
