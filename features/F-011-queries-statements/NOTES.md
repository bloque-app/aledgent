<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-011 — implementation notes

Written while implementing `F-011` (issue #11), 2026-09-23. The plan's proposals were followed as
accepted: balance checkpoints (design point 7) and the cursor format (design point 5). Where the
plan was silent, the more conservative behaviour was chosen (refuse rather than allow); each such
choice is listed here for review.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/query/mod.rs` | `ReadView`, the request and reply types, bounds, `QueryEvents` (access trace and incidents) |
| `crates/ledger-core/src/query/journal.rs` | The posting journal: the plaintext of every posting, its store rules, and its indexes (by account, by (account, asset), `(sequence, at)`, running balances, checkpoints) |
| `crates/ledger-core/src/query/scope.rs` | `AuthorisedScope` |
| `crates/ledger-core/src/query/store.rs` | `QueryStore`: the storage trait's read side, every method filtered by the scope inside the store, with the index contracts for `F-006` in its rustdoc |
| `crates/ledger-core/src/query/movement.rs` | `Movement`, the total mapping `PostingKind → MovementStatus`, counterparty, authority attribution |
| `crates/ledger-core/src/query/cursor.rs` | `Cursor` |
| `crates/ledger-core/src/query/trial.rs` | The trial balance on commitments, from the leaf store alone |
| `crates/ledger-core/src/ledger/queries.rs` | `Ledger::view`, `view_with`, `reconcile`; the seven queries of SPEC §8.2 this feature owns |
| `crates/ledger-core/src/state.rs` | `Write::Posting`, `StoreRefusal::Journal`, `State::journal` |
| `crates/ledger-core/src/ledger/lifecycle.rs`, `issuance.rs` | Each writes one `Write::Posting` per posting its kernel step applies, in the same batch |
| `crates/ledger-core/src/ledger/tests/queries.rs`, `src/query/tests.rs`, `tests/queries_concurrency.rs` | Unit tests, the 10 000-query fuzz, and `AC-H1.2` with writer threads |
| `crates/conformance/src/cases/f011.rs`, `harness.rs` | Every `F-011` case, run; the query methods of `ServicePort` |

## Decisions taken during implementation

### Where a movement comes from

1. **A posting journal in the service store.** The kernel stores each entry's amount as a
   commitment; the vault keeps only per-account sums. The plaintext of a posting exists only in the
   draft the core builds (`DEC-11`: the core holds the openings). So the commit that applies a
   posting also writes its plaintext (`Write::Posting`) into the service store, in the same
   all-or-nothing batch: `F-007`'s `commit_plan` (after every other write, with the sequence
   numbers it already predicts) and `F-003`'s issuance (checked before the kernel step with the
   predicted sequence number, committed after it). A movement is **derived** from that posting on
   every read; no movement record is stored (plan §4). This is a small edit to both commit paths;
   `F-007`'s fault-injection test now counts nine writes in a reserve's batch, not eight.
2. **The store derives the lines itself.** A `PostingWrite` carries the draft; the store maps each
   entry to a ledger account with its own registry, accounts and reservation index, and refuses
   what it does not hold. Its rules: the sequence number is above every one recorded, `at` is not
   below the previous posting's, the operation is the draft's, a holding line is in its
   reservation's asset. Gaps in the sequence are allowed: a posting applied behind the journal's
   back (a raw kernel apply) then shows up as an `INV-6` drift, not as a refused write.
3. **Reserved value is an entry of the payer.** A line on a holding position (`DEC-2`) is shown as
   an entry on its reservation payer's **reserved** balance, which is SPEC §5.5's `Entry.kind`. A
   reservation is therefore two movements of the payer (available debit, reserved credit), a
   capture debits reserved, and a capture for less credits the difference back to available.
4. **`at` never decreases.** The writer sets `at = max(injected time, previous posting's at)` and
   the store refuses anything else (plan §6). The time index is then the journal's own
   `(sequence, at)` list, searched by bisection. `OQ-36` is untouched.

### Movements, pages and filters

5. **A movement is one entry** against one account (`REQ-QR-4`: "every entry"). A posting's
   movements are never split across pages: a page holds at most `limit` movements unless its first
   posting alone has more.
6. **Status** is `Held` (Reserve, Encumber), `Completed` (Capture, Transfer, Issue, Redeem,
   Conversion), `Returned` (Release, Disencumber), `Reversed`, `Opening` or `Adjusted`, by a total
   `match` with no wildcard. The movement also carries the posting kind itself.
7. **Counterparty** is the first line on the other side of the posting, in the same asset, on
   another account: the other account of a transfer, the issuance counterparty for an issue or a
   redemption, the payer for a capture's destination. A reservation's own two entries have none.
8. **Attribution.** The authorities of a movement come from the operation's attribution record (one
   per leg for a multi-leg transfer, matched by the posting's place), a capture's or release's
   transition record, or the lifecycle transition that appended it. Empty means aledgent made the
   posting on its own (evidence, a deadline).
9. **Time range** is half-open, `from <= at < to`.
10. **A page reads at most `SCAN_BUDGET` (4 096) postings.** A sparse filter could otherwise make
    one request walk an account's whole history; a page that reaches the budget ends there with a
    cursor, possibly short or empty.

### Bounds and refusals

11. **The bounds come first, before the credential** is resolved, so an unbounded request is
    refused before any read (`T-011-09`). A page size is required, `1..=1000`; a larger one is
    refused, not clamped. A statement needs both ends of its period, `from <= to`, and an `as_of`
    no later than the view.
12. **A malformed cursor, one made for another filter, or one from a view the book has not
    reached is `ERR-QueryUnbounded`.** SPEC §10's list is closed and this is the entry that fits:
    the request no longer bounds a range aledgent can serve.
13. **A statement carries at most 1 000 movements**; a longer period is `ERR-QueryUnbounded`
    ("ask for a shorter period"). Its opening and closing are exact whatever the period's length.

### Statements and `INV-6`

14. **A statement is per (account, asset).** It resolves `[from, to)` to `[from_seq, to_seq)` at
    the view (or at `as_of`), derives the opening from a checkpoint and the entries since, and
    closes on opening plus movements. `closed` says whether a posting at or after `to` is already
    recorded; a closed period, or a repeated `as_of`, gives the same bytes (`Statement::to_bytes`),
    as plan Open points reads it.
15. **The `INV-6` comparison is made at the view, not at `to_seq`.** The kernel keeps only current
    balances, so there is no stored balance at a past sequence to compare with. Every statement
    compares the account's entry-derived balances at the view with the stored ones — `available`
    with the kernel's settled net, `reserved` with the reservation index — and sends a difference
    as `Incident::Drift` to the view's `QueryEvents`. The statement still reports the entry-derived
    figures. `Ledger::reconcile` runs the same comparison over every account and asset for the
    reconciliation run; it is not on the query surface (`TODO(F-018)`: the channel).
16. **Checkpoints** are taken every 64 postings per (account, asset) (`CHECKPOINT_EVERY`); `F-006`
    tunes it against `NFR-3`.

### The other queries

17. **`balances`**: without an asset, a row for every asset whose kernel sub-account exists (every
    asset the account has ever held); with one, a row for it on every account, zeros included.
    `available` is `Ledger::net`, the one function `F-004`'s balance view and the Treasury check
    read.
18. **`can_spend`** runs `F-008`'s amount, asset, account and authority checks in `F-008`'s order —
    reach by `eval::knows`, as the transfer does, so a revoked grant is `ERR-AuthorityRevoked` —
    then `available` against the amount (a Treasury account against its declared limit). It names
    no destination, so a grant bounded to counterparties is `ERR-BoundExceeded` (`F-003` decision
    22: fail closed).
19. **`trial_balance` is the operator's** (`REQ-PV-2`: what it needs to operate); anyone else is
    `ERR-NotAuthorised`. The check on commitments sums every kernel account the service store knows
    (every account in every asset, every holding position); the store-only form
    (`trial::log_accounts` + `commitment_trial`) sums every account the delta log wrote, and is
    what the `INV-5` store case runs after every unit. `ledger-core` now depends on
    `curve25519-dalek` for the point sum, the version `cipher-clerk` already links.
20. **`operation(reference)`** is visible to its initiator and to whoever reaches an account it
    touched; to anyone else it is `ERR-UnknownOperation`, as a reference that does not exist.
    Commands with no lifecycle show `status: None`, as their replies do (`F-007` decision 10).
21. **`assets()`** is open to any authenticated principal: the registry is no account's fact.
22. **Every query is attributed before it reads** (`REQ-PV-5`): the caller is resolved and the
    access handed to `QueryEvents::access`. An unattributable access is refused, never traced.
    `F-013` stores the trace (`TODO(F-013)`).

### Views and the store layer

23. **A view is `&Ledger`.** It borrows the ledger immutably and fixes the posting sequence at
    creation, so in process nothing can be written while it lives, and it has no `&mut` method.
    `tests/queries_concurrency.rs` shows it under a read-write lock with a writer thread running.
24. **`INV-16` at the store** is the `QueryStore` trait: each of its methods takes the
    `AuthorisedScope` and refuses outside it. A scope is made from `F-003`'s reach only, or, under
    `conformance-ports`, narrowed by hand for the store case. **Stated plainly:** `State` still has
    the unscoped getters every command uses; what the store layer guarantees is that the query
    surface reads only through the scoped trait. The store case is registered at SPEC §9's layer
    and is reported, not required (amendment 0006 item 2, unruled).
25. **`Canon::as_bytes`** was added to `F-007`'s canonical encoder, for the statement's bytes.

## Observations for the owner

- **Sequence numbers are visible.** Every reply names the view's sequence, and a statement names
  `[from_seq, to_seq)`: the global posting sequence, as the plan designs the cursor and the
  statement. A holder can read from them how many postings the whole book made between two of its
  own reads — aggregate volume, no account's fact. If that is too much, the replies need a
  per-subject ordinal instead, and the cursor an opaque encoding of the sequence.
- **Access trace.** `QueryEvents` is where it is emitted; `F-013` must decide where it is stored
  and make the privileged read path of `REQ-PV-4`.

## Conformance (T-011-09)

- 18 cases in `crates/conformance/src/cases/f011.rs`, all run, all green on both in-process
  stores: `INV-5` (store), `INV-6`, `INV-8`, `INV-16` (store and service), `INV-20`,
  `ERR-QueryUnbounded`, `AC-H1.1`, `AC-H1.2`, `AC-H2.1`–`AC-H2.3`, `AC-H3.1`, `AC-H3.2`,
  `AC-H4.1`, `AC-H5.1`, and `AC-I1.2` and `AC-M4.1` (assigned by `F-015`).
- The two store cases drive the service port and then call below it: the `INV-16` case calls
  `QueryStore` on the service store with a narrowed scope; the `INV-5` case sums the leaf store's
  commitments after every unit.
- **Assurance** at `m1` now counts `INV-5`, `INV-8`, `INV-16` and `INV-20` as enforced. `INV-6`
  waits on `F-006`'s store case.
- `ledger-core` unit tests: `AC-H4.1`'s 10 000-query fuzz (`F011_QUERIES` changes it), `AC-H2.2`
  against a brute-force scan of the journal (every filter alone and in pairs), `AC-H2.1` with
  appends between pages, random periods across checkpoints, an injected drift, the time index
  against a scan, and `ERR-QueryUnbounded` with the leaf store's reads counted.

## Deferred

- `TODO(F-011)` in `ReadView::operation`: `F-010`'s observation coordinates, incidents, gaps and
  unassociated bucket on the query surface. `F-010` (PR #36) had not merged when this branch was
  rebased; its settling postings already reach the journal through `commit_plan`.
- `TODO(F-013)`: storing the access trace; the privileged read path (`REQ-PV-4`).
- `TODO(F-018)`: the reconciliation channel for `Incident::Drift`.
- `F-006`: the journal's durable home and the index contracts at 150 million entries; the `NFR-3`
  proof; the `INV-6` store case.
- `F-009`: the wire form of the requests, replies and cursor.
- `F-017`, `F-015`: `verify` and `assurance()` on the query surface.
