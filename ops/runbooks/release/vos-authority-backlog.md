<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: unacknowledged Authority decisions near VOS's limit

`REQ-RT-1`, `REQ-RT-2`, `INV-18`, `NFR-11`, `REQ-AT-3` · `F-005`, `T-005-08` · **Pages:**
`AledgentVosAuthorityBacklog` (more than 191, three quarters of 256) · **Warns:**
`AledgentVosAuthorityUnacknowledged` (any, for ten minutes) · **Owner:** platform.

**What it means.** The core host is the Agent's Local host:

- it signs an **Authority receipt** (a decision) for every invocation it makes;
- it **acknowledges** the invocation at once, in a second execution, and the runtime then retires its
  result.

An invocation whose acknowledgement fails keeps its result retained. VOS bounds what it retains:

- its Authority issuer journal refuses a new decision once it holds **256** without evidence of their
  consumption (`MAX_AUTHORITY_OPERATION_ISSUER_RECORDS`, `JournalFull`, pinned VOS `e2d59d17`);
- a Local lane retains at most **32** unacknowledged results (`MAX_INVOCATION_RESULTS_PER_LANE`).

So on the Local host the warning (any decision unacknowledged for ten minutes) comes well before the page.
Past either limit VOS refuses further invocations, and the core halts on the next input
([core-halt](core-halt.md)). `aledgent_vos_unacknowledged_authority_decisions` counts the host's
unacknowledged decisions since it started. It is normally 0.

## Preconditions

- [ ] Read the core's standard error for `acknowledgement refused` (`DriverError::Acknowledgement`) and
  the runtime outcome it names.
- [ ] Check that the core still decides (`aledgent_core_halted == 0`, decisions rising in `core-diag`).

## Steps

1. **Capture the evidence:** the refusal, the input it belonged to, the runtime and actor pins the
   release records.
2. **Do not restart in a loop.** A restart resets the counter, not the runtime's retained results: they
   are in the Agent's state (`agent.state`).
3. **Reproduce off production** from a snapshot of the core volume, as for a halt
   ([core-halt](core-halt.md) step 2). The usual causes are a runtime limit (state image, retained
   bytes per lane) or an acknowledgement under another identity than the invocation's.
4. **Fix forward** with a build whose host acknowledges correctly, through the release gate. Deploy by
   digest. The runtime then retires what was retained on the next acknowledgements.

## Verification

- [ ] `aledgent_vos_unacknowledged_authority_decisions` returns to 0 and stays there.
- [ ] No input is lost or doubled (`INV-18`): the archive and the replies agree, and reconciliation axes 1
  and 2 pass on demand.

## Rollback

Redeploy the previous digest. Never edit `agent.state`, or any store outside the command surface
(`REQ-AT-3`).

## Exercised

On every change: `ops/tests/alerts.rs` checks that `AledgentVosAuthorityBacklog` and
`AledgentVosAuthorityUnacknowledged` page the test route. A refused acknowledgement is not exercised
automatically yet; it is exercised by hand in a staging deployment.
