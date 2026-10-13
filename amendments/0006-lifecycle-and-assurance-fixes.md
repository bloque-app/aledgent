<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0006 — lifecycle and assurance fixes found while planning features

**Status:** ruled by the owner on 2026-09-29; applied to `SPEC.md` v0.4.0
**Target:** `SPEC.md` v0.3.3
**Date:** 2026-09-22 (proposed), 2026-09-29 (ruled and applied)
**Rationale in one line:** writing the feature plans found ten places, and building `F-007` an eleventh, where the spec contradicts itself or
is silent where code must decide (the plan's review, issues #25 and #53).

The owner accepted every recommended ruling, with the clarifications recorded under each item. Each item
keeps its problem and its recommendation as proposed; the ruling follows.

---

## Item 1 and 4 — Reversal is its own operation

**Problem.** The transition table allows `Settled → Reversed` and `Needs-Review → Reversed`, but `Settled`
is terminal and `INV-21` says terminal states never change. `Needs-Review → Reversed` also has no settled
posting to reverse.

**Recommended ruling.** A reversal is a **separate operation** of kind *reversal*. It references the
posting it reverses, needs two approvers (`REQ-RC-2`, `REQ-OS-3`), and when it settles it appends one
`Reversal` posting (`REQ-PO-3`: at most one per posting). The original operation stays `Settled`. The state
`Reversed` and both edges into it are withdrawn. `Needs-Review` resolves only to `Settled` or `Released`;
if value must then be returned, that is a reversal operation. `INV-21` holds with no exception.

**Ruling (2026-09-29): accepted.** The owner adds what "two approvers" means, and refined it twice the
same day. The second refinement replaces the first where they differ. Approval is **authority-based and
check-based**.

- **Reversals: maker-checker and a mirror check.** One principal proposes and a **different** principal
  approves (`REQ-OS-3`). Two checks made by one principal are not two approvers. **Each principal holds
  its own reversal authority** on the operator surface (`REQ-OS-1`): one to propose, one to approve.
  Neither authority can stand in for the other principal. The reversal must also pass a mechanical check
  that neither approver can override (new `REQ-RC-5`): its posting mirrors the original exactly, whole
  and not partial (`REQ-PO-3`). Where the reversed operation has an external leg, that leg is subject to
  the proof rule below. New acceptance criterion `AC-K2.3`.
- **`Needs-Review`: up to three steps** (new `REQ-RC-5`). Humans decide without mechanical proof only in
  the last one.
  1. **The adapter decides on its own** whenever it holds proof, with no human. A finalised inbox entry
     for the operation's reference captures it (`Settled`). A finalised refusal (a proved rejection,
     item 3), or non-execution proven with the transaction's nonce unconsumed, releases it. New `AC-K1.4`.
  2. **One authority submits evidence** when the adapter could not prove the outcome on its own, for
     example because it missed blocks. The evidence is a block number and a transaction hash, submitted
     through a new operator command, `submit_evidence`. The adapter fetches that transaction and
     verifies it against the chain on its own, then applies the outcome the proof supports. One
     authority is enough, because the proof decides. New `AC-K1.5`.
  3. **Two principals, only without proof.** If the adapter still cannot decide after checking the
     evidence, a second, **different** authority becomes mandatory (maker-checker): two distinct
     principals must agree, and an incident is raised. This is the only path where humans decide without
     mechanical proof. New `AC-K1.6`.

  One operator can no longer resolve an external leg without its proof: that is `AC-K1.3`, and it is
  refused with an error, leaving the operation in `Needs-Review`.

  The owner's intent: a review that goes to release or to capture should be easy to check through the
  host extension's implementation. `SPEC.md` says "the settlement adapter"; `PLAN.md` binds the
  evidence check to `F-027`'s adapter over the Kreivo extension. In the core, the check sits behind a
  trait (`EvidenceFetch`), because that adapter piece does not exist yet.

  **The nonce, confirmed by the owner (2026-09-29).** A proved rejection: the transaction was
  included, so the signing key's nonce is consumed, and the contract recorded a refusal. Proven
  non-execution: the nonce is still unconsumed after the transaction's validity window ended. That is
  how `Ledger::proved_rejection` and the non-inclusion proof already read it.

  **Step 3 and `INV-10`, ruled (`OQ-62`, 2026-09-29).** Step 3 as first ruled let two principals
  resolve to either outcome. For an external leg, `Settled` would write the settling posting against the
  issuance counterparty with no proof that the leg is final, which `INV-10` forbids. It was raised as
  `OQ-62`, and the owner ruled: **without proof, two principals may only release, never capture.**
  Capturing would book an entry before the external leg is final, which breaks `INV-10`. A settlement
  without proof is refused with `ERR-EvidenceInsufficient`. Under `INV-11`, the evidence step 3's release
  is derived from is the adapter's finding of no proof, both principals' justifications and the incident.
  `OQ-62` is closed.

