<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Recovery authority — runbook

Owner: the recovery authority's team, which is **not** the team that operates the core (`DEC-13`).
Traces: `REQ-AU-9`, `REQ-AU-15`–`REQ-AU-19`, `US-A8`, `RISK-9`, `F-012`.

This runbook is the out-of-band verification of `DEC-13`. It is deliberately not code: the service in
this directory does one thing, sign an approval of one pending recovery, and it does that only as the
last step below. Nothing in the core can check that these steps were followed. That is why the team
that follows them must be separate from the team that runs the core, and why this document exists.

## What a recovery is

A holder of an **owned-custody** account who has lost every working credential asks to get back in.

1. The holder asks the operator, out of band. The operator records the request with
   `initiate_recovery` on the operator surface: the account, the verifier (public key) of the new
   credential, its scope, and a justification. The core records a **pending recovery**, effective
   after the deployment's delay (72 hours by default, `DEC-13`). Every authority holder of the account
   is notified, and the pending recovery shows on the account.
2. During the delay, any working credential of the holder — including a holder-held recovery
   credential — can cancel it with `cancel_recovery`. Using a credential for anything else does
   **not** cancel it (amendment 0006 item 10, pending).
3. The recovery authority verifies the holder by this runbook and, if satisfied, signs an approval of
   exactly that pending recovery.
4. After the delay, the operator submits `complete_recovery` with the approval. The core checks the
   signature against the recovery key's public half, and the delay. The new credential joins the
   holder's set; every credential that covered the account stops working; the audit record names the
   initiator, the recovery authority and the submitter.

The operator cannot complete a recovery without the approval. The recovery authority cannot start,
complete or cancel anything, cannot act on an account, and cannot move value: it holds no identity in
the book (`REQ-AU-15`, `RISK-9`).

## The key

- The recovery key is generated on the recovery authority's own hardware, by the recovery authority's
  team. Its secret never leaves that environment. It never exists on a host of the core, in the core's
  repositories, backups, logs, tickets or chat (`NFR-8`, `REQ-AU-8`).
- The service reads the key from a file the deployment provides (`SigningKey::from_file`). There is no
  default key. If the file is missing or malformed, the service does not start.
- The core's deployment configuration carries only the **public** half (`RecoveryConfig::recovery_key`).
  The core refuses to start if that key is one of the operator's keys, or if owned custody is offered
  without one (`F-012` §10).
- Rotation: generate a new key, have the core's configuration updated through its reviewed-change
  process, and destroy the old secret. Rotation touches no account (`DEC-13`). An approval signed with
  the old key stops being accepted when the configuration changes; any recovery pending at that moment
  needs a new approval.
- Suspected compromise: tell the core's owner at once and rotate. A compromised recovery key alone
  cannot complete a recovery (the operator submits it, after the delay, visibly to the holder), but it
  removes the separation this service exists for.

## Verifying a holder

Two members of the recovery authority's team take part in every case. Neither may be a member of the
core's operating team. Keep a case record for each request; it holds no credential and no secret.

1. **Receive the case.** The request reaches the recovery authority through its own channel, not
   through the operator's ticketing. Record: case number, date, the account identifier, the pending
   recovery's identifier (the operation reference of `initiate_recovery`), and the verifier the
   recovery will issue.
2. **Confirm the pending recovery exists and matches.** Ask the operator for the pending recovery as
   the core records it: account, owner identity, verifier, scope, `effective_at`. Every field must match
   the case. A mismatch ends the case: do not sign.
3. **Contact the holder independently.** Use contact details the recovery authority already holds for
   the holder, or that the holder registered before the loss — never details supplied in the request
   or by the operator for this case. Confirm the holder asked for the recovery.
4. **Verify identity.** Apply the deployment's identity-verification standard (documents, a live check,
   or both) and record what was checked, not the documents themselves.
5. **Confirm the new credential is the holder's.** The holder states, over the independent channel, the
   public key (or its fingerprint) of the new credential. It must equal the verifier in the pending
   recovery. If the holder does not recognise it, the request is not theirs: refuse, and tell the core's
   owner that a recovery was initiated without the holder.
6. **Check for a cancellation.** If the holder cancelled during the delay, or the pending recovery is no
   longer pending, stop.
7. **Second person.** The second team member reviews steps 1–6 in the case record and agrees.
8. **Sign.** Only now run `approve_recovery` with the pending recovery, the account and the verifier
   from step 2. Send the approval to the operator. The approval is useless for any other recovery,
   account or credential, and is not a secret, but send it only to the operator.
9. **Close the case** with the outcome. Keep the record for the deployment's audit retention period.

Refuse, and record why, whenever a step fails or is in doubt. Refusing costs the holder time; signing
wrongly hands their account to someone else.

## What the recovery authority never does

- Sign before the holder is verified, or sign a request whose fields differ from the pending recovery.
- Accept the operator's word, contact details or documents in place of its own verification.
- Configure guardians, thresholds or shared approvals among holders: guardian recovery is not offered
  (`REQ-AU-19`), and the configuration has no field for it.
- Hold, request or see a holder's credential secret, or the core's keys.
