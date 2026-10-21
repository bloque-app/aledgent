<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-040 — Per-transaction approval at ingress (Feature A)

`[PENDING AMENDMENT 0037]`. The amendment is awaiting the owner's approval. Until it is approved, this feature
is a plan only: no task below starts, and no code merges.

| | |
|---|---|
| Milestone | Not assigned by `PLAN.md`; the owner's rulings of 2026-10-19 to 2026-10-21 order it before the final regeneration if the approval lands in time (amendment 0037, Question 2) |
| Home | `services/ingress` (`credentials`, `command`, `auth`, `config`, `surface`, `wire`, `store`, `openapi`, a new migration if `T-040-09` is approved), `crates/authn` (`token`: the approval challenge), `site/` (the integrators' page), `e2e/` (the end-to-end flow). **Not** the core: no core code changes in this feature, with one exception that amendment 0037 names (R2): the refusal's line in `ledger-core`'s list, `T-040-10`, decided by Question 2 |
| Depends on | `F-028` (passkeys, challenges, durable sign counts and single-use challenge numbers: `T-028-04`), `F-038` (ingress as a fleet: any instance must refuse a spent approval), amendment 0037 |
| Unblocks | `F-041` (the core records the approval), `M-3D`'s page for integrators |
| Realises | `REQ-AU-23` and `US-A10` `[PENDING AMENDMENT 0037]`; `REQ-AU-1`, `REQ-AU-20`, `REQ-AU-21`, `REQ-CS-2`, `REQ-CS-8`, `REQ-OP-10`, `NFR-1`, `NFR-7` |

## Goal

A stolen session token can no longer move value, change who may act on an account, or add a credential. Each
command that matters is approved by a person at their own device, once, and ingress refuses it without that
approval before it logs anything. The core is told nothing yet (that is `F-041`).

## Spec traces

Existing requirements this feature must keep true, and the proposed ones it adds. Proposed ones are marked.

- `REQ-AU-23` `[PENDING AMENDMENT 0037]`: an approval is a proof at a person's device over one command, accepted
  once, everywhere; a command that needs one and has none is refused with nothing taken in; a bad approval is
  `ERR-CredentialInvalid`; a retry needs none.
- `US-A10`, `AC-A10.1` to `AC-A10.10` `[PENDING AMENDMENT 0037]`: the acceptance criteria the tests name.
- `REQ-AU-20`, `REQ-AU-21`: credentials are a set; every call enters through ingress. The approval is in addition
  to the core's own check that the credential is an active member of its identity's set and covers the account
  (`REQ-AU-3`), never instead of it.
- `REQ-CS-2`, `REQ-CS-8`, `NFR-1`: the approval step takes nothing in; the fifty milliseconds run from the approved
  submission and now include an assertion check and two durable writes.
- `REQ-OP-10`, `REQ-OP-11`: the client key and the operation reference, both in the digest; a retry is answered
  from the first result before any approval is asked for.
- `REQ-ER-1`, `REQ-ER-3`, `REQ-CS-4`: the refusal list is closed; a proof that does not verify is
  `ERR-CredentialInvalid`; a refusal's runtime shape is tested against its published description.
- `NFR-7`: every route is default-deny; the approval route is authenticated by the session, not open.
- `REQ-AU-8`: no assertion, challenge or credential reaches a log, a URL or an error.

## Design

Amendment 0037 holds the rules and the three byte formats; this section orders the build. In build order:

1. **The rules, applied.** The amendment's text goes into `SPEC.md` and `PLAN.md` (`DEC-28`, `DEC-19`'s sentence,
   two feature rows) in one change, with the owner's choices to Questions 1 to 8 written in. `T-040-01`.
2. **The formats, frozen.** The command digest (F1), the approval challenge (F2) and the signed message of the
   approval record (F3) written as code and pinned by a test whose values the amendment's script computed and a
   second, independent implementation reproduces. After this task ships, B cannot change them. `T-040-02`.