A reversal operation is proposed into `Accepted`, with a deadline. The approval settles it
(`Accepted → Settled`) with its one `Reversal` posting. Unapproved past its deadline, it goes to
`Needs-Review` like any other operation (item 5), where an operator resolves it to `Released`: nothing was
posted, so nothing returns.

## Item 2 — What V0's assurance level may declare

**Problem.** `REQ-AR-6` declares `INV-15`, `INV-16` and `INV-22` service-enforced in V0. §9 names Store +
service, Store and Credential for them, and `REQ-AR-4` forbids a weaker layer without an amendment.

**Recommended ruling.** §9 keeps its column as the **target** layer. A new column, **V0**, shows where the
first release enforces each invariant. For those three it says *service*. This amendment is the
amendment `REQ-AR-4` requires, so the downgrade is recorded, not silent. `REQ-AR-6` points at the V0
column.

**Ruling (2026-09-29): accepted.**

## Item 3 — Release after a final failure

**Problem.** `REQ-OP-9` allows release after a final failure observation, but the table has no edge.

**Recommended ruling.** Add `Submitted → Released` and `Observed → Released`, when a **final** observation
shows the external leg failed and cannot take effect. The release posting is the usual `Release`.

**Ruling (2026-09-29): accepted.** The owner adds: **a proved rejection is an immediate release.**

- A **proved rejection** is a refusal that the settlement layer recorded against the instruction, observed
  at finality, and confirmed by the signing key's nonce: the transaction that carried the instruction was
  included, and it was refused. In the plan's terms, a refusal in the settlement contract's inbox at
  finality, with the key's nonce consumed. Examples: the withdrawal is over its bound, the deployment's
  position is insufficient, or the settlement layer refuses the transfer.
- It releases the reservation **at once, with no review.** It is the "provably not executed" disposition of
  `REQ-SA-4`, and the proof `REQ-OP-9` asks for.
- Until this ruling the implementation sent such a refusal to `Needs-Review` by its deadline, only because
  §5.6 had no edge for it.

Applied as: the new rows `Submitted → Released` and `Observed → Released`, and the "When" cells of the
existing rows `Reserved → Released`, `Expired → Released` and `Orphaned → Released` name a proved
rejection too, so that it releases from every state it can arrive in.

## Item 5 — Deadlines on every non-terminal state

**Problem.** `Received` has no deadline in the table, though `INV-15` needs one. `Accepted`, `Reserved`,
`Final` and `Orphaned` have no operator path once past their deadline.

**Recommended ruling.** One rule for all: **every non-terminal state carries a deadline; an operation past
its deadline moves to `Needs-Review`**, where `resolve_operation` applies (`REQ-RC-1`). The bound in
`REQ-CS-7` is `Received`'s deadline.

**Ruling (2026-09-29): accepted, with a correction.** Every **non-terminal** state gets a deadline, and
past it an operator path to `Needs-Review`. **Terminal states get no deadline.**

Which states are terminal: `REQ-OP-4` names `Settled`, `Released`, `Rejected` and (before this amendment)
`Reversed`. **Neither `Final` nor `Orphaned` is terminal**: each has edges out of it (`Final → Settled`;
`Orphaned → Submitted`, `Orphaned → Released`). So both get a deadline under the rule.

The states that got a deadline edge to `Needs-Review` from this item:

| State | Before | After |
|---|---|---|
| `Received` | No deadline; past the `REQ-CS-7` bound an incident | Deadline = the `REQ-CS-7` bound; past it, `Needs-Review` (and still an incident) |
| `Accepted` | Deadline, no exit | `Accepted → Needs-Review` |
| `Reserved` | Deadline, no exit | `Reserved → Needs-Review` |
| `Final` | Deadline, no exit | `Final → Needs-Review` |
| `Orphaned` | Deadline, no exit | `Orphaned → Needs-Review` |
| `Expired` | Exit only when non-inclusion cannot be proven | Also past its deadline (same pair, `Expired → Needs-Review`) |
| `Submitted`, `Observed` | Already had the edge | Unchanged |

