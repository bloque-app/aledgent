<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-012 — Custody switching, credentials and recovery

| | |
|---|---|
| Milestone | M-3 — Production V0 (see Open points: the `M-1` gate also names `US-A5` and `US-A7`) |
| Track | A |
| Home | `crates/ledger-core` |
| Depends on | `F-003`, `F-007`, `F-011`, `F-013`. `DEP-7` (planned meanwhile, not blocking) |
| Unblocks | `F-020`, `M-3` |
| Realises | `DEC-3`, `DEC-13` (library side), `DEC-6` (delays run on injected time) |

## Goal

An identity holds a set of credentials. Any of them exercises owner authority unless the holder scoped it
to some accounts. A holder moves an account between owned and delegated custody. Only the holder can
give the operator power. Reclaiming it needs a working holder credential. A holder who lost every
credential recovers through a recovery authority that is not the operator, after a delay they can cancel.
A holder may name a successor, and may keep a recovery credential away from everyday devices. Every step
is attributed, audited and visible to the account's authority holders.

## Spec traces

- `US-A5`, `AC-A5.1`–`AC-A5.4` — custody honoured and visible; recovery shown before owned custody;
  credential sets (`REQ-AU-6`, `REQ-AU-7`, `REQ-AU-9`, `REQ-AU-20`).
- `US-A7`, `AC-A7.1`–`AC-A7.5` — `delegate_custody`, `reclaim_custody` (`REQ-AU-11`–`AU-14`).
- `US-A8`, `AC-A8.1`–`AC-A8.6` — recovery, cancellation, completion, successor, holder-held credential
  (`REQ-AU-15`–`AU-18`).
- `REQ-AU-19` — guardian thresholds cannot be configured.
- `REQ-AU-3`, `REQ-AU-4`, `INV-22` — removal is immediate; no credential widens itself.
- `INV-20`, `REQ-AT-1` — pending changes visible; audit names every approver.
- Errors: `ERR-NotAuthorised`, `ERR-CredentialInvalid`, `ERR-RecoveryNotElapsed`, `ERR-ClientKeyReused`.

## Scope

**In:** `register_credential`, `remove_credential`, `delegate_custody`, `reclaim_custody`,
`initiate_recovery`, `cancel_recovery`, `complete_recovery`, `name_successor`, `claim_succession`; owner
authority through the credential set; the recovery-mechanism listing; the recovery and succession state
machine; start-up checks on the recovery configuration.

**Out:** the data model of identities, credentials, custody and authority kinds, and the authority
evaluator (`F-003`); verifying a credential's signature at the boundary (`F-009`, under `DEP-7`); the
audit store (`F-013`); the recovery authority's own service and runbook (`DEC-13`; see Open points);
holder-held **kernel** keys (`F-020`); encumbrance (`DEF-11`).

## Design

**1. Credential set (`REQ-AU-20`).** `F-003` holds, per identity, `CredentialSet { members:
BTreeMap<CredentialId, Member> }` with `Member { verifier, scope: All | Accounts(set), kind: Everyday |
HolderRecovery, status: Active | Retired { at, by } }`. aledgent stores only verifiers, such as public
keys. A secret never enters the book or a log (`REQ-AU-8`). Ingress checks the proof of possession and
passes the core an authenticated `(principal, credential id)`. VOS already delivers these as separate,
non-interchangeable fields. **Owner authority** over account A holds when: the
credential is in the set of A's owner, it is `Active`, and its scope covers A (`AC-A5.4`). Retirement
takes effect in the commit that records it. The next command with that credential gets
`ERR-CredentialInvalid` (`REQ-AU-3`).

**2. Changing the set.** `register_credential` needs an `Active` credential of the set, or a completed
recovery. **Proposal:** a scoped credential may register only a credential whose scope is a subset of
its own. Only an unscoped credential, or a recovery, may register an unscoped one. This keeps widening
structurally impossible (`REQ-AU-4`, `INV-22`). *Alternative:* any member may register any scope. That is
simpler, but a scoped credential could then mint an unscoped one. `remove_credential` needs an `Active`
credential of the set. Without one: `ERR-NotAuthorised` (`AC-A5.4`). `HolderRecovery` members are added
the same way and marked as such, only when the deployment enables `REQ-AU-18`.

