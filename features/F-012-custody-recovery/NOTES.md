<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-012 — implementation notes

Written while implementing `F-012` (issue #12), 2026-09-23. The plan's proposals were followed as
accepted: the scope-subset rule (design point 2), the digest acknowledgement (point 5) and the
two-phase succession (point 8); and `services/recovery` as the recovery authority's home (`DEC-15`).
Where the plan was silent, the more conservative behaviour was chosen (refuse rather than allow); each
such choice is listed here for review.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/custody/mod.rs` | `RecoveryConfig` and its start-up checks, `RecoveryAuthority` (how records name it), the two signed messages (`reclaim_challenge`, `approval_message`) and `verifies`, `RecoveryMechanisms` and `MechanismsDigest`, the in-flight custody `snapshot`, `recovery_rules` (T-012-05, T-012-08) |
| `crates/ledger-core/src/custody/machine.rs` | The pending-change state machine as a constant table, `next()`, and its enumeration (T-012-06) |
| `crates/ledger-core/src/custody/store.rs`, `src/state/custody.rs` | Pending changes, named successors and custody notices in the service store, `Write::Custody`, and the store's rules (T-012-06, T-012-07, T-012-09) |
| `crates/ledger-core/src/ledger/custody.rs` | The nine commands, `recovery_terms`, `recovery_mechanisms`, `custody_view`, `custody_notices` (T-012-01 to T-012-09) |
| `crates/ledger-core/src/eval.rs` | `authorise_in_flight`: a later transition checked against the custody its operation was accepted under (T-012-04) |
| `crates/ledger-core/src/ledger.rs`, `ledger/requests.rs`, `lifecycle/mod.rs` | `OpenAccount::acknowledged`; the recovery configuration in `LifecycleConfig`, checked at genesis (`GenesisError::Recovery`) (T-012-05, T-012-08) |
| `crates/ledger-core/src/ledger/reservations.rs`, `ledger/settlement.rs` | capture, release and `submit_outbound` authorised with the snapshot (T-012-04) |
| `crates/ledger-core/src/audit/mod.rs`, `state.rs` | Nine `Command`s, classified and coded (T-012-09) |
| `crates/ledger-core/src/ledger/tests/custody.rs` | Unit tests, the `INV-22` property test, the store's rules |
| `services/recovery/` | The recovery-authority service: `key.rs` (the key, kept apart), `lib.rs` (`approve_recovery`, its one capability), `RUNBOOK.md` (the out-of-band verification) |
| `crates/conformance/src/custody.rs`, `cases/f012.rs`, `harness.rs` | `CustodyPort`; the 16 registered cases, run; the in-process deployment's recovery configuration |

## Amendment 0006 item 10 (ruled 2026-09-29)

The owner accepted the three recommendations, which this feature had taken as its conservative
behaviour; they are now `REQ-AU-16`, `REQ-AU-17`, `REQ-AU-20` and `AC-A8.2` in SPEC v0.4.0. Each is
**one function** in `ledger_core::custody::recovery_rules` (the module was `pending_0006`):

- **Removing the last credential** — `may_remove(active_after)`: refused (`ERR-NotAuthorised`,
  `REQ-AU-20`). A holder can never lock themselves out outside a recovery. Test:
  `req_au_20_the_last_credential_cannot_be_removed`.
- **Ordinary use of a credential does not cancel a pending recovery** — `use_cancels_pending()`:
  `false`. Only an explicit `cancel_recovery` with an existing credential cancels (`REQ-AU-16`;
  `AC-A8.2` now says "presenting my existing credential to `cancel_recovery`"). No notification on
  every use during the delay (decision 12). Tests: `ac_a8_2_ordinary_use_does_not_cancel_a_recovery`,
  `ac_a8_2__service`.
- **Grants on a change** — `grants_revoked_on(kind)`: on **succession**, the delegated grants the
  previous holder made over the account are revoked and the successor grants again; on
  **recovery** they survive (`REQ-AU-17`, `REQ-AU-14`). Tests:
  `a_succession_passes_the_account_and_revokes_the_previous_holders_grants`,
  `a_recovery_completes_only_with_the_approval_and_after_the_delay`.

Operation kinds (item 7): `delegate_custody` and `reclaim_custody` are the kind *custody change*,
`register_credential` and `remove_credential` *credential change*, and the recovery and succession
commands *recovery* (SPEC §8.1). They stay commands with no lifecycle record, as built.

## Decisions taken during implementation

### Configuration and start-up (T-012-08)

1. **The recovery configuration travels in `LifecycleConfig`** (`recovery: RecoveryConfig`,
   `with_recovery`), the deployment configuration `Genesis` already carries, and `Ledger::genesis`
   runs its checks: owned custody without a recovery key, a recovery key equal to the operator's
   verifier, or a zero delay fail with `GenesisError::Recovery`. `LifecycleConfig::new` offers
   **delegated custody only** (no key, no default key: `REQ-AU-8`), so a deployment must state its
   recovery authority to offer owned custody. Every test fixture that opens owned accounts now does.
2. **The recovery key is never a credential of the book.** `register_credential` and
   `initiate_recovery` refuse its verifier for any identity, so the start-up check (distinct from every
   operator key) cannot be undone at run time.
3. **Delays are in the binding's time unit.** `DEC-13`'s defaults are given in seconds (72 × 3 600,
   30 × 86 400). A delay of zero is not a delay (`REQ-AU-16`) and is refused at start-up; the store
   also refuses a pending change whose `effective_at` is not after its `initiated_at`.
4. **No guardian field**: the schema is closed; a `compile_fail` doctest and an exhaustive
   destructuring test pin it (`REQ-AU-19`).

### The recovery authority (T-012-06, `DEC-13`)

5. **It holds no identity in the book** (SPEC §3: "only for value of its own"), so it can never be a
   caller: no value-moving command can run under it (`REQ-AU-15`, `RISK-9`), structurally. Its one act
   is a Schnorr signature (the kernel's own scheme, over Ristretto) over
   `approval_message(pending, account, verifier)`, checked against the configured key by
   `complete_recovery`. The audit record names it by identifiers **derived from its key**
   (`RecoveryAuthority`), with `AuthorityKind::Recovery`. A key rotation changes the name; that is the
   honest reading of "the recovery authority can be replaced by rotating its key".
6. **Initiation and completion are the operator's**, on the operator surface: the operator's policy
   authority (the operator organisation, unscoped), with a justification, idempotent on the client key,
   refusals audited — `F-013`'s frame steps 2, 3 and 5. Step 1 (a per-kind operator-surface grant) is
   not used, as `F-013` §4 says; there is no `OperatorOpKind` for recovery. "Requested by the recovery
   authority" (SPEC §8.1) is a request the operator records, like the holder's: the service's one
   capability is approving.
7. **What a recovery issues**: the verifier and scope are stated at initiation, recorded in the pending
   change, and bound by the approval (the pending id binds them). The scope must name the recovered
   account and only the owner's accounts, or be unscoped.
8. **"The previous one stops working"** (`AC-A8.3`): completion retires every active member of the
   owner's set whose scope covers the recovered account (unscoped ones included); members scoped only
   to other accounts are kept (the plan's reading). Completion is refused if the account changed owner
   or left owned custody since initiation (`ERR-NotAuthorised`): a delegation by the holder shows a
   working credential, and the holder may cancel.
9. **Order of refusals at completion**: the operator, the justification, the change (unknown,
   terminal), then the approval (`ERR-NotAuthorised`), then the delay (`ERR-RecoveryNotElapsed`), so an
   unapproved early attempt learns nothing about timing.

### Credential sets (T-012-01, T-012-02)

10. **A set command changes the caller's own set only.** Acting for another identity is
    `ERR-NotAuthorised`, so the operator cannot add or remove a holder's credential (`AC-A5.4`).
    `remove_credential` also needs the removed member to be within the remover's scope (the subset
    rule applied to removal: a scoped credential cannot remove a wider one).
11. **Retirement is `F-003`'s `CredentialStatus::Removed`**, absorbing, effective in the commit that
    records it; the plan's `Retired { at, by }` is carried by the audit record (who, when) rather than
    the credential.
12. **No notification on every use of a credential during a pending recovery.** It would hook every
    command's resolution. The pending recovery is shown on the account's custody view, and its
    initiation is notified to every authority holder; see "Amendment 0006 item 10".

### Custody switching (T-012-03, T-012-04, T-012-05)

13. **Who may switch.** Only the holder, through owner authority from the set. Anyone else is
    `ERR-NotAuthorised` if it reaches the account or is the operator (so `AC-A7.2` holds), and
    `ERR-UnknownAccount` otherwise (`INV-16`). A frozen account cannot switch (`ERR-AccountFrozen`):
    an owned frozen account could never be unfrozen, since the operator frame needs delegated custody.
14. **The proven credential** is any active member of the owner's set covering the account; its
    signature over `reclaim_challenge(account, custody epoch)` proves it works. A verifier that is not
    a 32-byte key proves nothing. The record names the proving credential beside the presenting one.
    The **custody epoch** is the number of custody changes the account has had; every custody change
    advances it in the same store write, and the custody view shows it. The plan bound the operation
    reference instead, but ingress (`F-009`) mints references from the OS random source, so a holder
    could never sign before submitting. Binding the epoch keeps the plan's property: a proof is good
    for the one reclaim that ends the current epoch, and useless after it.
15. **The in-flight snapshot is derived, not stored.** An operation accepted under operator authority
    was accepted in delegated custody (operator authority exists nowhere else), so its capture, release
    and submission are evaluated as if custody were still delegated (`eval::authorise_in_flight`,
    `custody::snapshot`). Owner and delegated authority do not depend on custody, so for them the
    current custody applies. This needs no new write in `reserve` or `withdraw`. Stated plainly: after
    a reclaim, any holder of a live operator grant with `Capture` (or `Release`) may finish an
    operator-started reservation — its bounds still apply — not only the principal that started it.
    `F-013`'s `resume` and `resolve_operation` still apply the frame's current-custody rule (their
    Open point stands).
16. **The acknowledgement digest** covers the recovery authority (its derived name and delay),
    succession (delay and the named successor) and the holder's holder-held credentials covering the
    account. For `open_account` it is computed for a new account of the owner (`recovery_terms`, for
    the owner or the operator opening it). It is part of `open_account`'s request digest only when
    given, so delegated requests keep their digests. Missing or stale is `ERR-NotAuthorised`.
17. **Encumbrance guard**: `encumbered()` is zero in V0 (`DEF-11`, `TODO(F-024)`); `AC-A7.3` pins it.

### Succession (T-012-07)

18. **The claim is recorded as owner authority claimed by the successor** (`AuthorityKind::Owner`,
    principal the successor, command `ClaimSuccession`): `REQ-AU-7`'s list has no successor kind.
19. **A successor is an active person or organisation other than the holder**; agents own nothing. It
    claims with an unscoped credential, for itself. A named successor who is not otherwise an authority
    holder is told of the naming and may claim; anyone else gets `ERR-UnknownAccount`.
20. **One pending succession per account.** Naming or clearing a successor while one is pending is
    refused: the holder cancels first. `cancel_recovery` cancels either kind of pending change.
21. **On completion** the owner changes in the store (`SetOwner`, the owner index moved), the named
    successor is cleared, and the previous owner's credentials no longer cover the account because it
    is no longer theirs. Credentials of the previous owner whose scope names the account are **not**
    retired, as they may name other accounts too; they simply carry no authority over it.

### Visibility and audit (T-012-09)

22. **Every command writes an attribution record naming its account**, which is the account's action
    log (`account_actions`), and the service store emits its audit record in the same batch. Set
    changes and holder custody changes are `AuthorityChange`; `initiate_recovery` and
    `complete_recovery` are `OperatorIntervention`. Command codes 30–38.
23. **Notices** (`Notice`, in the custody records' outbox) go to every authority holder of the account
    — the owner and every live grant covering it, under either custody for a custody change — and to a
    named successor. `F-005` makes the outbox durable. Ingress (`F-009`) delivers the lifecycle
    outbox only: delivering these notices is `TODO(F-012)` (see Deferred).
24. **The account view is `custody_view`**: custody, owner, successor and every recovery and succession
    with its state and `effective_at`, for whoever reaches the account. `F-011` has no account query of
    its own in SPEC §8.2, so it is a read of this feature's, reach-gated (`INV-16`).

### Store (the layer under the commands)

25. **`Write::Custody`**: the store refuses a change outside the table, out of a terminal state, a
    completion before `effective_at`, a second pending change of one kind per account, a change for an
    owner that does not own the account, a custody or owner change that changes nothing, and a
    successor that is the owner or does not exist — whoever writes.

### Through ingress (`F-009`, merged while this branch was open)

26. **Every command and read has a route.** Holder commands on the command surface:
    `POST /v1/credentials`, `/v1/credentials/{credential}/remove`,
    `/v1/accounts/{account}/custody/delegate`, `/v1/accounts/{account}/custody/reclaim`,
    `/v1/recoveries/{pending}/cancel`, `/v1/accounts/{account}/successor`,
    `/v1/accounts/{account}/succession/claim`. On `CTR-OPS`, routed like `F-013`'s operator commands:
    `POST /v1/ops/accounts/{account}/recovery` and `/v1/ops/recoveries/{pending}/complete`. Reads:
    `GET /v1/identities/{identity}/recovery-terms`, `/v1/accounts/{account}/recovery-mechanisms`
    (each with the digest a command carries) and `/v1/accounts/{account}/custody`. They are answered
    with the core's decision, like every command other than a transfer. `open_account`'s wire body
    gained `acknowledged` (hex). Signatures and digests are hex; a malformed one fails closed
    (`ERR-CredentialInvalid` for a reclaim proof, `ERR-NotAuthorised` otherwise). The OpenAPI
    document is regenerated.
27. **Ingress's deployment configuration** gained an optional `genesis.recovery` block — the recovery
    authority's public key, the delays in milliseconds, holder-held on or off. Without it the
    deployment offers delegated custody only. The in-memory test deployment (`testkit`) configures
    the suite's test recovery authority, so every ingress test can exercise owned custody.

## Conformance (T-012-09)

- `services/ingress/tests/custody.rs` drives owned custody, a credential, a reclaim and a recovery
  through the HTTP surface.
- 16 cases in `crates/conformance/src/cases/f012.rs`, all run, all green on both in-process stores:
  `ERR-RecoveryNotElapsed`, `AC-A5.1`–`AC-A5.4`, `AC-A7.1`–`AC-A7.5`, `AC-A8.1`–`AC-A8.6`. The suite
  plays the recovery authority with `services/recovery` over a test key, and holders sign their proofs
  with keys of their own.
- The recovery state machine's enumeration (soundness, completeness, reachability, absorption, the
  store's refusals) and the `INV-22` property test run as `ledger-core` unit tests.
- Every fixture that opens owned accounts — `F-003`, `F-004`, `F-007`, `F-008`, `F-010`, `F-011`,
  `F-013`'s cases and `ledger-core`'s tests — now states the deployment's recovery authority and
  carries the acknowledgement (`harness::acknowledgement`).

## Open points for the owner

- **Milestone.** PLAN places `F-012` in `M-3`; this PR builds all of it, including recovery.
- **Recovery authority identifiers** derive from its key (decision 5). If the owner wants a stable name
  across rotations, the configuration needs a name beside the key.
- **Resolution of owned-custody operations** (`F-013`'s Open point) is unchanged by the snapshot
  (decision 15).

## Deferred

- `TODO(F-012)`: ingress's delivery worker delivers the lifecycle outbox only; the custody notices
  (`State::custody().outbox()`) are not yet pushed to recipients.
- `TODO(F-009)`: relaying holder-signed commands intact under `DEP-7`.
- `TODO(F-024)`: the encumbrance a reclaim must see lifted or carried over with consent.
- `F-005`, `F-006`: the durable home of the custody records.
- `F-020`: holder-held kernel keys.

## Follow-up: the credential set index (issue #50, 2026-09-23)

`F-014`'s scale run (`notes/MEASUREMENTS-F014.md`) found opening an owned-custody account
quadratic over the book. The scan was `State::credentials_of`, which filtered **every credential of
the book** by identity. It runs twice per owned opening (the acknowledgement's
`recovery_terms`, then `open_account`'s own check, both through `RecoveryMechanisms::of` when the
deployment offers holder-held credentials), and also in `recovery_mechanisms`, `reclaim_custody`
(its acknowledgement), `remove_credential` (the last-member count) and `complete_recovery` (the members
covering the account).

- **What was indexed.** The service store keeps each identity's credential set,
  `IdentityId → {CredentialId}`, beside the credentials, like `by_owner` for accounts and
  `by_principal` for grants. `credentials_of` is a point lookup in it. It is changed by the one
  store write that changes the set's membership (`Write::PutCredential`) and by that write's undo,
  so a refused batch, a `check` and the lifecycle's rollback (`F-007`) leave it as they found it. A
  status change (removal, including a recovery's) changes no membership: `credentials_of` has always
  listed a set's members in whatever status, and still does. A succession changes the account's
  owner, not a set. The index is derived: not exported, rebuilt by `State::from_sections`, so an
  export is byte-identical to before.
- **Not a public-key index.** The issue proposed an index by public key for "the uniqueness check".
  There is none: no command refuses a verifier because another credential has it (the scale tools
  themselves reuse verifiers), and the only verifier check, the recovery key's, compares with the
  configuration. An index by key would serve no read, and a uniqueness rule would be new behaviour
  (refuse rather than allow is not ours to add without the spec). The scan was by identity, so that
  is what is indexed.
- **Every store.** The service store has one implementation, `State`, in memory in `M-1`; every
  kernel store (`MemStore`, `FileStore`, `F-006`'s redb `BookStore`) holds leaves only, and the
  ingress store holds the received-log. `F-006`'s service tree can already index a leaf by identity
  (`Fact::OwnedBy`); when the service store moves there (`TODO(F-005)`), a credential leaf states
  that fact.
- **Other scans.** `F-014`'s comparison (`export::compare`, `caller_of`) found each identity's
  caller by scanning every credential; it now reads the set. No other command path reads the
  credentials by scan. Recovery and succession still scan every **grant** for those the previous
  holder made over the account (`grants_of_holder`); that is a grant index, outside this issue.
- **Semantics.** Unchanged, and tested against the old definition: `credentials_of` returns the same
  members in the same order (credential id) as the scan, so the mechanisms' digest a holder
  acknowledged is the same bytes.

Tests (in `crates/ledger-core/src/ledger/tests/custody.rs`):

| Test | Covers |
|---|---|
| `req_au_20_the_set_index_follows_every_credential_changing_command` | genesis, `open_identity` (person, agent), `register_credential`, `remove_credential`, both refused, `delegate_custody`, `reclaim_custody`, `initiate_recovery`/`complete_recovery`, `name_successor`/`claim_succession`, and the rebuild from the export sections (byte-identical): after each, the index equals a recomputation and `credentials_of` equals the old scan for every identity |
| `req_au_20_the_set_index_is_the_scan_of_the_credentials` | property test at the store: random puts, removals, refused batches, `check`s and faulted commits over four identities; the same two checks after every step |

### Scale check

`cargo run --release -p conformance --bin f012-scale -- --accounts 50000 --batches 10`: 50 000
people, each with one owned-custody account, opened through the command surface in the conformance
deployment (holder-held credentials offered) on an in-memory book: nothing on disk, 168 MB resident.
It prints the median and mean time per open per batch of 5 000 and fails if the last batch's
median exceeds three times the first's. Apple M2 Pro, 12 cores, 16 GiB, Rust 1.98.0, `--release`,
on a host shared with other builds (load average 80–98 during the runs, so the means are noisy and
the medians decide).

| | First batch (0–5 000) | Last batch (45 000–50 000) | Last / first | 50 000 opens |
|---|---:|---:|---:|---:|
| Before (scan; mean per open) | 30 µs | 402 µs | 13.2 | 9.9 s |
| After (index; median per open, three runs) | 11 µs | 13 µs | 1.15–1.18 | 2.9–4.8 s |

Before, the time per open grew linearly with the credentials in the book (30, 66, 101, 136, 171,
205, 241, 280, 342, 402 µs over the ten batches), and the check failed. After, it is flat. At
250 000 owned accounts (the size of `F-014`'s opening run, which used delegated custody to avoid
the scan) the index run took 13.0 s, mean per open 44–58 µs in every batch (last/first 1.09),
702 MB resident.