`Needs-Review` is non-terminal and keeps its deadline: the review service level of `REQ-OP-6`. Past it
the operation stays in `Needs-Review` and the queue escalates it; there is nowhere further to move it.

## Item 6 — A freeze is not a balance kind

**Problem.** The glossary lists a freeze under *encumbered*, but a frozen account's value stays in
`available`.

**Recommended ruling.** A freeze is an **account status**. It blocks debits and lets credits land
(`INV-9`); the value stays `available` and is simply not spendable while frozen. *Encumbered* keeps its
meaning for standing rules on an amount (`DEF-11`), and the glossary drops "a freeze" from its examples.

**Ruling (2026-09-29): accepted.** The owner wants both concepts to exist:

- a **freeze** is an account status: debits are refused, credits are accepted, balances do not change
  (new `REQ-LA-7`);
- **encumbered** remains a balance kind, for standing rules on a specific amount (`US-C5`, `DEF-11`,
  `F-024`).

## Item 7 — Posting and operation kinds without a command

**Problem.** The `Adjustment` posting kind has no command. Custody changes and opening loads have no
operation kind.

**Recommended ruling.** Withdraw `Adjustment`: a correction is a reversal plus a new operation (`US-K4`).
Add operation kinds *custody change*, *credential change*, *recovery* and *opening load* to the glossary's
list, so every command in §8.1 names its kind.

**Ruling (2026-09-29): accepted.** `Adjustment` is withdrawn. The glossary's operation kinds gain
*reversal* (item 1), *custody change*, *credential change*, *recovery* and *opening load*. A correction is
a reversal plus a new operation.

## Item 8 — Missing error names

**Problem.** No error for a reservation while the deciding component is down, or for an internal fault.

**Recommended ruling.** Add `ERR-DecisionUnavailable` (the reservation cannot be decided now; safe to retry
with the same client key) and `ERR-Internal` (an unexpected fault; no detail is returned to the caller,
and an incident is raised). A binding fault during reserve is `ERR-Internal`, not `ERR-NotAuthorised`.

**Ruling (2026-09-29): accepted.**

- `ERR-DecisionUnavailable`: the reservation cannot be decided now. Retrying with the same client key is
  safe: the command is kept, and a retry returns its decision once it is made.
- `ERR-Internal`: returns no detail to the caller, and raises an incident (new `REQ-ER-2`).

## Item 9 — Who owns the privileged-read path

**Problem.** `REQ-PV-4`'s single instrumented privileged-read path has no owning feature.

**Recommended ruling.** No spec change. The plan assigns it to `F-013` (audit and operator operations).

**Ruling (2026-09-29): accepted.** `F-013` owns it. The spec does not change.

## Item 10 — Recovery details

**Problem.** Unstated: removing the last credential; whether any use of a credential cancels a pending
recovery; whether grants survive a succession.

**Recommended ruling.**
- Removing an identity's **last** credential is refused (`ERR-NotAuthorised`). A holder can never lock
  themselves out outside a recovery.
- Only an explicit `cancel_recovery` with an existing credential cancels a pending recovery. Ordinary use
  of a credential does not.
- On **succession**, delegated authorities granted by the previous holder are **revoked**; the successor
  grants again. On **recovery**, they survive (`REQ-AU-14` already covers custody changes).

**Ruling (2026-09-29): accepted.** Applied to `REQ-AU-16`, `REQ-AU-17`, `REQ-AU-20` and `AC-A8.2`, whose
"presenting my existing credential" now says "to `cancel_recovery`".

## Item 11 — An unsafe release and `Needs-Review`

