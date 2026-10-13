<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-013 — Audit trail and operator operations

> **Amendment 0006 (ruled 2026-09-29).** A reversal is an operation of its own, approved by a
> second principal under its own authority; the reversed operation stays `Settled`. `Reversed`,
> `Needs-Review → Reversed` and the `Adjustment` posting kind are withdrawn, and a freeze is an
> account status. `NOTES.md` and `T-013-10` record what changed.

| | |
|---|---|
| Milestone | M-1 — The book, in a process |
| Track | A |
| Home | `crates/ledger-core` |
| Depends on | `F-001`, `F-003`, `F-004`, `F-007` |
| Unblocks | `F-012` (operator frame for recovery), `F-014`, `F-015` (Epic K), `F-017` (disclosure audit), `F-018` |
| Realises | `DEC-1` (the audit record shape), `DEC-3` (which authority acted) |

## Goal
Every value-moving operation, authority change, privileged read, operator intervention and disclosure
leaves an append-only audit record. The records are kept apart from the book's storage. Every operator
power is its own named operation, with its own authority, a required justification and a record. The
operator can resolve a `Needs-Review` operation and reverse a finalised movement with two people. There
is no other way to change the book.

## Spec traces
- `REQ-AT-1` — the five classes that produce an audit record, and its fields.
- `REQ-AT-2`, `NFR-6` — append-only, retained apart from operational storage, queryable.
- `REQ-AT-3` — operator intervention is a first-class, audited operation. No storage repair.
- `REQ-OS-1` — one authority, one record, one justification per operator capability. No catch-all.
- `REQ-OS-2` — the minimum operator surface.
- `REQ-OS-3` — the approver of a reversal differs from the proposer.
- `REQ-OS-4` — operator actions are visible to the account's authority holders.
- `REQ-RC-1` — every non-terminal state has an operator resolution path.
- `REQ-RC-2` — reversal needs human approval, raises an incident, is never automated.
- `REQ-PO-3` — a reversal references its posting, is complete, and happens at most once.
- `REQ-AU-5`, `REQ-AU-7`, `REQ-AU-10` — operator authority split by kind; the authority used is recorded.
- `REQ-AS-1`, `REQ-AS-2` — registering an asset is a reviewed, recorded change.
- `INV-2`, `INV-10`, `INV-11`, `INV-20`, `INV-21` — kept by the operator paths.
- `US-K1`, `US-K2`, `US-K4` — `AC-K1.1`, `AC-K1.2`, `AC-K2.1`, `AC-K2.2`, `AC-K4.1`, `AC-K4.2`.

## Scope
**In:** the audit record type and sink; the audit store for the in-process host; the operator-operation
frame; `resolve_operation`; `propose_reversal` / `approve_reversal`; freeze and unfreeze;
`register_asset`; grant and revoke of issuance authority on the operator surface; the record of the
settlement adapter's declared support; corrections through the ordinary command surface.
**Out:** recovery commands (`F-012`, which uses this feature's frame); encumbrance, deferred (`F-024`,
`DEF-11`); the transition table (`F-007`); authority semantics and bounds (`F-003`); how the adapter
declares support at start-up (`F-016`); the query surface that shows operator actions (`F-011`); the
privileged-read mechanism itself (see Open points); paging on incidents (`F-018`).

## Design

**Audit record.**
```
AuditRecord { audit_seq, prev_hash, class, principal, on_behalf_of, authority_used,
              subject, operation_ref, at, justification, outcome }
```
- `class`: `ValueMoving | AuthorityChange | PrivilegedRead | OperatorIntervention | Disclosure`
  (`REQ-AT-1`).
- `authority_used`: `Owner | Delegated | Operator | Issuance | Recovery` (`REQ-AU-7`, `DEC-3`).
- `subject`: account, identity, operation, asset or posting references. Never a credential
  (`REQ-AU-8`).
- `justification`: required for `OperatorIntervention`, and for a `Disclosure` made under a legal
  obligation (`REQ-DS-5`). Missing or blank is `ERR-JustificationRequired`.
- `at`: the time input of the transition (`REQ-RT-7`).
- `outcome`: accepted, or the named `ERR-*` of a refusal.
- `audit_seq` and `prev_hash` form a hash chain, so removing or changing a record is detectable.
Justification text lives only in the audit store. It never enters the book, an identifier or an
anchored value.