3. **One-time use, durable.** Ingress already keeps spent challenge numbers in a table shared by every instance
   (`used_challenges`, `use_challenge`) and drops the expired ones when one is used. The approval uses the same
   store, with the same guarantee, and the task shows it for approvals: on both stores, from any instance and after
   a restart. It adds only what is missing. `T-040-03`.
4. **The policy.** Which commands need an approval, which callers are exempt and what a deployment may add. Every
   command is classified on purpose; one nobody classified is refused at build time, not exempt at run time.
   `T-040-04`.
5. **The two steps.** An authenticated route that takes nothing in and returns the challenge and the digest; and the
   command route accepting the approval. The checks run in the order amendment 0037 gives and every failure of
   the proof is one `ERR-CredentialInvalid`. `T-040-05`.
6. **The refusals tested alone.** Each way an approval can be wrong, each broken on its own. `T-040-06`.
7. **The rollout, and the latency.** A switch with three positions and the check that the fifty milliseconds
   still hold with approvals on. `T-040-07`.
8. **The page for integrators.** `T-040-08`.
9. **Two tasks that wait for the owner.** The ingress's note of who approved (`T-040-09`, Question 8) and the
   refusal's line in the core's list (`T-040-10`, Question 2).

### The rules, in short (the amendment has them whole)

- **Scope.** Every command that moves or reserves value, changes who may act, or changes a credential. A
  deployment may add commands, never remove one.
- **Callers.** A caller that signed in with a passkey needs an approval. An API key, a service token and the book's
  own inputs do not. A deployment may also require approval for every command of an identity of kind person.
- **Sessions** stay for sign-in and reads. Every write, and adding a passkey, asks for an approval.
- **A retry** of an approved, taken-in command under the same client key is answered from the first result and
  asks for no approval.
- **Which passkey.** `[PENDING AMENDMENT 0037, Question 3]` Proposal: the one the session was made with.

### What ingress keeps, and does not

- The received log keeps the command as before. It never keeps an assertion, an attestation or a secret
  (`REQ-AU-8`).
- Spent challenge numbers are kept until their expiry in the table `F-028` made.
- If the owner says yes to Question 8, one more column or table keeps the approving credential and the two
  digests beside the command (`T-040-09`), so that before `F-041` there is a trace of who approved.

## Trade-offs, stated plainly

- **Two calls per write, and a person at a device.** A client that submits many commands for one person, such as
  a payroll run, cannot do it on one session any more. A service principal's key is the route for that, and it
  is exempt by design.
- **A tap is not an understanding.** The device does not show the amount. A compromised application can ask for
  the wrong command. The approval stops a stolen session and a program acting alone; it does not stop malicious
  code in the holder's own browser.
- **Ingress is trusted until `F-041`.** The core does not check the approval. A compromised ingress can log what
  it likes.
- **Work added to a bounded path.** An assertion check and two durable writes now sit inside `NFR-1`'s fifty
  milliseconds for a transfer, a reservation and a withdrawal. `T-040-07` shows the bound still holds.
- **An unclassified command must not be exempt by default.** The cost is that adding a command also means classifying
  it, on purpose, in the same change.

## Out of scope

- **Anything in the core.** Recording the approval, the trusted-key set and the core's own check are `F-041`. The
  one exception is the refusal's line (`T-040-10`), which is a change to the core's *library*, not to what the
  core does.
- **The client SDK.** It lives outside this repository. This feature specifies the two calls it will wrap and runs
  a reference client in `e2e/`; changing the SDK is its owners' work, and the integrators' page names it as
  pending.
- **Showing the person what they approve.** The digest and the command are returned so a client can show them;
  nothing here makes a device display them.
- **Making a person without a passkey hold one.** Question 5 and the amendment's G6.
- **Approval by a `key` credential or an issued secret.** Only a device-bound credential approves.
- **Changes to the registration ceremony** of amendment 0027, which still runs in addition to the approval.