**Problem.** `AC-C3.2` says that when the external leg may still execute, a release is refused **and the
operation moves to `Needs-Review`** (`REQ-OP-9`). §5.6 has no such edge: it reaches `Needs-Review` from
`Submitted` only once the deadline has passed. Found while building `F-007`. Meanwhile an unsafe release is
refused with `ERR-ReleaseUnsafe`, the state is unchanged, and the operation joins the review queue. The
conformance gate for `M-1` defers `AC-C3.2` until this is ruled (issue #53).

**Recommended ruling.** Add an edge to `Needs-Review` from every state where an external leg may still
execute (`Submitted`, `Observed`), taken when a release is refused as unsafe. The refusal is still
`ERR-ReleaseUnsafe`; the move and the refusal are recorded together. `resolve_operation` then applies
(`REQ-RC-1`). `AC-C3.2` stands as written.

**Ruling (2026-09-29): accepted.** The owner's principle:

> **Every release must be backed by proof. If there is undeniable proof that a reservation is releasable,
> it releases immediately, with no manual step.**

This item's edge covers the **other** case: a release **requested without that proof**. It is refused
with `ERR-ReleaseUnsafe`, and the operation goes to `Needs-Review`, recorded together. **This edge cannot
release anything.** It moves no value, appends no posting, and leaves the value reserved. It is the edge
that *prevents* an unsafe release: a request that could not be honoured safely is taken out of the
caller's hands and given to a human with the evidence, instead of being left to be retried. A release
backed by proof never takes it: it releases (item 3).

Applied from every non-terminal state in which the external leg may still execute: `Submitted` and
`Observed`, as recommended, and `Expired` and `Orphaned`, whose pairs to `Needs-Review` exist after item 5.
`AC-C3.2` is no longer deferred in the conformance gate.

The principle holds in `Needs-Review` too. A proof that arrives once the operation is there releases it
at once (`REQ-RC-5` step 1, items 1 and 4). The one release not backed by proof is `REQ-RC-5` step 3,
which the owner's later refinement of items 1 and 4 allows: two distinct principals, after the adapter
checked submitted evidence and found none, with an incident. `REQ-OP-9` names it as that exception.

---

## What this costs, stated plainly

- **More operations reach `Needs-Review`.** Past its deadline, a `Reserved` or `Accepted` operation now
  moves there instead of waiting in the review queue under its own state; once there, the caller can no
  longer capture or release it. Deadlines must be set with that in mind.
- **An early release request costs the caller the automatic path.** A caller who asks to release while
  the leg may still execute sends the operation to a human, even if a proof would have arrived moments
  later. That is the price of never releasing on a request alone.
- **Two principals can release value without mechanical proof** (`REQ-RC-5` step 3), once the adapter
  has checked submitted evidence and found none. It is rare, raises an incident, and is the one release
  that is not backed by proof (item 11).
- **V0 enforces three invariants below their target layer**, now on the record in §9's V0 column.

## Identifier ledger

| Action | Identifiers |
|---|---|
| Withdrawn | state `Reversed`; posting kind `Adjustment`; the edges `Settled → Reversed` and `Needs-Review → Reversed` |
| New | `ERR-DecisionUnavailable`, `ERR-Internal`, `REQ-ER-2`, `REQ-LA-7`, `REQ-RC-5`, `AC-K1.3`, `AC-K1.4`, `AC-K1.5`, `AC-K1.6`, `AC-K2.3`, the command `submit_evidence`, the §9 V0 column; §5.6 edges `Received`, `Accepted`, `Reserved`, `Final`, `Orphaned` → `Needs-Review` (item 5), `Submitted → Released` and `Observed → Released` (item 3) |
| Raised and closed | `OQ-62` (step 3 and `INV-10`; ruled on 2026-09-29: two principals only release) |
| Revised | `REQ-OP-4`, `REQ-OP-5`, `REQ-OP-9`, `REQ-AR-6`, `REQ-RC-1`, `REQ-RC-2`, `REQ-OS-2`, `REQ-OS-3`, `REQ-CS-7`, `REQ-PO-3`, `ERR-ApprovalRequired`, `INV-15`, `REQ-AU-16`, `REQ-AU-17`, `REQ-AU-20`, `AC-A8.2`; §2 glossary (encumbered, freeze, operation, reversal); §5.4; §5.5; §5.6 (states, transitions, "When" cells, book effects, prose); §8.1 (`release`, `resolve_operation`, `propose_reversal` / `approve_reversal`, and the kinds of the commands); §15 |
| Unchanged | `REQ-PV-4` (owned by `F-013` in the plan); `AC-C3.2` (stands as written) |

## Carried to the plan

- `F-007`: the table as data follows §5.6; `past_deadline` fires `Needs-Review` from every non-terminal
  state; `resume` stays the operator's forward path.
- `F-010`, `F-016`: the proved rejection, and the states it releases from.
- `F-013`: the reversal operation and its two authorities; freeze as an account status; the three steps of
  `REQ-RC-5`, with `submit_evidence` and the two-principal release.
- `F-027`: the adapter's side of step 2, fetching and verifying submitted evidence (`EvidenceFetch`).
- `F-009`: `ERR-DecisionUnavailable` for a reservation the deciding component cannot decide in time;
  `ERR-Internal` for a fault.
- `F-012`: item 10's three rules as the only behaviour.
- `F-015`: the V0 column in the catalogue and the floor; `AC-C3.2` required again.