**3. `delegate_custody` (owned → delegated; `REQ-AU-12`).** The initiator must act with owner authority
from the set. Operator, delegated and recovery authority all get `ERR-NotAuthorised` (`AC-A7.2`). The
effect is `custody = Delegated` on the account. `F-003`'s evaluator reads custody at each check, so
operator authority exists from the next command (`AC-A7.1`). The change is recorded as the holder's.

**4. `reclaim_custody` (delegated → owned; `REQ-AU-13`).**
- *Proven credential.* The command must be signed by an `Active` set member over a challenge that binds
  the account id and the operation reference. The signature is the proof that the credential works.
  With no such member, or a bad signature: `ERR-CredentialInvalid` (`AC-A7.4`).
- *In-flight operations.* At acceptance, each operation records an authority snapshot `(kind, principal,
  custody at acceptance)`. Later transitions of that operation, and its capture or release, are checked
  against the snapshot, not against current custody. So operations already reserved finish under the
  authority that started them (`AC-A7.3`, `REQ-OP-9`). New operations use current custody.
- *Encumbrance.* In V0 no encumbrance can exist (`DEF-11`). The guard checks `encumbered = 0` and refuses
  otherwise. A conformance test pins the V0 case.
- *Grants.* Delegated grants to agents and members are not touched (`REQ-AU-14`, `AC-A7.5`).

**5. Recovery mechanisms shown first (`REQ-AU-9`, `AC-A5.3`).** `recovery_mechanisms(account)` lists what
applies: the recovery authority, always; succession, if the deployment offers it; the holder-held
credential, if enabled and registered. The list includes the delays. Any command that makes an account
owned (`open_account` with `Owned`, `reclaim_custody`) must carry `acknowledged_mechanisms`, the digest of
that list. A missing or stale digest is refused. **Proposal:** the digest acknowledgement.
*Alternative:* a separate `acknowledge_recovery_terms` step recorded before the switch.

**6. Recovery and succession as a closed state machine.** One record type, `PendingChange { kind:
Recovery | Succession, account, initiated_by, initiated_at, effective_at, new_credential | successor,
approvals }`. States: `Pending`, `Cancelled`, `Completed`. Edges: `Pending → Cancelled` and
`Pending → Completed`. It is encoded as a constant table, like `F-007`'s, and checked by the same
enumeration harness. At most one `Pending` record per account and kind. A second `initiate_recovery`
returns the first through the client key, or is refused.

**7. Recovery (`REQ-AU-15`, `REQ-AU-16`, `DEC-13`).**
- `initiate_recovery`: account `Owned`. Initiated by the operator on the holder's out-of-band request,
  or by the recovery authority. `effective_at = now + recovery_delay`. `now` is the invocation's
  injected time (`DEC-6`). The default delay is 72 hours (`DEC-13`).
- `cancel_recovery`: any `Active` member of the set whose scope covers the account, including a
  `HolderRecovery` member, before completion (`AC-A8.2`).
- `complete_recovery`: `now ≥ effective_at`, else `ERR-RecoveryNotElapsed` (`AC-A8.4`). It also needs a
  recovery-authority approval: a signature by the configured recovery key over
  `(pending id, account, new credential verifier)`. Without it: `ERR-NotAuthorised`. The operator can
  submit the command but cannot supply the approval (`AC-A8.1`). On completion, in one commit: the new
  credential joins the set, the covering credentials are retired (see Open points), and the audit record
  names the initiator, the recovery authority and the submitter (`AC-A8.3`).
- The recovery authority is its own authority kind in `F-003`. It is valid only for these commands. Any
  value-moving command under it gets `ERR-NotAuthorised` (`REQ-AU-15`, `RISK-9`).

**8. Succession (`REQ-AU-17`).** `name_successor` needs owner authority from the set. It records a
successor identity. **Proposal:** `claim_succession` works in two phases. The first call by the successor
opens a `Pending` succession with `effective_at = now + succession_delay` (default 30 days, `DEC-13`).
The holder can cancel it. A second call after `effective_at` completes it, and an early call gets
`ERR-RecoveryNotElapsed`. On completion the account's owner becomes the successor identity, and the
previous owner's credentials no longer cover it (`AC-A8.5`). *Alternative:* the delay runs from naming.
Then a successor could take over, with no warning, an account whose holder is still active.

