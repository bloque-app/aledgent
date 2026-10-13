<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-013 — implementation notes

Written while implementing `F-013` (issue #13), 2026-09-23. The plan's proposals were followed as
accepted: the audit store's form (append-only, segmented, hash-chained, its own media and
credential), the evidence rules of `resolve_operation`, freeze as a status that refuses debits and
lets credits land, and `Needs-Review → Reversed` as a reversal of the `Reserve` posting under the
two-person approval. Where the plan was silent, the more conservative behaviour was chosen (refuse
rather than allow, keep reserved rather than release); each such choice is listed here for review.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/audit/` | `AuditRecord`, its canonical encoding and hash chain; `AuditTrail` (the trail's rules and read API), `AuditSink`; emission inside the commit (`emit.rs`) (T-013-01, T-013-02) |
| `crates/ledger-core/src/operator.rs` | `OperatorOpKind`, `OperatorGrant`, `PendingReversal`, `Incident`, and their store rules (`OpsState`) |
| `crates/ledger-core/src/state.rs` | `Write::Audit`, `Write::Ops`, the audit log and operator records in the service store; one audit record derived per batch |
| `crates/ledger-core/src/ledger/auditing.rs` | `ship_audit`, `check_audit_head` (T-013-01, T-013-02) |
| `crates/ledger-core/src/ledger/operator/` | the frame, operator grants, privileged reads and the account's action log (`mod.rs`, T-013-03); freeze (`freeze.rs`, T-013-06); registry and issuance (`registry.rs`, T-013-07); adapter support (`adapter.rs`, T-013-08); `resolve_operation` (`resolution.rs`, T-013-04); the reversal pair (`reversal.rs`, T-013-05) |
| `crates/ledger-core/src/ledger/lifecycle.rs` | `resume` runs through the frame (closes `F-007`'s `TODO(F-013)`) |
| `crates/ledger-core/src/ledger/settlement.rs` (`F-010`'s, additive) | `resolution_settling`, `release_evidenced`, `non_inclusion_proven`: what `F-010`'s records establish for a resolution; `supported` delegates to `require_supported` |
| `crates/ledger-kernel/src/posting.rs`, `build.rs`, `book/mod.rs` | `PostingDraft::mirror_of`: a `void_of` mirror with the original's blindings; `TransferAlreadyVoided` classified `Duplicate` |
| `crates/audit-store/` | the audit store for the in-process host (T-013-02) |
| `crates/ledger-core/tests/audit_trail.rs` | the audit property test (T-013-01) |
| `crates/ledger-core/tests/privileged_reads.rs` | the committed count of the privileged-read mechanism's places (`REQ-PV-4`) |
| `crates/ledger-core/tests/api_surface.rs`, `ledger.rs` doctests | the write-surface check (T-013-09) |
| `crates/conformance/src/ops.rs`, `cases/f013.rs` | `OperatorPort` over the service port, and every `F-013` case |

## Amendment 0006 (ruled 2026-09-29)

- **Items 1 and 4: a reversal is an operation of its own.** `propose_reversal` opens the reversal
  operation under the proposal's reference (`OperationKind::Reversal`, `KindClass::Reversal`),
  decided at once into `Accepted` with the review service level as its deadline, and raises the
  incident. `approve_reversal`, by a **different principal under its own `ApproveReversal`
  authority** (maker-checker: two principals, two authorities, `REQ-OS-1`, `REQ-OS-3`), settles it
  (`Accepted → Settled`, `Trigger::ApproveReversal`) with the one `Reversal` posting. The reversed
  operation **stays `Settled`**. Unapproved past its deadline, the reversal operation goes to
  `Needs-Review` like any other, and an operator resolves it to `Released`: nothing was posted.
  `OperationKind::Reversal` is never grantable: `grant_authority` refuses a grant naming it.
  - At most one reversal per posting (`REQ-PO-3`) stays at the kernel: the posting's identity is
    `(reversed operation, Reversal, 0)`, so the kernel refuses a second for ever, and a proposal for
    an operation already reversed is refused up front (`ERR-InvalidTransition`). The posting's
    `operation` is therefore the reversed operation's reference; the reversal operation records it
    in its `Settled` transition.
  - Only a `Settled` operation is reversed. The `Needs-Review → Reversed` path (the mirror of the
    `Reserve` voided in the kernel) is withdrawn with the state; `Needs-Review` resolves to
    `Settled` or `Released` only, and `Resolution::Reverse` is gone.
- **The check (items 1 and 4, refined twice by the owner on 2026-09-29; `REQ-RC-5`, `AC-K1.3`
  to `AC-K1.6`, `AC-K2.3`).** Approval is authority-based **and** check-based. `Needs-Review`
  resolves in up to three steps (`ledger/operator/resolution.rs`):
  1. **The adapter's proof, with no human** (`Trigger::ProvenOutcome`, `Ledger::resolve_by_proof`).
     A final inbox entry naming the operation's reference settles it. A proved rejection
     (`Ledger::proved_rejection`: the refusal final and the key's nonce past its transaction),
     proven non-inclusion (the inbox range final and the nonce unconsumed), or an inbound record
     orphaned releases it. It runs from `verify` (a record of it now final or orphaned) and from
     `observe_head` (its nonce decided, or its expiry passed).
  2. **Evidence one authority submits** (`submit_evidence`: a block and a transaction hash, the
     `ResolveOperation` kind, a justification, audited as `Command::SubmitEvidence`). Nothing moves.
     The adapter reads the pending submissions (`adapter_evidence`, `GET /v1/settlement/evidence`),
     fetches each on its own view of the layer (`settlement::EvidenceFetch`), reports what it found
     through its ordinary inputs, then `evidence_checked` (`POST /v1/settlement/evidence-checks`).
     If the records prove the outcome, step 1 applies it; otherwise the submission is found
     `Unprovable`. An operator may still cite a record, or non-inclusion, that the store holds
     (`resolve_operation` with `Record` or `NonInclusion`), held to the same proof.
  3. **Two principals** (`ResolutionEvidence::NoProof`), open only once a submission was found
     unprovable. The first `resolve_operation` records the proposal (`ResolutionCase.proposed`) and
     moves nothing; a second, different principal with its own kind agrees; the release carries
     `IncidentKind::ResolvedWithoutProof`. The same principal twice is `ERR-ApprovalRequired`.
     A settlement without proof is refused (`ERR-EvidenceInsufficient`): two principals only
     release, never capture (`INV-10`; `OQ-62`, ruled 2026-09-29).

  One operator without the proof gets `ERR-ReleaseUnsafe` or `ERR-EvidenceInsufficient`, and the
  operation stays in `Needs-Review`. A reversal's posting must mirror the reversed one exactly and
  whole (`reversal::mirrors`), checked at proposing and at approving; otherwise
  `ERR-EvidenceInsufficient`. Reversals of operations with an external leg are refused as before
  (`ERR-NotAuthorised`), so their leg proof has nothing to run on yet. The resolution cases are
  service leaves (`service.resolutions`), exported and re-derived like the rest (export format 6).
- **What step 2 needs from `F-027`.** An implementation of `settlement::EvidenceFetch` over the
  Kreivo extension: for each pending submission, fetch the block and transaction from its own light
  client; check that the block is final and canonical and the transaction is in it; return the inbox
  records the transaction wrote (observations), their canonical checks, and the signing keys' nonces
  at a finalised head. `settlement::evidence_inputs` turns that into the inputs the core takes, ending
  with `AdapterInput::EvidenceChecked`. It must also poll `GET /v1/settlement/evidence`. A fetch that
  fails reports nothing, and the submission stays pending. Until that is built, a submission waits,
  and step 3 cannot open.
- **Item 5: resolution of a reservation with no external leg.** A reservation, a command never
  decided or a reversal never approved now reaches `Needs-Review` past its deadline.
  `resolve_operation` releases such an operation with no evidence: nothing outside the book can
  still take effect (`REQ-OP-9`). It cannot settle one: a capture needs a destination a resolution
  does not carry.
- **Item 6: a freeze is an account status** (`REQ-LA-7`): debits refused, credits accepted, no
  posting, balances unchanged. Encumbrance stays a balance kind, for `F-024`.
- **Item 8: `ERR-Internal`.** An audit store that does not take a privileged read's record, and a
  rebuilt posting that disagrees with the kernel's stored one, are internal faults now, counted as
  their incident, no longer `ERR-NotAuthorised`.
- **Item 9: the privileged-read path** is this feature's, as built. No spec change.

## Decisions taken during implementation

### The audit record and its emission (T-013-01)

1. **Emission is the service store's rule.** `State` derives the audit record of every batch that
   carries an attribution record — its `PutRecord`, else its `PutTransition`, else its first
   lifecycle transition that names authorities — and appends it, in the same all-or-nothing
   batch, to a hash-chained outbox. So a posting cannot commit without its record, whatever the
   command, and the commands of `F-003`, `F-004` and `F-007` needed no change. One batch is one
   command, so one record. `tick`'s deadline transitions name no authority: the service's own work,
   no record.
2. **`Write::Audit(AuditNote)`** adds what an attribution record does not carry — the
   justification, extra subjects, a class — to the batch's record, or stands alone as a record: a
   refused operator command, a privileged read. A standalone note without an action, a class or an
   authority is refused by the store (`INV-20`).
3. **The record's shape.** `authorities` is a list, as `OperationRecord`'s is: the first is the
   initiator's (`principal()`, `on_behalf_of()`, `authority_used()`), the others the authorities it
   also relied on (the issuance and the account authority of a `redeem`, the proposer beside the
   approver of a reversal). The credential is named by its identifier, never its verifier. Two
   subject kinds beyond the plan's: `Review` (a registry change's reviewed-change reference) and
   `Evidence` (the digest of the coordinate an operator cited). Identifiers are encoded in their
   surface form, so a record decodes on its own.
4. **Refusals of operator commands are audited**, as `Refused(ERR-*)` records: an operator's
   failed attempt is an intervention attempt. A refused ordinary command still writes nothing
   (`F-007` decision 8).
5. **`Command` gained the operator commands**, each with a stable code and a class
   (`AuditClass::of`, exhaustive). A command another feature adds must be classified there.
6. **Justification text** lives in the record: in the service store's outbox until the audit store
   acknowledges it, then only in the audit store. It enters no book leaf, identifier or anchored
   value. `Justification` is never blank (`ERR-JustificationRequired`) and never printed by `Debug`.

### The audit store (T-013-02)

7. **`crates/audit-store`**, the plan's first candidate home. Frames `[length][record][hash]` in
   `seg-NNNNNNNNNN.log` files opened only to append; a `HEAD` file rewritten after each append;
   the writer credential stored as its hash only. Opening re-hashes every frame and walks the chain;
   fewer records than `HEAD` is `Truncated`, more than one past it is `Unexpected`, a changed record
   is a corrupt frame or a chain break. A directory inside the book's, or containing it, is refused
   (`SameMedia`): that is the in-process meaning of "separate media"; a deployment also keeps the
   book's backups elsewhere.
8. **Retention.** The store never removes a record, so any retention period `NFR-6` configures is
   met; expiring old segments is an operations task (`F-018`).
9. **Shipping** is at least once: a record leaves the outbox only once the sink took it, and the
   sink takes a record it already holds unchanged. `check_audit_head` compares the sink's head with
   the last acknowledged record and the outbox; a sink behind or different raises an `AuditGap`
   incident.

### The operator frame (T-013-03)

10. **Operator-surface grants are their own record**, `OperatorGrant`, not `F-003`'s `Grant`:
    `F-003`'s operator grants are authority over accounts for value-moving kinds; these are
    authority over `CTR-OPS` commands. They share the grant identifier space, and the store refuses
    a collision. The operator's organisation identity makes and revokes them with its policy
    authority (`DEP-8`), with a justification, and holds none itself until it grants itself one.
11. **No catch-all.** `OperatorOpKind` has no "all" variant, and a grant naming every kind is
    `ERR-UnboundedGrant` (the store refuses it too): with every kind it has no bound in the one
    dimension it has (`REQ-OS-1`, `REQ-AU-5`).
12. **An operator acts with an unscoped credential**: a credential limited to some of its
    identity's accounts is not an operator's (`ERR-NotAuthorised`).
13. **Order of refusals**: the kind, then the justification, then the command's own checks
    (custody, state, evidence). The request's digest covers the justification, under the
    `Operator` surface.
14. **`resume` is an operator command** (kind `Resume`): it was `F-007`'s, under the policy
    authority with no record of its own. It now records, is idempotent on its client key, and
    audits refusals. `F-007`'s unit test grants the kind; `requests::resume` is gone.

### Freeze (T-013-06)

15. **Freezing a frozen account, or unfreezing an active one,** is accepted, changes nothing and is
    recorded. A closing or closed account is `ERR-AccountClosed`. Owned custody is
    `ERR-NotAuthorised` (`REQ-AU-6`), and so is the issuance counterparty.
16. **Credits land on a frozen account** (`INV-9`): `issue` and `capture` destinations now need only
    `LedgerAccount::require_creditable` (not closing or closed). `F-008`'s `transfer` still refuses
    a frozen destination: SPEC §8.1 makes "both accounts active" its pre-condition, and a refused
    transfer discards nothing (the payer keeps the value). The owner may want transfers into a
    frozen account to land too; that is a change to §8.1.
17. **A capture or release of a reservation whose payer is frozen** is refused (`ERR-AccountFrozen`),
    and the value stays reserved (`F-004` decision 10, the conservative reading of the plan's open
    point).

### Registry and issuance (T-013-07)

18. **Registering an asset again**, at any precision, is `ERR-NotAuthorised`: SPEC §10 names no
    error for it, and no authority covers it (`INV-24`). A precision above the maximum is
    `ERR-PrecisionExceeded`. A review reference of zeroes names no review: `ERR-NotAuthorised`.
19. **`deprecate_asset`** is added under its own kind: `REQ-AS-2` makes changing an asset a reviewed,
    recorded change, and `F-003` deferred it here.
20. **`grant_issuance`** makes root issuance grants only (no parent), with the per-window and asset
    bounds `F-003` requires. `F-003`'s `grant_authority` still lets the operator's policy authority
    make issuance grants without a justification: see Open points.

### Adapter support (T-013-08)

21. **One declaration, `F-010`'s.** `declare_adapter_support` writes `F-010`'s
    `SupportDeclaration` (`(operation kind, asset)` pairs) in the settlement store, the record
    `withdraw` and `submit_outbound` read. `F-010`'s `declare_support`, by the authoritative source
    at start-up (`F-016`), writes the same record; each replaces the last. Only `Issue` and
    `Redeem` may be declared (the kinds with an external leg), else `ERR-NotAuthorised`; every asset
    must be registered. `require_supported` is the one check: `F-010`'s `supported`, which
    `withdraw` and `submit_outbound` call, delegates to it. Stated plainly: the adapter's own
    declaration is not an operator command and leaves no audit record; it is `F-010`'s evidence
    path, from the authoritative source only.

### Resolution (T-013-04)

22. **Evidence is `F-010`'s settlement store.** The operator cites a coordinate the store
    ingested (`ResolutionEvidence::Record`) or non-inclusion under the last finalised head
    (`ResolutionEvidence::NonInclusion`). The operator cannot make a record final: only a canonical
    check from the authoritative source does. Settling builds its posting from a `FinalityWitness`
    (`F-010`'s capability for `INV-10`), on a final record associated with the operation of the
    event that settles its leg; an executed amount that differs is posted in the same chain and
    raised (`ERR-AmountMismatch`). Releasing needs a final `WithdrawRefused` of the instruction with
    no record of it executing (amendment 0006 item 3's route, as `F-010` notes it), proven
    non-inclusion, or an inbound record orphaned.
23. **The frame's custody rule applies to the payer** whose reservation a resolution ends: an
    owned-custody payer's operation cannot be resolved by the operator (`ERR-NotAuthorised`). See
    Open points.
24. **Every operation in `Needs-Review` has an external leg** (§5.6 reaches it only from
    `Submitted`, `Observed` and `Expired`). An inbound arrival settles by resolution with `Issue`
    on its final deposit record, into the account it was addressed to; its destination must be
    open (`F-010` decision 9).

### Reversal (T-013-05)

25. **Under `DEC-2` Option A a captured reservation's mirror cannot be posted**: the capture closed
    the holding position, and the kernel refuses any entry on a closed account. The plan's chain —
    the mirror of the capture, then the mirror of the reserve — is posted as one `Reversal`
    posting: the capture's mirror with the holding replaced by the payer, the two holding legs,
    which cancel, left out. It is complete (every entry of the capture is reversed, from the book's
    own records) but carries no `void_of`; at most one reversal per operation holds at the kernel
    by the posting's identity `(operation, Reversal, 0)` (`REQ-KER-6`). Where a mirror can be
    posted — the `Reserve` of a `Needs-Review` operation, and `F-008`'s transfers — it is a
    `void_of` mirror, and the kernel refuses a second void of the same posting for ever
    (`TransferAlreadyVoided`).
26. **No reversal moves value through the issuance counterparty**: a settled `Redeem` or `Issue` is
    `ERR-NotAuthorised` to reverse, as no authority reaches the issuance counterparty outside `issue`
    and `redeem` (`F-003`). A correction there is an `issue` or a `redeem`.
27. **A settled `Transfer`** (`F-008`) is `ERR-InvalidTransition` to reverse: its legs' amounts are
    not in the service store's records, so its postings cannot be rebuilt and checked against the
    kernel (`TODO(F-008)`: record the legs; `reversal_of` then mirrors each leg's posting with
    `void_of`, ordinals `0..N`).
28. **Every rebuilt original is checked against the kernel's stored transfer**, entry by entry:
    accounts, sides and commitments. `reversal_of` rebuilds through `F-004`'s own seam on a scratch
    index, so the posting shapes stay `F-004`'s.
29. **The frame's custody rule applies to every account a reversal takes value from**, which must
    also accept a debit (a frozen destination is `ERR-AccountFrozen`). A proposal checks all of
    this at proposing, so a proposal that could never be approved is refused at once.
30. **Several proposals may be pending for one operation**; the first approved posts the reversal
    (since amendment 0006 the reversed operation stays `Settled`, and later ones are refused at the
    kernel or up front), and the
    others then find it terminal. A kernel refusal at approval (the destination spent the value,
    `ERR-InsufficientAvailable`) moves nothing and leaves the proposal pending.
31. **The reversal of a `Reserve`** carried `CLOSING_DEBIT` and closed the reservation in the index
    as `Released`. Withdrawn with `Needs-Review → Reversed` (amendment 0006 item 4).

### Privileged reads (amendment 0006 item 9)

32. **One mechanism**, `Ledger::privileged_read` (private): the `PrivilegedRead` kind, a
    justification, a `PrivilegedRead` audit record committed and **shipped to the audit store
    before any data is returned**; if the store does not take it, the read is refused and the
    record withdrawn. One place uses it, `privileged_balances`. `tests/privileged_reads.rs` counts
    the places against a committed list: a new place fails the build.
33. **The library's host-level readers** — `Ledger::state`, `Ledger::book`, `balance_view` — are not
    principal reads: they have no caller. `F-011`'s query surface must build on `reach_*` or on the
    privileged-read mechanism, never on them.

### The write surface (T-013-09)

34. **`tests/api_surface.rs`** lists every public `&mut self` function of `Ledger`, `F-008`'s
    `transfer` and `F-010`'s commands and settlement-boundary calls included. A feature that adds
    one adds it to that list in review; the test fails until it does.
35. **`resume` is on the frame for `F-010` too**: its test of `resume` from `Final` now grants the
    operator the `Resume` kind; `resume_settling`'s refusal is audited like any other.
36. **`F-010`'s commands are classified**: `withdraw` and `submit_outbound` are value-moving,
    `configure_settlement` an authority change (it names the authoritative source).

## Conformance

- 11 run cases in `crates/conformance/src/cases/f013.rs`, all green on both in-process stores, and
  under the `m1` profile: `ERR-AccountFrozen`, `ERR-JustificationRequired`,
  `ERR-ApprovalRequired`, `AC-K1.1`, `AC-K1.2`, `AC-K2.1`, `AC-K2.2`, `AC-K4.1`, `AC-K4.2`, and
  `INV-9` and `INV-20` at the service layer (§9's).
- `AC-K4.1` and `AC-K4.2` make the correction with `reserve` and `capture` under `F-003`'s operator
  authority; `TODO(F-008)`: the same with `transfer`.
- The kernel half of `AC-K2.2` — a second reversal of one posting refused by the kernel — needs the
  book's own port, and runs as unit tests: `ledger-kernel`'s `req_po_3_a_mirror_voids_its_original_once`
  and `ledger-core`'s `ac_k2_2_a_reversal_is_a_new_posting_and_a_second_one_fails_at_the_kernel`.

## Open points for the owner

- **Issuance grants off the frame.** `F-003`'s `grant_authority` still lets the operator's policy
  authority make and revoke issuance grants with no justification. `REQ-OS-1` wants every operator
  capability on the frame. Retiring that path changes `F-003`'s cases and every feature's fixtures
  that fund accounts; it is left for the owner.
- **Owned custody in `Needs-Review`.** With the frame's custody rule, nobody can resolve an
  owned-custody payer's operation in `Needs-Review`: the value stays reserved. `REQ-RC-1` wants a
  resolution path for every non-terminal state. Resolution on final evidence moves no value at the
  operator's discretion; the owner may want the custody rule not to apply to it.
- **Reversal after the value was spent** has no path but new operations (`REQ-PO-3` forbids a
  partial reversal); operators should be told.
- **`Adjustment`** stays unused: corrections are ordinary commands (amendment 0006 item 7).
- **`set_identity_status`** is still the policy authority's with no justification (`F-003`
  decision 13); it is not in `REQ-OS-2`'s list, so it was not moved onto the frame.

## Deferred

- `TODO(F-008)`: reversing a settled transfer (its legs recorded, then a `void_of` mirror per leg);
  the correction cases by `transfer`.
- `F-011`: the account's action log on the query surface (`account_actions` is its source).
- `F-012`: recovery commands on the frame (steps 2, 3 and 5; the recovery authority stays theirs).
- `F-016`: the adapter's own declaration at start-up (through `F-010`'s `declare_support`). `F-017`: disclosure records
  (`AuditClass::Disclosure`). `F-018`: paging on incidents, audit store retention.
- `F-005`: the outbox rides the Agent's delta; the audit worker ships it.
- Encumbrance commands: `DEF-11`, `F-024`.

## T-013-13: shipping the trail from the core (2026-10-04)

`F-032`'s finding G-02: nothing called `Ledger::ship_audit` in ingress or the core host, so the audit
outbox grew in memory for the life of the book, and `NFR-6`'s independent retention was not met.

- **Where it ships from.** A shipper thread beside the core (`ingress::audit_ship`), in the process
  that holds the core: `aledgent-core-host serve` for the Agent core, ingress for the in-process core.
  Both read `audit_store` from the deployment's configuration and the writer credential from
  `ALEDGENT_AUDIT_WRITER_CREDENTIAL` (`opsctl`'s variable).
- **Exactly once, in order.** The shipper reads what the core committed (through the gate: never an
  input the Agent has not committed), appends it in one batch, and only then acknowledges it to the
  core (`Ledger::acknowledge_audit`, a host-side trim that is not a call and changes no root). The
  store takes only the next record, linked to the one before; a record it already holds is accepted
  again and appended nothing. Any other record is refused.
- **Restarts.** Every rebuild of the core's replica (a start, or a rebuild after a refused input) drops,
  as it replays, the records through the store's head (`host::replay_audited`), checking the record
  at the head against the store's hash. A store ahead of the core (a core restored from an older
  backup) refuses the core's records, and shipping stops.
- **Back-pressure.** The core refuses new inputs (`Unavailable`; they stay `Received`) while
  `max_unshipped` records wait (65 536 by default). Conservative reading: no decision without its
  record retained apart; the alternative, deciding on and re-deriving records from the archive later,
  is not taken.
- **The store's writer.** `audit_store::AuditAppender` holds the chain's head and the hashes of the
  most recent 65 536 records, never the records. Opening it re-reads every segment, one at a time. A
  batch writes its intent (`HEAD.pending`) before its frames, so a crash between a batch's frames and
  its `HEAD` opens cleanly, and records beyond an intent are still `Unexpected`.

**Trade-off, stated plainly.** The shipper runs as the core's OS user with the writer credential in the
same process. A compromised core host can append to the trail; it cannot rewrite what the store already
holds without the chain and `HEAD` showing it. A shipper process under another OS user is the stronger
form, and belongs with the key boundary (`F-035`).

**Deferred.** A torn final frame (a crash in the middle of one `write`) still opens as `Corrupt`, as it
did before; the store never truncates itself. Moving segments past the volume to object storage under
Object Lock (`NFR-6` retention) is an operations task.