**Emission inside the commit.** The library returns audit records as part of every transition, beside
postings, operation changes and outbox items. The host commits them in the same boundary as the book
(`REQ-RT-1`). In-process, that is the same store transaction. On VOS, they ride the Agent's outbox and
delta (`F-005`). A worker then copies them to the audit store, at least once, deduplicated by
`audit_seq`. So a posting can never commit without its record. Privileged reads and disclosures do not
commit to the book. For them the query and disclosure paths write the record to the audit store
**before** returning data, and refuse if the write fails (`REQ-PV-5`).

**Audit store.** **Proposal:** an append-only, segmented, hash-chained log, written by its own process
with its own credentials. It is stored on media apart from the book's storage and its backups
(`REQ-AT-2`). It has a read API by subject, principal, class and time range (`NFR-6`). Its head is
checked against the core's last emitted `audit_seq` on a cadence. A gap raises an incident.
*Alternative:* write-once object storage with retention lock. It is stronger against deletion and
weaker for queries. The retention period is deployment configuration (`NFR-6`).

**Operator-operation frame.** Every operator command goes through one frame:
1. The principal must hold operator authority **for this kind** of operation. Grants name a non-empty
   set of `OperatorOpKind`. There is no "all" variant, so a catch-all grant cannot be expressed
   (`REQ-OS-1`, `REQ-AU-5`, `REQ-AU-2`).
2. A justification is present (`ERR-JustificationRequired`).
3. The client key makes it idempotent (`REQ-OP-11`).
4. Operator authority over an account applies only in delegated custody (`REQ-AU-5`, `REQ-AU-6`).
   Otherwise `ERR-NotAuthorised`.
5. The action is recorded with `authority_used = Operator`. It is visible to the account's authority
   holders: postings carry the authority kind (`INV-20`, `REQ-QR-4`), and non-posting actions go to the
   account's action log, a service record read by `F-011` (`REQ-OS-4`).
`F-012`'s recovery commands use steps 2, 3 and 5. The recovery authority check stays in `F-012`.

**`resolve_operation`.** Input: reference, target (`Settled`, `Released` or `Reversed`), evidence,
justification. `F-007` checks the transition from `Needs-Review`. Otherwise `ERR-InvalidTransition` or
`ERR-OperationTerminal`. The posting follows §5.6's book-effects table and is built by `F-004`.
**Proposal on evidence:**
- To `Settled` with an external leg: the evidence must name a coordinate whose observation is recorded
  as final (`REQ-IN-3`). Otherwise `ERR-EvidenceInsufficient`. This keeps `INV-10` and `INV-11`: the
  operator supplies evidence, never an outcome.
- To `Released`: the evidence must show the instruction can no longer execute, or a final failure
  observation (`REQ-OP-9`). Otherwise `ERR-ReleaseUnsafe`.
- To `Reversed`: through the reversal pair below.
Evidence found later, for example from an archive after a gap (`REQ-IN-5`), is first recorded with
`record_observation` and then cited.

**`propose_reversal` / `approve_reversal`.** A proposal names a settled operation, its settling posting
and a justification. It creates a pending reversal record and raises an incident: an outbox item on the
operator incident channel, paged by `F-018` (`REQ-RC-2`). Approval needs a different principal holding
the reversal-approval kind. The same principal gets `ERR-ApprovalRequired` (`REQ-OS-3`). There is no
code path that approves automatically (`REQ-RC-2`). On approval the library posts, as one linked chain
(`REQ-KER-10`), a mirror of the settling posting with CipherClerk's `void_of`. If the operation reserved
under `DEC-2`, it also posts a mirror of the `Reserve` posting, so the payer's value comes back. Both
postings have kind `Reversal` and reference their originals. The kernel refuses a second void of the
same transfer (`TransferAlreadyVoided`). That makes "at most one reversal per posting"
kernel-strength (`REQ-PO-3`). The mirror must match the original exactly (`VoidEntriesMustMirror`), so a
reversal is always complete. The operation goes `Settled → Reversed` (`F-007`).

**Freeze and unfreeze.** `freeze_account` / `unfreeze_account` switch the account status between
`Active` and `Frozen` (§5.2). **Proposal:** a frozen account refuses every operation that debits or
reserves against it, with `ERR-AccountFrozen`. Credits still land, because `INV-9` forbids discarding an
inbound credit to an existing account. No posting is made.

