<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-017 — implementation notes

Written while implementing `F-017` (issue #17), 2026-09-23. The plan's proposals were followed as
accepted: the anchor counter on the settlement layer with the entry sequence inside the commitment,
the clerk key published on the pallet's instance record (`attestation_key`), the verifier at
`crates/verify`, `HoldsAtLeast` naming its balance kind, and `FlowWithin` windows as ranges of entry
sequence. Where the plan was silent, the more conservative behaviour was chosen (refuse rather than
allow); each such choice is listed here for review.

## F-005 and issue #38

The plan's homes are `crates/ledger-core`, `agents/core` and a verifier crate. `agents/core` is
`F-005`'s, and `F-005` is blocked on the owner's decision in issue #38 (VOS Agent or native core).
Nothing here is VOS-specific and nothing depends on `F-005`: the core side is `ledger-core`, reached
through ingress's `boundary::Core` and built and tested against the in-process core
(`ingress::inprocess::InProcessCore`), exactly as `F-009` and `F-016` did.

**The clerk key's private-input path is `F-005`'s, and depends on #38.** The plan has the key reach
the core through the private-input path `F-005` builds for kernel signatures (`DEC-3`, `REQ-RT-6`).
Meanwhile the core sees it only through a narrow trait, `anchor::clerk::ClerkSigner` (its public key,
and a signature over bytes). The in-process implementation, `InProcessClerk`, holds the secret scalar
with no `Clone`, a redacted `Debug`, and zeroisation on drop; the in-process core holds one behind an
`Arc` (`InProcessCore::with_clerk`), passed to `issue_disclosure` as an argument, never stored in the
service store, the journal of inputs, an export or a reply. The ingress binary reads it once from
`ALEDGENT_CLERK_KEY` (optional; without it no disclosure is issued), as it reads the core's root.
Whatever #38 rules, the core's side is the trait.

## What landed

| Where | What |
|---|---|
| `crates/verify` (new, `DEC-15`) | The anchor commitment; the disclosure envelope and its one canonical encoding; CipherClerk's four claims parsed from their wire form; the clerk's Schnorr verified with Ristretto arithmetic; `verify()` and its `Report`; `AnchorSource` with `MemAnchors` (fake) and `rpc::RpcAnchors` (feature `rpc`); `cli` and the `aledgent-verify` binary (feature `cli`) |
| `crates/ledger-core/src/anchor/` | `Checkpoint`, `AnchorInstruction` (a number and 32 bytes, a `const` build gate), the anchor store and its rules, `PositionCut`, `Comparison`, incidents; `clerk` (`ClerkSigner`, `InProcessClerk`); `disclosure` (the assertions and the request) |
| `crates/ledger-core/src/ledger/anchoring.rs` | Creation (on `tick` and `anchor_checkpoint`), the adapter's outbox read, acknowledgement and re-emission, finality from the canonical check, `final_checkpoint`, the settlement comparison (`record_positions`, `compare_settlement`), the operator's read |
| `crates/ledger-core/src/ledger/disclosure.rs` | `issue_disclosure`, the truth checks, the envelope, `verify_disclosure` |
| `crates/ledger-core` (small, additive) | `Write::Anchor`; `Observation::anchor` (`AnchorFact`); `LifecycleConfig::anchoring`; `Ticked::checkpoint`; `Command::{AnchorCheckpoint, CompareSettlement, IssueDisclosure}` (41–43); `OperatorOpKind::{AnchorCheckpoint, CompareSettlement, LegalDisclosure}` (14–16); `OperationKind::Disclose` (11); `AdapterInput::{AcknowledgeAnchor, AnchorNotExecuted, Positions}`; the six export sections and format version 3 |
| `services/ingress/src/anchoring.rs` and small edits | `POST /v1/disclosures`, `GET /v1/disclosures/verify`, `POST`/`GET /v1/ops/anchors`, `POST /v1/ops/settlement/comparisons`; the adapter's `GET /v1/settlement/anchor` and `POST anchor-acknowledgements`, `anchor-failures`, `positions`; the clerk key in the in-process core; anchoring in the deployment configuration; `openapi.json` regenerated |
| `services/settle` | The anchor key (`anchor_key`, `Adapter::with_anchor_signer`), the anchor step and the positions step; `Chain::sign_anchor` and `InstanceView::last_anchor` (fake and RPC); anchor rows in the store (`MemStore`, `PgStore`, `migrations/0002_anchors.sql`); the fake chain's `anchor` call |
| `crates/conformance/src/cases/f017.rs` | Every `F-017` case, run |
| `crates/conformance/src/tools/book.rs` | The random book anchors, compares and discloses, so every export section is filled |

This closes `F-014`'s `TODO(F-017)` in part (below), `F-016`'s `TODO(F-017)` (anchor instructions
through the adapter; `Anchored` records mean something), and `F-010`'s `TODO(F-017)`.

## Decisions taken during implementation

### Checkpoints (T-017-01)

1. **`entry_sequence` is the sequence of the last entry covered**, inclusive, as `REQ-PR-7` words
   it ("up to and including a stated sequence"). A checkpoint needs at least one entry; with none,
   there is nothing to anchor.
2. **The kernel root and the entry sequence are read in one committed state**, in the same call,
   with nothing written between the two reads; the checkpoint and its instruction commit in one
   batch. The store refuses a checkpoint whose commitment is not the one its fields produce, so no
   writer can put another commitment in the outbox.
3. **No checkpoint without the settlement configuration**: the commitment binds the instance, which
   the configuration names. `anchor_checkpoint` answers `ERR-SettlementLayerUnavailable` meanwhile;
   `tick` creates nothing.
4. **One checkpoint in flight**, as the plan says: the next is created only once the last is final,
   by the store's rule. The cadence is measured from the last checkpoint's creation. An
   `anchor_checkpoint` with nothing new, or with one in flight, answers the latest and creates
   nothing. Stated plainly: while an anchor cannot become final (the layer down, or a commitment
   mismatch, below), no new checkpoint is made; the anchor lag grows (`AC-J1.4`), and holders are not
   affected (`REQ-PR-9`).
5. **The anchor instruction is a number and 32 bytes**, with no other constructor than a checkpoint's
   and a `const` assertion on its size: the `INV-17` build gate for anchored values.

### Cadence and on demand (T-017-02)

6. **`Ledger::tick` creates the checkpoint**, after its deadlines, and never fails because of it.
   The interval is configuration (`AnchoringConfig::interval`, default one hour): its value is
   `OQ-11`'s.
7. **`anchor_checkpoint` is an operator command through `F-013`'s frame**: kind `AnchorCheckpoint`, a
   justification, the client key; its repeat answers the same checkpoint, as it stands now.

### Finality (T-017-03)

8. **The `Anchored` record's number and commitment travel beside the payload**, as a movement does
   (`F-010` decision 2): `Observation::anchor`. The core never parses a payload (`REQ-PO-4`). The
   adapter fills it from the decoded inbox entry.
9. **Only the canonical check that finds the record final makes the checkpoint final**, in the same
   commit, and only with the same commitment. A record of the number with another commitment raises
   `AnchorIncident::CommitmentMismatch` and the checkpoint never becomes final; the number is taken on
   the layer, so anchoring stops until an operator acts (conservative: nothing is skipped). A record of
   a number aledgent never made raises `UnknownAnchor`. A record with no anchor fact finalises nothing.
10. **Re-emission.** The adapter reports `AnchorNotExecuted` only with a proof (the era ended with the
    nonce unspent, the nonce spent by another extrinsic, or a failed dispatch); the core moves
    `Submitted → Pending` and the same `(number, commitment)` is in the outbox again, unacknowledged.
    Safe because an anchor moves no value and the pallet takes a replay as a no-op.
11. **`ERR-AnchorNotFinal`** for anything not final: pending, submitted, or a number that does not
    exist.

### Disclosures (T-017-04, T-017-05)

12. **Authority** (`REQ-DS-5`): the subject account's owner, or a **delegated** grant over it naming
    the new `OperationKind::Disclose`. Operator grants over accounts do not count: an authority the
    subject granted is the subject's own or a delegation. Refusals of reach and authority
    (`ERR-NotAuthorised`, `ERR-UnknownAccount`, `ERR-AuthorityRevoked`, `ERR-BoundExceeded`) are
    `ERR-NotDisclosable`.
13. **The legal path** is an operator's with the new operator kind `LegalDisclosure` and a
    justification, audited like any operator command (a refusal too), and **only over an account in
    delegated custody**: operator authority does not exist over owned custody (`REQ-AU-6`), and a legal
    obligation is not read as widening it. The owner may want this otherwise.
14. **The truth checks**, at the checkpoint's entry sequence, from the journal:
    - `HoldsAtLeast`: the subject's `available` after entry `S`. `reserved` and `total` are
      `ERR-DisclosureUnsupported` (accepted proposal: only `available` is one kernel layer, `DEC-2`).
    - `OperationOccurred`: the operation has a completing posting (transfer, capture, issue, redeem,
      conversion) at or below `S` touching the subject. **A reversal at or below `S` makes it false**
      (never over-assert); an opening load is `ERR-DisclosureUnsupported` (its kernel key is not the
      operation's). One `transfer_existed` claim per completing posting.
    - `OperationMoved`: what those postings delivered in the asset — the credit lines on accounts the
      same posting does not debit (so a capture's change back to the payer is not counted) — equals
      the amount exactly. One `transfer_amount_equals` claim per delivered entry, with the kernel's
      stored commitment of that entry; the verifier sums them.
    - `FlowWithin`: per posting, the net change of the subject's balances in the asset over
      `[from, to]`, `to ≤ S`; inflows are the positive nets, outflows the negative. A reservation moves
      nothing in or out. A window past the checkpoint is `ERR-DisclosureUnsupported`. The claim's bound
      is the Pedersen commitment of the public bound with a zero blinding, which the verifier recomputes.
15. **The envelope carries a list of claims**, not one: an operation of several postings or entries
    needs several. Each is CipherClerk's wire form with `state_root = kernel_root`; the envelope's
    signature covers every other field. A cross-check against CipherClerk's own `verify_*` functions
    runs in `ledger-core`'s tests.
16. **The recipient** is an identity of the book or an opaque 32-byte reference, never free text
    (`REQ-PV-3`). An identity recipient is a subject of the audit record. The recipient's principal
    gains nothing: no grant, no key, no read (`AC-I2.2`).
17. **The assurance level is configuration**: the digest and summary of the declaration `F-015`
    generates, supplied by the deployment (`LifecycleConfig::anchoring.assurance`). With none, no
    disclosure is issued (`ERR-DisclosureUnsupported`): a disclosure must carry it (`REQ-AR-5`). The
    summary a disclosure carries always starts with the label "Signed assertion. Evidence of aledgent's
    claim, not of its truth." and its kind is `SignedAssertion` (`REQ-PR-6`, `REQ-AR-6`).
18. **No randomness inside the core** (`REQ-RT-7`): the clerk's Schnorr nonce is derived from the
    secret and the message, as Ed25519 does. The signatures are CipherClerk's scheme and verify with
    `cipher_clerk::crypto::verify_signature`. CipherClerk's own `prove_*` helpers draw from `OsRng`
    behind its `signing` feature (`getrandom`), which the core does not enable; it enables
    only `disclosure`, for the claims' `public_bytes`.
19. **A repeat of `issue_disclosure`** under the same client key returns the same bytes, kept with the
    operation in the anchor store (and exported).
20. **`verify(disclosure)`** is `Ledger::verify_disclosure`, the offline verifier's checks against the
    core's own final checkpoints and clerk key; on ingress, `GET /v1/disclosures/verify` with the bytes
    as a query parameter. The access log records no query string. Any authenticated principal may
    ask.

### The verifier (T-017-06)

21. **`crates/verify` depends on nothing of aledgent's**: `blake2` and `curve25519-dalek`, `no_std`.
    The core builds the commitment and the envelope with it, so there is one format. The claims'
    wire forms are parsed there, not taken from `cipher-clerk` (which would bring `rkyv`); a test holds
    them to CipherClerk's.
22. **The clerk key**: given by the recipient, or read from the instance record; if both and they
    differ, the check fails. The adapter does not set the attestation key: the instance's admin does
    with `set_keys` (`F-019`), which is the operator's runbook (`TODO(F-018)`).
23. **The Kreivo read** (`rpc::RpcAnchors`, feature `rpc`): `state_getStorage` of
    `AledgentSettlement::Anchors(instance, number)` and `Instances(instance)` at
    `chain_getFinalizedHead`, keys hashed `Twox64Concat`, a `u32` block number. One ignored test
    against a local node (`ALEDGENT_VERIFY_RPC`, local addresses only); no public network in tests.

### The settlement comparison (T-017-07)

24. **`F-010` did not build a separate per-asset settlement-position account**: it posts a deposit's
    `Issue` and an outbound leg's `Redeem` against the issuance counterparty. The book's record of
    value held on the layer at cut `k` is therefore derived: what the settling postings of the
    operations bound to final `Deposited`/`Withdrawn` records at positions below `k` moved through the
    issuance counterparty. Issuance under issuance authority has no record on the layer and is not
    counted, which is `REQ-AS-6`'s separation. Stated plainly: the plan's design (a position account
    in the book) would let the trial balance show it; this does not, and the owner may want `F-010`'s
    postings changed.
25. **A cut is compared only when every position below it is ingested and final**
    (`ERR-EvidenceInsufficient` otherwise), or a record on its way would show as a difference.
26. **Continuously, and recorded where it matters**: every report is compared and kept as the latest;
    it is recorded in the history when it is some final anchor's comparison (`AC-J3.1`), when an
    operator asked (`compare_settlement`), or when it differs. Each new difference per asset is an
    incident (`REQ-PR-5`: an incident, not a report), not repeated while it stays the same.
27. **The adapter reports positions** only with an anchor key, once its cursor has read the whole
    finalized inbox, when a comparison is due or the positions changed. The daily schedule and paging
    are `F-018`'s.

### The adapter and the end-to-end run (T-017-08)

28. **Anchors are published with a key of their own** (`anchor_key`), one of the instance's keys used
    for nothing else, so its nonces are the anchors' alone and withdrawals' lanes are untouched. The
    start-up assertions check it is an instance key. Without it, the adapter neither anchors nor
    reports positions: `F-016`'s behaviour is unchanged.
29. **Stored before broadcast** (`REQ-SA-6`): an anchor row per number with its live extrinsic and the
    ones that provably did not execute; a restarted adapter broadcasts the same bytes.
30. **The end-to-end run on a local Kreivo is not run**, for the reasons `F-016`'s notes give (no
    stable2606 binaries, the runtime build over the disk budget, no transaction-extension `Config` for
    Kreivo). What runs instead: the thirteen conformance cases and `services/settle/tests/anchoring.rs`
    against the fake layer, which runs the pallet's `anchor` call as `F-019` wrote it; and the
    verifier with every aledgent process stopped, on what the layer holds.

### Shared files

31. `ingress`: `Outcome` allows `clippy::large_enum_variant` (one more boxed field in `CommandResult`
    crossed the threshold; boxing the result would change every constructor). `testkit.rs`: three
    constants and two helpers, the anchoring configuration in `genesis`, and the clerk in `start_on`.
32. `F-014`: the anchor store is six sections and an observation carries its anchor fact, so
    `FORMAT_VERSION` is 3; the observable-state comparison reads anchoring too.

## Amendment 0006

- **Item 2 (the assurance level's layers).** Ruled 2026-09-29: §9 has a V0 column. A disclosure
  carries the digest and summary of whatever declaration the deployment configured, which `F-015`
  generates from that column. Nothing here declares or re-judges an
  invariant's layer: `INV-19` is registered at the service layer, which §9 names; `INV-17` at the
  build gate and the store, as `F-003` left it.

## Open points for the owner

- **Guardrail 13 and `DEC-3`.** The clerk key is in the process that holds the book (the in-process
  core), as the core's key-ring root already is; `DEC-3` puts it in the core. Whether a signer outside
  the process (a KMS) is wanted is `F-005`'s question with #38.
- **`F-014`'s `Anchors` stays a stand-in.** The export checks the **book root** (kernel and service) at
  a posting sequence; the anchor commits to the **kernel root** only (`INV-19`, "nothing else"), and
  the service store holds the anchor records themselves, so a checkpoint cannot answer for a book root.
  The export's check could be re-pointed at the kernel root of a final checkpoint; that changes
  `F-014`'s tests and is left for review.
- **Plan open point 3**: the composite root also commits to account records and the kernel's
  bookkeeping sets; they are functions of the book up to `S` and of account openings, as the plan
  reads it.
- **A reversed operation** is refused as not having occurred (decision 14); the owner may prefer
  "occurred" to mean "was posted".
- **Legal disclosures of owned-custody accounts** are refused (decision 13).

## Tests

| Test | Covers |
|---|---|
| `crates/verify` (16, 1 ignored): `the_commitment_*`, `every_shape_is_cipher_clerks_wire_form`, `a_cipher_clerk_signature_verifies_here…`, `ac_i2_1_…`, `t_017_06_a_tampered_disclosure_fails`, `ac_j1_3_…`, `ac_i2_3_…`, `every_shape_is_checked_for_consistency`, `t_017_06_the_cli_passes…`, the RPC key and record encodings | `REQ-DS-3`, `REQ-PR-4`, `AC-I2.1`, `AC-I2.3`, `AC-J1.3`, T-017-06 |
| `ledger-core` `anchor::tests` (5), `anchor::clerk::tests` (2) | the store's rules; the build gate; the clerk's signature and hygiene (`NFR-8`) |
| `ledger-core` `ledger::tests::anchoring` (20): `inv_19_…` (property, 16 random books), `ac_j1_1_…`, `req_pr_1_…`, `ac_j1_4_…`, `ac_j1_3_…` (2), `t_017_03_…`, `req_ds_1_…`, `req_ds_4_…`, `err_disclosure_unsupported_…`, `err_anchor_not_final_…`, `req_ds_5_…` (2), `ac_i2_2_…`, `req_op_10_…`, `t_017_06_…`, `ac_j3_2_…`, `ac_j3_1_…` | T-017-01 to T-017-07 at the service layer |
| `conformance` `f017.rs` (13 cases × 2 targets) | `INV-17` (build gate), `INV-19`, `ERR-DisclosureUnsupported`, `ERR-DisclosureFalse`, `ERR-NotDisclosable`, `ERR-AnchorNotFinal`, `AC-I1.1`, `AC-J1.1`–`AC-J1.4`, `AC-J3.1`, `AC-J3.2` |
| `services/settle/tests/anchoring.rs` (5) | `AC-I2.1`, `AC-I3.1` with every aledgent process stopped; `AC-I2.2`, `AC-I3.2`; `AC-I2.3`, `AC-I3.3`; re-emission; `REQ-SA-6` across a restart; no anchor key |
| `ingress` `anchoring::tests` (2), `config` | the wire forms; the deployment's anchoring configuration |
| `F-014`'s export tests | the six sections filled and round-tripped (`AC-K5.2`) |

## Deferred

- `TODO(F-005)`: the clerk key through the private-input path (#38).
- `TODO(F-016)`: a subxt `Config` naming Kreivo's transaction extensions, for the anchor call as for
  withdrawals; the run on a local Kreivo (`services/settle/tests/live_node.rs` does not anchor yet).
- `TODO(F-018)`: paging on anchoring's incidents and on a stuck anchor, the daily comparison, setting
  the instance's attestation key in the runbook.
- `F-023`: disclosures a recipient can check without trusting aledgent (`DEF-3`).
