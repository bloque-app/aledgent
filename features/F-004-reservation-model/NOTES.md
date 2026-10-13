<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-004 — implementation notes

Written while implementing `F-004` (issue #4), 2026-09-23. The plan's proposals were followed as
accepted: Option A for the holding position, capture for less as one three-entry posting, a binding
fault refused as `ERR-NotAuthorised`. Where the plan was silent, the more conservative behaviour was
chosen (refuse rather than allow, keep reserved rather than release); each such choice is listed here
for review.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/tests/kernel_facts.rs` | The three kernel facts of the plan's §1, pinned on raw CipherClerk transfers (`T-004-01`) |
| `crates/ledger-core/src/reservation/` | `ReservationBook` (the seam), `PerOperation` (Option A), `Chosen`/`CHOSEN`, `UnitBuilder`, the open-reservation index, `BalanceView`, the holding check |
| `crates/ledger-core/src/ledger/reservations.rs` | `Ledger::reserve`, `capture`, `release`, `balance_view`, `check_holdings` |
| `crates/ledger-core/src/state.rs` | The index in the service store (`Write::Reservation`), transition records (`Write::PutTransition`), `Command::{Reserve, Capture, Release}` |
| `crates/ledger-core/src/ledger/reads.rs` | `Ledger::balances`; `trial_balance` counts holding positions |
| `crates/ledger-core/src/ledger.rs` | `close_account` refuses while a reservation is open (`F-003`'s deferred item) |
| `crates/ledger-core/tests/concurrency.rs` | The randomised concurrency test (`T-004-07`) |
| `crates/conformance/src/cases/f004.rs` | Every `F-004` case, run (`T-004-08`) |

## Decisions taken during implementation

### The holding option and the seam

1. **Option A is built; Option B is not.** `PerOperation` is the only `ReservationBook`. The service
   names the option once, `reservation::Chosen` (with the value `CHOSEN`), and reaches it through the
   trait and nothing else. Every trait method only *builds*: it appends postings and index changes to a
   `UnitBuilder` and applies nothing. The holding check (`check_holdings`) is on the trait too, because
   what the kernel must hold differs by option. Switching to Option B, or to a later kernel primitive
   (`DEP-4`), is a new `impl ReservationBook` and a change of the `Chosen` alias. See "The holding
   option" below for what `F-002` must show.
2. **Capture for less is one three-entry posting**: debit the holding `a`, credit the destination `c`,
   credit the payer `a − c`, with `CLOSING_DEBIT`. This follows the plan's accepted proposal. `DEC-2`'s
   text says "two entries in one chain"; both forms are balanced and atomic (`REQ-KER-10`), and
   `DEC-2`'s intent — the difference returns with the capture, matched by the holding's identity —
   holds in either. One posting meets `AC-C2.1`'s "in the same posting" literally. `PLAN.md` is not
   edited.
3. **One reservation per operation.** The seam's methods take the operation reference, never an asset
   to pick among several reservations of one operation. Under Option A the holding id derives from
   (operation, asset), as the plan says, so a later multi-asset operation stays expressible; today a
   second reserve under a reference that already has a reservation, open or closed, is refused
   (`ERR-ClientKeyReused`, as `F-003` refuses a reused reference).

### The index

4. **The index is in the service store**, not beside it. `Write::Reservation(IndexChange)` applies an
   open or a close under the index's own rules, in the same all-or-nothing batch as the window charges
   and the operation record, committed only if the kernel step applied. So the index is updated in the
   same apply unit as the posting (`REQ-RT-1`, `DEC-4`) and a raw write cannot break it: a reservation
   is opened once and closed once, closing is absorbing, a capture never clears more than was reserved
   (the store enforces `INV-14`'s amount rule too), and the payer and the asset must exist.
5. **Nothing is deleted.** A closed reservation keeps its record and how it ended
   (`Closing::{Captured, Released, SettledOutbound}`). In `M-1` the index lives in memory.
   **For `F-005`/`F-006`:** only the open part is hot state; the closed part grows with every operation
   and belongs in the side store, not in the Agent's 4 MiB.
6. **`reserved` is capped at `u64::MAX` minor units per (account, asset).** The index refuses an
   opening past it (`IndexRefusal::Overflow`, surfaced as a store refusal and so `ERR-NotAuthorised`).
   It cannot be reached in practice; it keeps `BalanceView` in exact integers.

### Commands, references and refusals

7. **Operation references.** `reserve` opens an operation: its `Ctx::op` is fresh and names the
   reservation from then on. `capture` and `release` are transitions of that operation: their
   `Ctx::op` **is** the reservation's reference. They are recorded as transition records beside the
   operation's record (`Write::PutTransition`, at most one per command kind), attributed like any
   record (`INV-20`). `F-007` folds both into its operation record.
8. **Refusals**, chosen from SPEC §10's closed list:

   | Condition | Refusal |
   |---|---|
   | `capture` or `release` of a reference with no reservation | `ERR-UnknownOperation` |
   | `capture` or `release` of a reservation already captured or released | `ERR-OperationTerminal` (`TODO(F-007)`: an identical repeat returns the original outcome, `REQ-OP-3`) |
   | `reserve` under a reference that already has a reservation | `ERR-ClientKeyReused` (`F-003` decision 6) |
   | `cleared` above the reserved amount | `ERR-AmountMismatch`, before any kernel call |
   | the kernel refuses on the payer (`BalanceMustNotBeNegative`) | `ERR-InsufficientAvailable` |
   | the kernel refuses on a holding (the index and the kernel disagree) | `ERR-AmountMismatch` (defence in depth) |
   | the kernel says a holding is closed, or the posting is a duplicate, on a transition | `ERR-OperationTerminal` |
   | a binding fault | `ERR-Internal`, the fault kept for the operator and counted as an incident (amendment 0006 item 8, `REQ-ER-2`) |

   Authority is checked before a closed reservation is reported terminal, so a principal with no
   authority over the payer learns nothing about how an operation ended (`ERR-UnknownAccount`).
9. **Authority.** `reserve` is authorised as `Reserve` on the payer's account with its asset and
   amount: bounds apply and windows are charged once, there. `capture` is authorised as `Capture` on
   the same account with the destination as counterparty and **no amount**, and `release` as `Release`
   with no amount: they spend nothing that was not already reserved, so no window is charged twice.
   **A release does not restore the window it used.** Restoring it is `AC-G4.2`, in `US-G4`, which
   SPEC §15 does not put in V0; the conservative reading keeps the charge.
10. **An ended reservation stays reserved while the evaluator refuses.** Capture and release go
    through the evaluator in its fixed order, so a frozen or closed payer, a restricted owner, or an
    asset deprecated since the reserve refuse them like any operation, and the value stays reserved.
    `F-013` (freeze, deprecation) may want a lifecycle path around this; not decided here.
11. **Destinations.** A capture may credit any open, non-Issuance ledger account whose owner is active
    (the same rule as `issue`), creating its position lazily in the same step (`INV-9`). Capturing into
    the payer itself is allowed and posted as a `Capture`.
12. **A Treasury payer** is held to its declared limit before the reserve, as `redeem` is
    (`check_treasury_limit`); the kernel does not check an unflagged account.
13. **A payer position that never held the asset** is created in the reserve's step; the kernel then
    refuses the reserve (`ERR-InsufficientAvailable`) and drops the creation with the chain.
14. **Release safety is not checked here.** `REQ-OP-9` and `ERR-ReleaseUnsafe` belong to the lifecycle
    (`F-007`, `F-010`), which must decide before it calls `release` when an operation has an external
    leg. No reservation has one yet (`TODO(F-007)`, `TODO(F-010)`).
15. **`settle_outbound`** (the `Redeem` posting that empties a holding into the issuance counterparty)
    is built by the seam only. No command exposes it: only the lifecycle, on evidence, may call it
    (`INV-11`, `TODO(F-010)`).

### Reads

16. **`available` is the settled net**, the number the kernel checks, read by `balances`,
    `balance_view` and `F-003`'s `available` alike. `reserved` is the index's sum at the same state
    version; `encumbered` is 0 (`DEF-11`). In process, one `&Ledger` is one committed version.
17. **`trial_balance` counts holding positions**: with a reservation open, the ledger accounts alone no
    longer sum to zero (`INV-5`).

### Composition

18. **One operation per unit.** Two builders applied as two steps of one unit are built against the
    index as committed before the unit, and neither knows whether the other will apply. If both touched
    one operation, the second could create a holding the first created, which the kernel refuses as a
    binding fault that discards the whole unit. A composer (`F-005`, `F-007`, `F-008`) puts all of an
    operation's postings in one builder (one chain). `UnitBuilder`'s documentation says so; the
    concurrency test follows the rule.
19. **`UnitBuilder::post` is public**, so a composer can chain an ordinary posting (a transfer, a fee)
    with a reservation posting, all or nothing.

### Tests

20. **Kernel facts** (`T-004-01`) run on the kernel's own `MemLedger` (`cipher-clerk`'s `helpers`,
    dev-only in `ledger-core`), with no aledgent code in between, so an upgrade that changes a fact
    fails there first. Fact (c) is shown on an unflagged payer (the only kind that can hold a `PENDING`
    debit, by fact (b)) and then contrasted with `DEC-2`'s shape.
21. **The concurrency test** (`T-004-07`): "10 000 cases" is read as 10 000 proptest cases **per
    store**, each a book with 36 commands split into units and chains, run as four parallel parts per
    store; about 90 s for both stores on a laptop. `F004_CONCURRENCY_CASES` changes the count. The
    `FileStore` runs on redb's in-memory backend, as `F-001`'s differential test does. Commands the
    engine refuses to build (unknown or closed reservation, over-capture, a reused reference) are
    dropped and counted, as the service would refuse them before any kernel call.
22. **`ledger-core` dev-depends on `conformance`** (for `ShadowBook` and the delta log) and on
    `ledger-kernel` with `std` (for `FileStore`). Both are dev edges; `check-conformance-ports.sh` only
    rejects normal and build edges (`F-015` NOTES 24).
23. **The layer check is registered under `INV-3` at the kernel layer**, run through the service port:
    it reads the kernel's own leaves after service commands and shows that nothing is ever on
    `Pending` or `Encumbrance`, so the kernel's settled check is the whole of "never negative in any
    balance kind". `T-004-08` asked for it without naming an id; `INV-3` is the invariant it serves.

## The holding option: what `F-002` must show to keep Option A

The plan's rule: Option A, **unless** `F-002` shows that the extra creation pushes the reserve path's
p99 past its share of `NFR-1`, or that leaf growth breaks `F-006`'s budget. `F-002` is still running.
To keep Option A, its result must show all of:

1. **Latency.** The reservation-path budget of `F-002`'s decision rules — S2 for the shape *reserve
   with a new holding account* (one creation plus two entries) + S3 verify-and-apply at 1 000 000
   accounts + S4 at the state size the design carries — is within `NFR-1`'s 50 ms at p99 (as the
   rule was then read; amendment 0007 corrects it: the decision is part of completion, `NFR-15`). The
   difference from Option B is one registrar signature check and one account-leaf insert (and its
   proof path in the witness); S1/S2 should report that delta explicitly, per posting and with the
   curve precompile and without.
2. **Witness size.** The reserve-with-creation and the capture-with-`CLOSING_DEBIT` shapes stay within
   the invocation limits (S3 item 1), or within the "carry as a private input" fallback.
3. **Growth.** S3's 12-month leaf-store growth at `NFR-2`'s volume **with one
   holding leaf per reserving operation, never deleted** — one extra account leaf for every
   operation, if every operation reserves — fits `F-006`'s budget. The index's hot part is only the open
   reservations (decision 5); S3's hot-state figure should count those, not the closed ones.

If (1) or (3) fails and Option B passes them, the owner switches to Option B. What that costs, stated
plainly: the kernel then protects only the pool, so matching a capture to its reservation rests on the
index alone (`INV-14` at the service, which §9 allows; the store's own `cleared ≤ amount` rule stays),
and a second capture of one operation is no longer refused by a closed account, only by the index and
the posting identity. The engine test `dec_2_a_second_capture_or_release_is_refused_by_the_kernel` is
Option A's and would be replaced; the conformance cases and the concurrency test run unchanged.

This section is the proposal for the owner. The ruling is not recorded yet.

## Conformance (`T-004-08`)

- 14 cases in `crates/conformance/src/cases/f004.rs`, all run, all green on both in-process stores
  under `m0` and `m1`: `INV-3` (kernel, two cases), `INV-14` (service), `ERR-InsufficientAvailable`,
  `ERR-AmountMismatch`, `AC-B2.1`, `AC-B2.2`, `AC-C1.1`–`AC-C1.3`, `AC-C2.1` (first half), `AC-C2.2`,
  `AC-C3.1`, `AC-G3.2` (moved from `F-003`).
- **The repeat half of `AC-C2.1`** ("a repeat is a no-op that returns the original outcome") is
  registered in `f007.rs` as its own case, pending on `F-007`. Until then a repeated capture is
  `ERR-OperationTerminal`.
- `INV-3` at the kernel is now backed by `F-001`'s case and both of `F-004`'s.

## Deferred

- `T-004-06`, in part: **Option B is not built** and the choice is not ruled; the seam is in place and
  the criteria are above. Waits on `F-002`'s numbers and the owner.
- `TODO(F-007)`: an identical repeat returns the original outcome (`AC-C2.1`, `REQ-OP-3`); operation
  states and deadlines; folding transition records into the operation record.
- `TODO(F-007)`, `TODO(F-010)`: release safety (`REQ-OP-9`, `ERR-ReleaseUnsafe`, `AC-C3.2`);
  `settle_outbound` on evidence.
- `F-011`: `balances` per identity across accounts and assets (`REQ-BA-5`) on `BalanceView`.
- `F-005`, `F-006`: the durable home of the index, the open part hot and the closed part cold.