**`register_asset`.** Operator authority of kind asset-registration, plus a review reference: the id of
the reviewed change, carried in the command (`REQ-AS-2`). The registry itself is `F-003`'s. A changed
precision for an existing asset is refused. Precision lives only in the registry (`INV-24`).

**Issuance authority.** `grant_authority` and `revoke_authority` with issuance scope go through the
frame. Bounds per window and per asset are `F-003`'s (`REQ-AU-10`).

**Settlement adapter support.** `declare_adapter_support` records the (operation, asset) pairs the
adapter supports, as an operator operation (`REQ-OS-2`). `submit_outbound` reads this record to return
`ERR-UnsupportedLeg` at submission (`REQ-SA-1`, `F-010`). `F-016` supplies the declaration.

**Corrections.** There is no correction command. A correction is an ordinary command: `transfer`,
`issue`, `redeem` or a reversal, under an authority, with a client key (`US-K4`, `AC-K4.1`). It is a
posting and every invariant applies (`AC-K4.2`). The library exposes no API that writes a leaf, a
balance or an operation state outside a command (`REQ-CS-5`, `REQ-AT-3`).

## Test plan
- `AC-K1.1`: resolve a `Needs-Review` operation to each allowed target, through the frame. Refusals for
  a missing authority kind, missing justification and insufficient evidence.
- `AC-K1.2`: the audit record names principal, time, subject, justification and authority. A test
  asserts the library's public API has no write path outside commands (a compile-time API surface
  check, plus a search over exported symbols).
- `AC-K2.1`: a reversal proposal raises an incident. Self-approval gives `ERR-ApprovalRequired`. No
  timer, tick or observation can approve.
- `AC-K2.2`: after approval, the original posting is unchanged. The reversal references it. A second
  reversal of the same posting fails at the kernel (`INV-2`, `REQ-PO-3`).
- `AC-K4.1`, `AC-K4.2`: a correction by `transfer` under operator authority needs a client key and an
  authority, and passes the full invariant suite.
- `REQ-AT-1`: property test. For random command streams, every command in the five classes produces
  exactly one audit record, and the chain has no gap.
- `REQ-AT-2`: the audit store rejects rewrites and truncation. A chain break is detected.
- `INV-9`: a frozen account still receives credits.
- `INV-20`, `REQ-OS-4`: every operator action appears in the account's view for its authority holders.

## Exit criterion
`AC-K1.1`, `AC-K1.2`, `AC-K2.1`, `AC-K2.2`, `AC-K4.1` and `AC-K4.2` pass in the conformance suite in a
plain process. Every operator capability in `REQ-OS-2` that is in V0 exists as its own command with its
own authority kind. The audit property test finds one record per audited action and no chain gap.

## Open points
- **Freeze and the meaning of "available".** The glossary lists a freeze as an example of *encumbered*.
  V0 defers encumbrance (`DEF-11`) but keeps freeze in `REQ-OS-2`. With the Proposal above, a frozen
  account's value stays in `available` but cannot be spent. That reads against the definition of
  available. The owner decides between a spec clarification (amendment) and holding freeze until
  `F-024`. Also open: whether captures of reservations made before the freeze may proceed.
- **Proposal:** evidence rules for `resolve_operation`. Stricter than §5.6 states. The owner should
  confirm. The question is whether an operator may release on justification alone when the settlement
  layer can never give proof (a non-expiring instruction). If not, such value stays reserved.
- **`Needs-Review → Reversed`.** §5.6 allows it, but it is unclear what posting it reverses. The
  **Proposal** is a `Reversal` of the `Reserve` posting through the same two-person approval. It needs
  the owner's confirmation.
- **Reversal after the value was spent.** If the destination no longer holds the amount, the kernel
  refuses the mirror (`ERR-InsufficientAvailable`). A partial reversal is forbidden (`REQ-PO-3`). There
  is then no path but new operations. That is consistent with §13, but should be stated to operators.
- **`Adjustment` postings.** §5.5 lists the posting kind `Adjustment`, but no §8.1 command produces one.
  This plan makes corrections from existing commands. If an `adjust` command is wanted, it is an
  amendment.
- **Privileged reads.** `REQ-AT-1` needs a record per privileged read. `REQ-PV-4`'s single instrumented
  mechanism has no owning feature in `PLAN.md` §4. This feature provides the record. The mechanism
  needs an owner, `F-011` or `F-003`.
- **Proposal:** the audit store's form, and its home. `DEC-15` names no path for it. Candidates:
  `crates/audit-store`, or `ops/`.
