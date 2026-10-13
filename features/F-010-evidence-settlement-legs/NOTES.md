<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-010 — implementation notes

Written while implementing `F-010` (issue #10), 2026-09-23. The plan's proposals were followed as
accepted: the simulated settlement layer as a `sim` feature of `ledger-core`, the posting rule for
an amount mismatch (§9), and a deterministic account reference. Where the plan was silent, the more
conservative behaviour was chosen (refuse rather than allow, keep reserved rather than release);
each such choice is listed here for review.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/settlement/mod.rs` | The `CTR-SET` types: `Coordinate`, `Observation`, `CanonicalCheck`, `FinalisedHead`, `Verdict`, `Verified`, `SupportDeclaration`, `Destination`, `OutboundInstruction`, `SettlementConfig`, `Gap`, `Incident`, `RescanReport` (T-010-02) |
| `crates/ledger-core/src/settlement/store.rs` | The settlement store, part of the service store: the unique coordinate set, one final record per position, the outbox of instructions, the finalised head, gaps, incidents (T-010-03, T-010-09) |
| `crates/ledger-core/src/settlement/port.rs` | `SettlementPort`, the object-safe surface the suite and the driver use |
| `crates/ledger-core/src/settlement/sim.rs`, `driver.rs` | The simulated settlement layer and the driver that plays the adapter, feature `sim` (T-010-01) |
| `crates/ledger-core/src/ledger/settlement.rs` | The engine: `configure_settlement`, `declare_support`, `withdraw`, `submit_outbound`, `acknowledge`, `record_observation`, `verify`, `observe_head`, `record_gap`, `rescan`; `FinalityWitness` (T-010-03 to T-010-09) |
| `crates/ledger-core/src/reservation/mod.rs` | `ReservationHandle`; `ReservationBook::settle_outbound_for` (Redeem for less, the rest back to the payer) |
| `crates/ledger-core/src/state.rs` | `Write::Settlement`, `StoreRefusal::Settlement`, `State::settlement()`; `Command::{Withdraw, SubmitOutbound, ConfigureSettlement}` |
| `crates/ledger-core/src/ledger/lifecycle.rs` | `resume` from `Final` builds the settling posting from the finality witness (`Redeem` and now `Issue`) |
| `crates/ledger-core/src/ledger/tests/settlement.rs` | Engine tests: kernel replay, the outbox after a restart, the deadline check, re-inclusion, re-scan, gaps, late execution, `resume` from `Final`, item 3 |
| `crates/ledger-core/tests/settlement_sim.rs` | The property run: 10 000 seeded histories (T-010-10) |
| `crates/conformance/src/cases/f010.rs` | Every `F-010` case, and the property run the suite and the test above share (T-010-10) |
| `crates/conformance/src/harness.rs` | `ServicePort::settlement_port()` |

## The coordinate format (agreed with `F-016` and `F-019`)

**A coordinate is `(instance: u32, seq: u64, digest: [u8; 32])`**: the deployment's instance on
the settlement layer, the position in its sequenced inbox, and the digest of the inbox entry at
that position. The entry includes the height of the block that wrote it, so the digest does too.

- This is `F-016`'s proposal (`(instance, inbox seq, hash of the entry)`, the entry including its
  block number) over `F-019`'s storage (`Inbox: (InstanceId, InboxSeq) → InboxEntry { at, event }`).
  `F-019` (in parallel, `pallets/aledgent-settlement`) stores exactly those fields; its notes do not
  state the coordinate, and need not: the pallet only has to keep `at` inside the entry, which it
  does.
- A record re-written at the same position by a fork, or the same event re-included at another
  height, is another coordinate. So `Observed → Orphaned` is reachable from the inbox
  (`REQ-IN-4`), as `F-016` wanted.
- **The height travels beside the coordinate** (`Observation::at`), because the finality point
  (`CanonicalCheck::finalised_through`) and the expiry are heights. It is not part of the identity:
  deduplication (`INV-12`) is on `(instance, seq, digest)`.
- **For `F-016`:** the digest is the adapter's to compute; the library treats it as opaque. The
  natural choice is `blake2b-256` of the SCALE encoding of the pallet's `InboxEntry`. The
  simulator's digest is its own encoding of the same fields, not byte-compatible with SCALE.
- `F-016`'s `NotIncluded` observation at `(instance, signer, N, block hash)` is not needed by the
  library: non-inclusion is proven from the inbox itself (below). If `F-016` still wants it, it is
  another observation kind to add; it would not change the coordinate of inbox records.

## Amendment 0006 (ruled 2026-09-29)

- **Item 3: a proved rejection releases at once.** A `WithdrawRefused` record is ingested,
  associated with its operation and reported as `Incident::FinalFailure` (so an operator sees
  why), and moves nothing while provisional. It is a **proved rejection** once the record is final
  (a canonical check) **and** the signing key's nonce is consumed past the instruction's
  transaction (a finalised head, the owner's ruling of 2026-09-24), with no record of the
  instruction executing, final or pending (`Ledger::proved_rejection`). Then
  `Ledger::release_rejected` moves the operation to `Released` with its `Release`, at once and with
  no review, from `Reserved`, `Submitted`, `Observed`, `Orphaned` or `Expired`
  (`Trigger::ProvedRejection`). It runs from `verify` when the record becomes final, and from
  `observe_head` when the nonce is decided, before the head disposes of any expired leg, so a final
  refusal is never read as "non-inclusion unprovable". A refusal whatever its reason counts,
  `F-026`'s coming `Refusal::CallFailed` included. An operation already in `Needs-Review` is left to
  the operator, who resolves it citing the record (`F-013`). Tests:
  `req_op_9_a_proved_rejection_releases_at_once`, `req_op_9_a_refusal_without_its_proof_releases_nothing`,
  and the conformance case `ac_e1_4_a_proved_rejection_releases_at_once__service`.
- **Item 5: every non-terminal state has a deadline row to `Needs-Review`.** An outbound leg
  orphaned after its expiry, or left at `Final` by a refused settling posting, moves to
  `Needs-Review` past its deadline instead of waiting in the queue under its own state; `resume` is
  the operator's forward path before then.
- **Item 11: a caller's release without proof** moves the leg to `Needs-Review` and releases
  nothing. `ac_e1_4_proven_non_inclusion_against_the_simulated_layer__service` no longer asks for a
  release before the proof arrives: that request would now hand the operation to a human.

## Decisions taken during implementation

### Boundary and verdicts

1. **A fourth input, `FinalisedHead { height, next_seq }`.** The plan's `CanonicalCheck` carries
   `finalised_through` per coordinate. Expiry and non-inclusion need two more facts that no
   coordinate carries: the finalised height when no record exists, and the inbox length at that
   height. `observe_head` takes them once per cycle. It is a fact about the layer, from the
   authoritative source only; a head from another source, or one behind the last, changes nothing.
2. **`Movement { asset, amount }` beside the opaque payload.** The plan's `Observation` has no
   amount; a deposit's credit and `ERR-AmountMismatch` need one. The adapter maps the layer's asset
   and units to the book's.
3. **Verdicts and results.** `record_observation` returns `Ok(Applied(state) | Ignored)`, or
   `Err(ERR-ObservationDuplicate)` (the verdict `AlreadyApplied`, nothing written) or
   `Err(ERR-ObservationUnassociated)` (the verdict `NotAssociated`). `Verdict::of` maps a result to
   its verdict for the adapter (`REQ-SI-5`). An `Err` means no transition; an unassociated or
   unusable record is still ingested, counted and raised, so its position counts towards
   completeness.
4. **`ERR-EvidenceInsufficient`** for: a record of another instance (nothing written); a check of a
   coordinate never ingested; a movement that does not fit its operation (zero amount, another
   asset, an operation that is not an outbound leg: ingested, `Incident::Unusable`); a check that
   says a record already final stopped matching; an acknowledgement that does not name the
   instruction's digest.
5. **An unknown or deprecated asset in a deposit is unassociated**: the value is on the layer with
   no destination in the book. Recorded and raised; nothing is issued (the plan's meanwhile for
   unassociated deposits).
6. **Corroborating observations are kept** per `(source, coordinate)`; a repeat of one is
   `ERR-ObservationDuplicate`. They never write to the book (`REQ-IN-6`).

### Finality, orphaning and `INV-12`

7. **One final record per position**, a store rule the plan did not list. Two coordinates at one
   `(instance, seq)` can be ingested (a fork), but at most one is ever marked final, and final and
   orphaned exclude each other and absorb. A buggy or hostile adapter cannot make two records at one
   position both settle.
8. **The kernel's `external_id` stays `F-001`'s** (`(operation, kind, ordinal)`), as `F-007`
   decided, rather than the plan's `H("aledgent/coord/v1", coordinate)`. An unoriginated deposit's
   operation reference is `inbound_reference(coordinate)`, so the coordinate reaches the kernel's
   identity through it, and a second `Issue` for one coordinate is refused by the kernel
   (`ac_d3_2_a_replayed_settling_posting_is_refused_by_the_kernel`). An outbound leg settles once
   per operation the same way.
9. **`Observed → Final → Settled` in one commit.** If the settling posting is refused — by the
   kernel, or because the destination account is closed or frozen, which the service checks — the
   operation still moves to `Final` in a second commit, stays there, is flagged into the review
   queue, and `Incident::SettlementRefused` names the refusal (`INV-23`). A closed destination is
   treated as the kernel's refusal would be.
10. **Observations wait for their operation.** A `Withdrawn` record that arrives before the
    adapter's acknowledgement (the operation still `Reserved`) is kept and applied at the
    acknowledgement; one that arrives while the operation rests on another record is kept and
    applied if that record is orphaned. An orphaned outbound leg whose expiry is not final moves
    `Observed → Orphaned → Submitted` in one commit, and on to `Observed` at once if such a record
    is held.
11. **Late execution.** A `Withdrawn` record for an operation already `Expired` moves it to
    `Needs-Review` (`Incident::LateEvidence`); for any other state that no longer waits for one it
    is kept and raised.
12. **`REQ-IN-7`.** Observed, final and settled are three transitions, each with its own time; the
    record keeps them as three transition records rather than three fields.

### Outbound

13. **Two commands, as SPEC §8.1 has them.** `withdraw` reserves (`Received → Accepted →
    Reserved`, kind `Redeem`, class outbound); `submit_outbound` assembles the instruction from the
    reservation's handle and puts it in the outbox; the adapter's `acknowledge` moves it to
    `Submitted`. The destination is `submit_outbound`'s.
14. **Authority for a withdrawal** is `Redeem` over the payer's account, with the asset and the
    amount (bounds and windows charged once, at `withdraw`); `submit_outbound` is authorised the
    same way with no amount. No issuance authority is asked: the `Redeem` posting is made on
    evidence at finality, not by the caller. `F-003`'s `redeem` command, which moves value with no
    external leg, keeps its two authorities.
15. **The support check** runs after authority (a principal with no authority learns nothing),
    before any reservation (`ERR-UnsupportedLeg`, `REQ-SA-1`), and again at `submit_outbound`. With
    no configuration or declaration, every leg is unsupported. A reservation with no external leg
    passed to `submit_outbound` is `ERR-UnsupportedLeg` too.
16. **Expiry and deadline.** The expiry is the last finalised height plus the configured
    `expiry_window` (`REQ-SO-2`: aledgent assembles it). `submit_outbound` refuses with
    `ERR-DeadlineRequired` when the operation's deadline is not after now + `expiry_window` ×
    `block_time` + `finality_margin`: `ERR-DeadlineRequired` is the closest condition SPEC §10 has
    for "no usable deadline". The estimate is configuration until `F-016` measures it (`OQ-36`
    untouched).
17. **Non-inclusion is proven from the inbox** (`REQ-SL-4`, `REQ-SL-9`). The instruction records
    the last finalised head's inbox length when it was assembled (`from_seq`): every position below
    it was final before the instruction existed. Once the finalised head is at or past the expiry,
    non-inclusion is proven when every position of `[from_seq, head.next_seq)` holds an ingested
    final record, no recorded gap touches the range, and no record names the operation. A gap, or
    a final record naming it (executed or refused), makes it unprovable: `Expired → Needs-Review`.
    A record naming it that is not yet final or orphaned: wait.
18. **An instruction never acknowledged** is released on the same proof, `Reserved → Released`
    (§5.6's "released with one that provably cannot execute").
19. **A repeat of `submit_outbound`** to the same destination returns the original instruction; to
    another destination it is `ERR-InvalidTransition`. The outbox keeps every instruction for
    ever; `pending_instructions` hands the unacknowledged ones, the same bytes under the same
    identity, after a restart (`REQ-SO-7`).

### Amount mismatch (accepted proposal, §9)

20. **Executed less than reserved:** one three-entry `Redeem` posting from the holding position:
    the executed amount to the issuance counterparty, the rest back to the payer
    (`ReservationBook::settle_outbound_for`, added to `F-004`'s seam beside `settle_outbound`).
21. **Executed more:** `Redeem` of the reservation, plus a second `Redeem` (ordinal 1) of the excess
    from the configured Suspense account to the issuance counterparty, in the same chain. Suspense
    is never-negative (`F-003`), so it must hold the excess: when it does not, or no Suspense is
    configured, the settling chain is refused and the operation stays `Final` in the review queue
    with both incidents. Once Suspense is funded, the operator's `resume` settles it
    (`inv_23_resume_settles_from_the_finality_witness`).
22. Either way the condition is named: `Verified::condition` is `ERR-AmountMismatch` and
    `Incident::AmountMismatch` records both amounts.

### `INV-10` by capability

23. **`FinalityWitness`** has private fields and no public constructor. Only the `Observed → Final`
    path builds one (and the witness of an operation already `Final`, for `resume`), and the
    builders of the settling `Issue` and `Redeem` take one by value. A compile-fail doctest pins
    it (`E0451`). `F-007`'s `resume` from `Final` now builds its posting through it, for `Issue`
    too. An operation brought to `Final` through the conformance ports alone has no record to rest
    on; only in builds with those ports (or tests) does `resume` then settle it for the reservation,
    as `F-007` built it; in a production build it is `ERR-EvidenceInsufficient`.
24. **`INV-13` by construction.** `OutboundInstruction::new` takes a `&ReservationHandle`, which only
    an open reservation of the index yields; both have private fields. Two compile-fail doctests
    pin it (`E0061`, `E0451`).

### Configuration and structure

25. **`configure_settlement`** is the operator's policy authority, set once, not a `Genesis` field
    (so `Genesis`, which every feature builds, keeps its shape). The authoritative source, the
    instance, the Suspense account, the inbound deadline and the timing estimates live there. The
    support declaration comes from the authoritative source and replaces the last.
26. **The account reference** is `F-003`'s `account_reference(account) = H("aledgent/account-ref/v1",
    account)`. The plan proposed `H("aledgent/acctref/v1", deployment, account)`; account
    identifiers are already minted from operation references derived under the deployment, so the
    existing function was kept rather than changing `F-003`'s derivation. Resolution scans the
    ledger accounts (never the issuance counterparty); `F-006` gives it an index. Rotating or
    single-use references (`F-019` open point 3, `REQ-PV-2`) are not built.
27. **The simulator** is height-based: a block every 600 ms of simulated time, finality at a lag the
    seed picks, forks above the finalised head, pruning below a window, expiry by height, and
    knobs for refusing, dropping and mis-executing an instruction. `Sim::transcript` is the
    byte-for-byte replay record. `F-015`'s `conformance` crate enables `sim`.
28. **`ERR-UnsupportedLeg`'s case stays `F-016`'s.** A second, `F-010`-owned case for it made
    `F-015`'s gate test (which picks the first case of that id) see an `m1` requirement it pins as
    absent. The refusal at submission is asserted inside the `INV-13` case instead. `AC-E1.4` and
    `AC-E1.5` get `F-010` cases against the simulator beside `F-016`'s pending ones.

### The signing key's nonce as a release precondition (owner ruling, 2026-09-24; #88)

**The ruling.** Before aledgent releases a reservation because an outbound transaction provably
never executed (decision 17), a second, independent fact must agree with the inbox proof: **the
signing key's account nonce at the finalised head has not moved past the transaction's nonce.**
The adapter never re-signs, and stores each signed transaction before it submits it (`F-016`), so
each nonce of a signing key belongs to one of the deployment's transactions, and a consumed nonce
means that transaction was included. The inbox proof alone rests on one reader; the nonce is read
from another part of the layer's state, so a reader fault (an entry decoded wrongly, a record the
adapter never saw) can no longer turn an execution into a release.

**Rejected: a position-balance check.** The owner considered and rejected checking the
deployment's position balance instead. A deposit landing in the same gap can mask a withdrawal,
so a balance cannot be attributed to one operation. No balance-based release condition exists,
and none may be added without a new ruling.

29. **The signing record** (`Ledger::record_signing`, the adapter's `AdapterInput::Sign`, route
    `POST /v1/settlement/signings`). The instruction record (`Instructed`) now keeps what the
    adapter did about signing it: `SigningRecord::Signed { key, nonce, last_valid }`, the one
    transaction it signed, recorded **before it submits it**; or `SigningRecord::NeverSigned`,
    its word that it never will (`F-016` decision 19's tombstone). The store records one, once,
    and only after the acknowledgement. The engine records a signed transaction only while the
    operation is `Submitted` and the finalised head is below the expiry, and only if its last
    valid height is at or before the expiry (`REQ-SL-4`); a tombstone in any state that is not
    terminal. A second, different record is `ERR-EvidenceInsufficient`; after a tombstone no
    signing is ever recorded. Recording moves no state.
30. **The nonce fact** (`Instructed::nonce`). The adapter sends, with every finalised head, the
    account nonce of each signing key it uses, read at that same block (`observe_head`'s
    `nonces`). For each signed transaction whose fact is not yet decided: a key nonce past the
    transaction's is `NonceFact::Consumed` (it was included), at any height; at a height at or
    past the transaction's **last valid height**, a key nonce at or below it is
    `NonceFact::Unconsumed` (it never can be). **The fact is decided once, at the first head that
    shows it, and never changes.** Trade-off, stated plainly: the ruling's premise, one nonce per
    transaction, holds only until the transaction provably dies. Account nonces are sequential,
    so once a transaction's era ends unincluded the adapter must give its nonce to the next one
    (`F-016` decision 13); a head read after that would show the nonce consumed by a transaction
    that is not this one. Fixing the fact when it is first decisive, and deciding
    `Unconsumed` at the era's end rather than the expiry, keeps a reused nonce from being read as
    this transaction's inclusion; the adapter's side of it is `F-016` decision 32.
31. **The proof** (`Proof`, decision 17 extended). Once the inbox proves non-inclusion:
    - never acknowledged (decision 18), or tombstoned: `NotIncluded`, on the inbox alone. A
      signing cannot be recorded before the acknowledgement, so decision 18's path never covers
      a transaction the adapter signed;
    - signed, nonce `Unconsumed`: `NotIncluded` → `Released`;
    - signed, nonce `Consumed`: **never released.** `Expired → Needs-Review`, and
      `Incident::IncludedWithoutRecord { op, key, nonce }`, which says plainly that the
      transaction was included but no inbox record names it;
    - signed, no fact yet (the key's nonce not reported): no proof; it waits.
    - **Acknowledged, with no signing record at all: no proof; it waits.** The ruling does not
      say this case; it is the conservative choice. An adapter that failed to record a signing
      must not get a release on the inbox alone; the adapter records a tombstone for every
      instruction it will never sign, so a correct adapter never leaves one here.
    A gap, or a record naming the operation, is `Unprovable` as before. A consumed nonce while
    the inbox range is not yet complete waits: the record may still arrive and settle the leg.
32. **An unreported nonce.** A head with no nonce for the key decides nothing. The leg stays
    `Expired`; past its deadline it moves to `Needs-Review` (`F-007`'s `past_deadline`, amendment
    0006 item 5). Nothing is released. A head that reports one key twice is
    `ERR-EvidenceInsufficient`, and nothing is written, the head included.
33. **What changes for a failed dispatch.** A transaction included in a finalized block whose
    call failed consumes its nonce and writes nothing to the inbox. The core cannot tell that from
    an execution its reader missed, so it now goes to `Needs-Review` with
    `IncludedWithoutRecord` instead of being released on the inbox alone. `F-016`'s disposition
    test says so (`req_sa_4_each_disposition_is_kept_apart`).
34. **Export.** `Instructed` carries the signing record and the nonce fact, and the settlement
    incidents the new variant (tag 10): `FORMAT_VERSION` is 4. A version-3 export is refused by
    version, not misread.
35. **The simulator and its driver.** The simulated layer gives every instruction a signing key
    of its own (`Sim::signing_key`), whose one transaction carries nonce 0; its nonce at the
    finalised head is 1 once the instruction is in a finalised block, executed or refused
    (`Sim::included_by`). The driver records the signing after the acknowledgement, with the
    expiry as the last valid height, and sends every head with those keys' nonces; an instruction
    it is handed only after the head passed its expiry is acknowledged and tombstoned, as the
    adapter would. The property run therefore releases only on both facts.

## Conformance and tests (T-010-10)

- 23 run cases in `crates/conformance/src/cases/f010.rs`, all green on both
  in-process stores: `INV-10`, `INV-11` (two), `INV-13`, `INV-23` at the service; `INV-12` at the
  store; `ERR-ObservationDuplicate`, `ERR-ObservationUnassociated`, `ERR-EvidenceInsufficient`;
  `AC-D1.1`–`AC-D1.4`, `AC-D3.1`–`AC-D3.3`, `AC-E1.1`–`AC-E1.5`; and `AC-B1.3` against the
  simulated layer (`F-008`'s `TODO(F-010)`): 100 transfers while the adapter cycles, blocks and
  finality go on, and the layer's inbox stays empty, its position unmoved, no instruction is
  assembled and nothing is ingested. `F-008` keeps its book-side case of `AC-B1.3`; its two
  `TODO(F-010)` comments now point here. The item-3 route of `AC-E1.4` runs since the ruling.
- **The property run** (`runs` in `f010.rs`): seeded histories of deposits (to open, closed and
  unknown accounts), withdrawals, blocks, finality lag, forks, pruning, delays and drops,
  duplicates and corroborating copies, mis-executions, deadlines and re-scans. After every step:
  `INV-10` (nothing against the issuance counterparty but `Final → Settled`), `INV-11` (every
  settled leg rests on a record final in the store and on the layer, of the right event; after
  settling, every released leg provably did not execute on the layer), `INV-12` (one final record
  per position, one settled leg per record), `INV-13` (every instruction rests on a reservation,
  every withdrawal the layer holds is one of aledgent's), `INV-23` (no external leg `Rejected`),
  non-negative Customer balances and a zero trial balance. `ledger-core`'s
  `tests/settlement_sim.rs` runs **10 000** histories (1 000 books of 10, one thread per core,
  about two minutes on a 12-core laptop; `F010_RUNS` changes it) and asserts every disposition was
  taken. Two mutations were tried and caught: promoting on a match without finality, and releasing
  without coverage.
- Simulator tests (`settlement/sim.rs`): heads, sequencing, fork, prune, expiry, refusal, replay
  of the same instruction, and a seed replaying a run byte for byte.

## Deferred

- `TODO(F-016)`: the real adapter, its signer, light client and cursor; the digest of a pallet
  inbox entry; `NotIncluded` observations if still wanted; measuring `block_time`.
- `TODO(F-013)`: operator resolution of `Needs-Review` with the evidence recorded here (the final
  failure of item 3 included); reconciling Suspense.
- `TODO(F-009)`: `record_observation`, `acknowledge` and the head on the adapter's authenticated
  surface; authenticating the source.
- `TODO(F-011)`: incidents, gaps and the unassociated bucket on the query and operator surfaces.
- `TODO(F-017)`: what an `Anchored` record means; it is ingested and ignored here.
- `F-005`, `F-006`: the durable settlement store; an index of account references; the outbox.