**9. Holder-held recovery credential (`REQ-AU-18`, `AC-A8.6`).** It is a set member of kind
`HolderRecovery`. Presenting it is owner authority at once, with no delay. It can also cancel a pending
recovery. It never replaces the recovery authority: start-up fails if owned custody is offered and no
recovery key is configured.

**10. Start-up checks (`REQ-AU-15`, `REQ-AU-19`).** The deployment fails to start if the recovery
authority's key equals any operator key, or if owned custody is enabled with no recovery key. The
configuration schema has no threshold or guardian field, so guardian recovery cannot be expressed.
Organisational separation (`DEC-13`) cannot be checked in code. The runbook owns it.

**11. Visibility and audit (`INV-20`, `REQ-AT-1`).** Every command here is idempotent on its client key and
attributed (`F-007`, `REQ-AU-11`). Each one writes an audit record to `F-013` and a notification to every
authority holder of the affected accounts. `F-011`'s account view shows the custody mode, any `Pending`
recovery or succession with its `effective_at`, and the named successor.

## Test plan

- `AC-A5.1`: an owned account refuses every operator-authority command. The evaluator test covers each
  operation kind. `AC-A5.2`: operator actions on a delegated account are visible to the holder as operator
  actions (with `F-011`).
- `AC-A5.3`: switching to owned without the current digest is refused. With it, the switch succeeds.
- `AC-A5.4`: two credentials, one scoped. Each acts where its scope allows. A set change without a member
  or a recovery gets `ERR-NotAuthorised`.
- `AC-A7.1`–`AC-A7.5`: one test each, including a reserved operation that completes under the snapshot
  after a reclaim.
- `AC-A8.1`–`AC-A8.6`: one test each. Time is advanced by `tick` only. No test sleeps.
- `INV-22`: property test. No sequence of set commands from a scoped credential produces a member wider
  than that credential.
- The recovery state machine goes through the `F-007` enumeration harness: no pair outside the table.
- Start-up check tests for `REQ-AU-15` and `REQ-AU-19`.
- All of the above go into the `F-015` suite.

## Exit criterion

`AC-A5.1`–`AC-A5.4`, `AC-A7.1`–`AC-A7.5` and `AC-A8.1`–`AC-A8.6` pass in the conformance suite against the
library. The recovery state machine passes the enumeration check. `INV-22` holds under the property test.

## Open points

- **Milestone mismatch.** PLAN places `F-012` in `M-3`. The `M-1` gate asks for Epic A green except
  `US-A8`, which includes `US-A5` and `US-A7`. Suggested split: tasks T-012-01 to T-012-05 by `M-1`, and
  recovery by `M-3`. The owner should decide.
- **What "the previous one stops working" means with a set** (`AC-A8.3`). Meanwhile completion retires
  every member whose scope covers the recovered account. Members scoped only to other accounts are kept.
  This is a reading.
- **Does any use of an existing credential cancel a pending recovery** (`AC-A8.2` "presenting")? Meanwhile
  only `cancel_recovery` cancels, and any use of a credential during the delay triggers a notification.
- **Removing the last credential** is not forbidden by the spec. Meanwhile it is allowed, and the account
  stays reachable through recovery.
- **Grants after succession.** The spec does not say whether the previous holder's delegated grants
  survive a succession. Meanwhile they are kept (the nearest rule is `REQ-AU-14`).
- **Custody change as an "operation".** `REQ-AU-11` calls it an operation. §2's list of operation kinds and
  §5.6's book effects do not include it. Meanwhile these commands have a client key and an operation
  reference but no lifecycle posting.
- **No home for the recovery-authority service.** `DEC-13` describes a separate service that exposes only
  `approve_recovery`. `DEC-15` has no path for it and no feature owns it. This feature builds the library
  side and a test double. **Proposal:** add `services/recovery` to `DEC-15`, owned by a new feature.
- **Proposal:** the scope-subset rule (point 2), the digest acknowledgement (point 5), and the two-phase
  succession (point 8).
- **`DEP-7`.** If holder credentials may not reach aledgent directly (`OQ-12`), the integrating party
  relays every command here. Then "signed by the holder's credential" must be carried through the relay
  intact. The design needs that either way.
