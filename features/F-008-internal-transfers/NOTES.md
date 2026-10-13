<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-008 — implementation notes

Written while implementing `F-008` (issue #8), 2026-09-23. The plan's proposal was followed as
accepted: a multi-leg unit is a linked chain of one `Transfer` posting per leg, under one operation
reference. Where the plan was silent, or where it and the merged engine disagreed, the more
conservative behaviour was chosen (refuse rather than allow); each such choice is listed here for
review.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/ledger/transfers.rs` | `Transfer`, `Leg`, `Ledger::transfer`, the fixed validation order, `chain_of`, the kernel-status mapper, the received path |
| `crates/ledger-core/src/lifecycle/mod.rs` | `LifecycleConfig::max_legs`, `with_max_legs`, `DEFAULT_MAX_LEGS` (T-008-06) |
| `crates/ledger-core/src/ledger/requests.rs` | `requests::transfer`, the canonical digest of a transfer |
| `crates/ledger-core/src/ledger/lifecycle.rs` | `Refusals::Transfer`: the engine names a kernel refusal of a transfer's chain with `F-008`'s mapper |
| `crates/ledger-core/src/state.rs` | `Command::Transfer` |
| `crates/ledger-core/src/eval.rs` | `knows` made public: the evaluator's reach step, used by the transfer's own step 4 |
| `crates/ledger-core/src/ledger/tests/transfers.rs` | Unit tests over the real kernel, with a leaf store that counts its reads |
| `crates/ledger-core/tests/transfer_units.rs` | The randomised test: 10 000 units on `MemStore`, 1 000 on `FileStore` |
| `crates/conformance/src/cases/f008.rs` | Every `F-008` case, run: two kernel cases and nine service cases |
| `crates/conformance/src/harness.rs` | `ServicePort::transfer` |

## Decisions taken during implementation

### The command

1. **What `REQ-CS-1` lists travels in the `Ctx`.** `Transfer` carries its legs and an optional
   deadline. The initiating principal, its credential and whom it acts for are the `Caller`; the
   client key is the operation reference `keys::operation_reference` derives from it (`F-007`),
   passed as `Ctx::op`, as every other command takes it. The plan's
   `Transfer { legs, initiator, on_behalf_of, client_key, deadline? }` is therefore
   `Ctx + Transfer { legs, deadline }`.
2. **A transfer needs a deadline** (`REQ-OP-5`, `INV-15`): the command's, else the deployment's
   default for `OperationKind::Transfer`, else `ERR-DeadlineRequired`. `Accepted` exists only inside
   the one commit, but the store refuses a non-terminal state without one, and a transfer received
   first (`F-009`) waits in `Accepted` no longer than that commit either. The in-process harness
   declares no default for transfers (an `F-007` case relies on that), so the suite's transfers carry
   their own deadline.
3. **A unit of no legs is `ERR-ZeroAmount`**: it moves nothing where a movement was intended.

### The fixed validation order

4. **The repeat check moves up.** The plan lists idempotency last (step 5). `F-007`'s engine checks a
   repeat right after the credential for every command, and `REQ-OP-10` stores the canonical
   response with the key: a repeat must get the original outcome even if an account was frozen or a
   balance spent since. So the order is: leg count (before any lookup), credential, repeat or
   `ERR-ClientKeyReused`, then the plan's steps 2–4, the deadline, and the limits. One consequence:
   a different body under a used key is `ERR-ClientKeyReused` even if it also has a zero amount.
5. **Account status before the owner's**, as the plan orders step 3. The evaluator (`F-003`) checks
   the owner first; it runs again in step 4 and then always passes these checks.
6. **The credited account** must exist (`ERR-UnknownAccount`), be open, and have an active owner. It
   need not be reached by the principal: it is addressed, as a capture's or an issue's destination
   is (`F-004`, `F-003`). The **issuance counterparty is never a transfer's destination**
   (`ERR-NotAuthorised`): value enters and leaves it only by `issue` and `redeem`. As a source the
   evaluator already refuses it.
7. **`ERR-PrecisionExceeded` for a unit whose legs sum past `u64::MAX` minor units** on one account,
   asset and side. A `Minor` is always within an asset's precision; this is the one amount the unit
   "cannot represent exactly" (plan §1), refused and never wrapped, as `F-003` decision 9 refuses a
   decimal that does not fit.

### Authority

8. **The unit is one operation for its bounds.** Each leg is authorised on its debited account with
   its own amount and counterparty, and the authority used is recorded per leg (`REQ-AU-7`: the
   record's `authorities` has one entry per leg, in leg order). Legs that draw on one grant are then
   added together: their sum per asset must be within the grant's effective per-operation bound, and
   each window counter is charged once with the sum and must stay within its limit. Otherwise
   splitting one payment into legs would pass a per-operation bound that the payment exceeds.

### Limits the kernel does not check

9. **Treasury accounts, leg by leg.** `REQ-LA-2`'s declared limit is checked (with `F-003`'s
   `check_treasury_limit`) against a running net that starts from the book and follows the legs in
   the order the kernel applies them, credits included.
10. **A leg into its own account is held to what is available.** The kernel sees a debit and a credit
    of one sub-account in one transfer, a net of zero, and accepts any amount: a self-leg of 50 from
    a balance of 10 applied in a probe. SPEC does not forbid a self-transfer, but `AC-B1.2` says a
    movement that available does not cover fails, so the service refuses it with
    `ERR-InsufficientAvailable` when the running net at that leg is below its amount. A covered
    self-leg is accepted and posted.

### The lifecycle

11. **A command refused synchronously records nothing** (`F-007` decision 8): a validation refusal or
    a kernel refusal leaves the book, the operation store, the outbox and the records as they were
    (`AC-B1.2`). The plan's `Accepted → Rejected` on a kernel refusal is taken where there is an
    operation to record it on: **a transfer durably received first** (opened at `Received`, by
    `F-009`'s intake; in process, by the engine's `receive`). The same command decides it:
    `Received → Accepted → Settled` on success; `Received → Rejected` for a refusal before the
    kernel; `Received → Accepted → Rejected` when the kernel refuses the chain. The notification of
    `Rejected` names the refusal, the client-key entry is written with reply `Rejected`, and a repeat
    of the same request gets the same refusal (any other request, `ERR-ClientKeyReused`). A binding
    fault decides nothing: the operation stays `Received` and the fault is kept (plan Open points).
12. **`external_id` is `F-001`'s** `(operation, Transfer, ordinal)`, with ordinals `0..N` in leg
    order (`F-007` decision 4), so every leg of a unit has its own kernel identity and a replayed leg
    is refused for ever.

### The kernel-status mapper

13. **The mapper reads the binding's labels**, not raw statuses: `Insufficient` →
    `ERR-InsufficientAvailable`, `Closed` → `ERR-AccountClosed`, `Duplicate` →
    `ERR-ClientKeyReused`, anything else fails closed. The engine reads the one event that is not
    `LinkedEventFailed`, so the failing leg's own status names the refusal wherever it is.
14. **`AccountNotFound` is a binding fault, not `ERR-UnknownAccount`** as the plan's table has it.
    The binding creates every missing sub-account in the same step (`INV-9`), so the kernel can
    only say it if a creation was lost; `F-001` classes it as a fault, and the command answers
    `ERR-Internal` with the fault kept (amendment 0006 item 8, `REQ-ER-2`).
15. **`Duplicate` is `ERR-ClientKeyReused`.** A repeat of an accepted request is answered from the
    service store before the kernel is reached. A posting the kernel holds and the service store
    does not know has no stored outcome to return, so the reference is refused as used.

### The declared maximum

16. **`max_legs` is in `LifecycleConfig`**, the deployment configuration `Genesis` carries and
    `Ledger::config()` shows, with `with_max_legs` beside `with_default_deadline`. Its default is
    `DEFAULT_MAX_LEGS = 10`, `TODO(F-002)`: the kernel's dev-profile chain of ten (plan §9), not a
    measured budget. `OQ-24` is not answered (plan Open points).

### Tests

17. **"10k runs"** is read as 10 000 random units through `Ledger::transfer`, in 400 books of 25
    units (as `F-015` decision 25 reads its own), four parallel parts, on `MemStore`; and 1 000 more
    on `FileStore` (redb's in-memory backend). `F008_UNITS` and `F008_UNITS_FILE` change the counts.
    It takes about eight seconds.
18. **`INV-1` and `INV-3` are registered at the kernel**, the layer SPEC §9 names: raw, validly
    signed chains through `KernelPort`, linked as the binding links a unit. The kernel refuses a
    chain whose unbalanced (or overdrawing) leg is at any position, whole, and applies legs in order
    against the staged state.
19. **`AC-B1.3`**: no settlement instruction type exists yet (`F-010`), so the tests assert that the
    outbox holds only the transitions' notifications, every posting is a `Transfer`, and the
    issuance counterparty's balance does not move. `TODO(F-010)`: also assert that the simulated
    settlement layer recorded nothing.

## An observation outside this feature

Applying a posting to the in-memory book gets slower as the book grows, roughly linearly in its
size: about 5 ms per transfer on a nearly empty `MemStore` book, over 30 ms after 800 postings and
over 60 ms after 1 600 (debug build of `ledger-core`, optimised dependencies). Raw `Book::apply`
shows the same growth, so it is in the binding or the in-memory stores (`F-001`), not in the
service.
The 1 000-transfer test of `AC-B1.3` takes about 24 s because of it. `F-006`'s production store
and `F-002`'s measurements should confirm it is not in the path they keep.

## Deferred

- `TODO(F-002)`: the default of `max_legs`.
- `TODO(F-009)`: intake of transfers at `Received`, the ticker, and the wire form of a unit. The
  decision of a received transfer is built here.
- `TODO(F-010)`: `AC-B1.3` against the simulated settlement layer.
- `OQ-25`: a leg names ledger accounts only; there is no way to name an external destination, so
  nothing decides which refusal such a leg would get.
- `OQ-24`: whether multi-leg is caller-facing and its contractual maximum.
