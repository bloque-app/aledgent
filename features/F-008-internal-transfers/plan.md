<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-008 — Internal transfers and multi-leg

| | |
|---|---|
| Milestone | M-1 — The book, in a process |
| Track | A |
| Home | `crates/ledger-core` |
| Depends on | `F-001`, `F-003`, `F-007` |
| Unblocks | `F-011`, `F-013`, `F-009` |
| Realises | `DEC-1`, `DEC-3` |

## Goal

A holder moves value to any other ledger account in one posting. The transfer is final when it is
accepted, and it never touches the settlement layer. A caller can submit several transfers as one unit,
and either all of them apply or none do. A unit larger than the declared maximum is refused before
anything is attempted.

## Spec traces

- `US-B1`, `AC-B1.1` — one posting, final on acceptance (`REQ-OP-1`, `INV-1`).
- `AC-B1.2` — `ERR-InsufficientAvailable`, nothing changes (`REQ-LA-2`, `INV-3`).
- `AC-B1.3` — no settlement-layer contact, no cost (`REQ-SL-5`).
- `AC-B1.4` — `ERR-ZeroAmount`.
- `US-B3`, `AC-B3.1` — all or nothing (`REQ-KER-10`).
- `AC-B3.2` — legs debiting the same account and asset are summed for the check (`REQ-BA-4`).
- `AC-B3.3` — `ERR-BatchTooLarge` at submission (`OQ-24`).
- Supporting: `REQ-KER-1`, `REQ-KER-2`, `REQ-AS-4`, `REQ-OP-11`, `REQ-CS-1`, `REQ-CS-3`, `REQ-ER-1`,
  §5.6 rows `Received → Accepted`, `Accepted → Settled`, `Accepted → Rejected`.

## Scope

**In:** the `transfer` command and its multi-leg form, validation, the kernel posting and its signing,
the lifecycle wiring, the mapping of kernel statuses to `ERR-*`, the declared leg maximum.

**Out:** authority and bounds evaluation (`F-003`, called here); reservations and concurrent-spend
proofs (`F-004`, which owns `AC-B2.*`); queries over the resulting movements (`F-011`); conversion between
assets (`US-F2`, deferred by `DEF-7`); any leg with an external side (`F-010`); command intake and the
`Received` acknowledgement (`F-009`).

## Design

**1. Command shape.** `Transfer { legs: Vec<Leg>, initiator, on_behalf_of, client_key, deadline? }`,
with `Leg { from: AccountId, to: AccountId, asset: AssetId, amount: MinorUnits }`. A single transfer is
a unit with one leg. The command carries everything `REQ-CS-1` lists. Amounts arrive as integer minor
units. The one decimal conversion (`REQ-AS-4`) happens at ingress (`F-009`) through the `F-003` asset
registry. The library also refuses any amount it cannot represent exactly.

**2. Validation order.** Each check is cheap. All of them run before the kernel is called, so a refusal
leaves nothing behind:

1. Leg count ≤ `max_legs`, else `ERR-BatchTooLarge` (`AC-B3.3`). This runs first, before any lookup.
2. For each leg: amount > 0, else `ERR-ZeroAmount`. Asset registered, else `ERR-UnknownAsset`. Asset
   active, else `ERR-AssetDeprecated`. Amount within the asset's precision, else `ERR-PrecisionExceeded`.
3. For each account: exists and reachable, else `ERR-UnknownAccount`. Not frozen or closed, else
   `ERR-AccountFrozen` / `ERR-AccountClosed`. Owning identity active, else `ERR-IdentityRestricted`.
4. Authority over each debited account for kind `transfer`, through `F-003`: `ERR-NotAuthorised`,
   `ERR-AuthorityRevoked` or `ERR-BoundExceeded`. The authority used is recorded per leg (`REQ-AU-7`).
5. Idempotency through `F-007`: a repeat returns the stored reply. A changed body gives
   `ERR-ClientKeyReused`.

The first failing check names the refusal. The order is fixed and tested, so the same bad command always
gets the same error.

**3. Lifecycle.** The core takes the command at `Received`. `next(Received, Decide, …)` accepts it. In
the same commit, the posting is attempted: on success `Accepted → Settled` appends a `Transfer` posting,
on kernel refusal `Accepted → Rejected` appends nothing. A transfer has no reservation and no external
leg, so no other row of §5.6 applies. `INV-23` does not apply either, because nothing is observable
outside the book. The acknowledgement may be `Received` (`REQ-CS-3` allows it for a command that does not
reserve). The outcome reaches the caller by notification or query (`REQ-CS-6`).

**4. Kernel posting.** Each leg becomes one CipherClerk transfer with two entries on `Layer::Settled`:
debit `from`, credit `to`, same `ledger` tag (the kernel's word for currency). Settled is
the only layer used for available value under `DEC-2`. The builder takes the tag from the asset registry
and the account record, never from the caller. The kernel does not check an entry's currency against the
account's, so this is the library's duty. A test covers it. The core signs with each
debited account's `auth_key`, derived from the core root (`DEC-3`), after the authority check has passed.
The kernel wants one signature per distinct debited account, in first-appearance order.
`Customer` accounts carry `BALANCE_MUST_NOT_BE_NEGATIVE`, so the kernel's own check gives `INV-3`.

**5. Multi-leg as a linked chain (`REQ-KER-10`).** **Proposal:** the N legs are N transfers in one
CipherClerk linked chain: `LINKED` on the first N−1, a plain terminator on the last. The kernel stages the
chain in an overlay. It commits only if every event succeeds, and drops the overlay otherwise. Each leg is its own `Transfer` posting under one operation reference, with consecutive sequence
numbers. So `F-011` can show each leg's counterparty. The kernel applies the legs in order against the
staged state, so a later leg on the same account and asset sees the earlier debit. The sum is checked, not
each leg alone (`AC-B3.2`). A chain with no terminator makes the kernel stop the whole batch. So the builder always terminates the chain, and a debug assertion plus a test guard it.
*Alternative:* one CipherClerk transfer with 2N entries. It is also atomic and also sums per account, but
it merges all legs into one posting. Then the movement view cannot say who paid whom.

**6. Kernel status → `ERR-*`.** The list is closed (`REQ-ER-1`):

| Kernel status | Result |
|---|---|
| `BalanceMustNotBeNegative` | `ERR-InsufficientAvailable` |
| `AccountNotFound` | `ERR-UnknownAccount` |
| `DebitAccountAlreadyClosed`, `CreditAccountAlreadyClosed` | `ERR-AccountClosed` |
| `ExternalIdAlreadyUsed`, `TransferIdAlreadyExists` | a replay. Return the stored outcome, not an error |
| `LinkedEventFailed` on the other legs | the failing leg's own status decides the error |
| anything else | an internal fault (see Open points) |

In a failed chain, only the event that actually failed keeps its real status. The kernel rewrites the
others to `LinkedEventFailed`. The mapper reads that one event.

**7. Idempotency at two strengths.** The client key (`F-007`) returns the first reply. Each posting's
kernel `external_id` is derived from the operation reference, transition key and leg index (`F-007`
design point 6). A second copy that reaches the kernel is then refused permanently, at kernel strength
(`REQ-KER-6`).

**8. No settlement contact (`REQ-SL-5`).** The transfer path has no dependency on the outbound
instruction type of `F-010`. The effect list of a transfer can hold only a posting chain, a notification
and an audit record. A test asserts that the outbox receives no settlement instruction, and that the
simulated settlement layer of `F-010` records nothing.

**9. `max_legs`.** It is a declared deployment value, read at start-up and shown in the deployment's
declaration of limits. Its default comes from the `F-002` measurement of a chain inside one invocation.
The kernel has no batch limit of its own. Its dev-profile numbers are about 0.5 ms for
one transfer and 12.7 ms for a chain of ten. These are not a basis for a contract.

## Test plan

- `AC-B1.1`: one leg between two `Customer` accounts. One `Transfer` posting, state `Settled` in the same
  commit, balances moved exactly. Integration test on the `F-001` in-memory store.
- `AC-B1.2`: amount above available. `ERR-InsufficientAvailable`. Composite root and balances unchanged.
- `AC-B1.3`: the outbox and the simulated settlement layer are empty after 1,000 transfers.
- `AC-B1.4`: amount zero. `ERR-ZeroAmount`, kernel not called.
- `AC-B3.1`: a unit whose last leg overdraws. No leg applies, root unchanged. Property test over random
  units with one failing leg at a random position.
- `AC-B3.2`: two legs from one account, each covered alone but not together. The unit is refused.
- `AC-B3.3`: `max_legs + 1` legs. `ERR-BatchTooLarge`, no lookup made. A mock store counts the reads.
- `INV-1`, `INV-3`: property test of random concurrent transfer units against the kernel. No `Customer`
  balance is ever negative, and every posting balances per asset.
- Validation-order test: a command with several faults always gets the error of the first check.
- All of the above go into the `F-015` suite under the ids they name.

## Exit criterion

`AC-B1.1`–`AC-B1.4` and `AC-B3.1`–`AC-B3.3` pass in the conformance suite, against the library in a
plain process. The randomised unit test finds no partial application and no negative `Customer` balance
in 10k runs. This feature also closes `AC-A2.2` from `F-003` (a transfer between two accounts of one
identity is one posting, final on acceptance).

## Open points

- **`OQ-24` has no `DEP` in PLAN §3.** `AC-B3.3` cites it, and it is still open: is a multi-leg
  submission caller-facing, and what is its contractual maximum? This plan builds the check with a
  declared value and does not fix the number. The owner should add a `DEP` or answer the question.
- **`OQ-25` (multi-leg with external legs).** This feature accepts internal legs only. A unit that names an
  external destination is out of scope. Which refusal it gets (`ERR-UnsupportedLeg` is the nearest) is
  not decided here.
- **Internal faults.** A kernel status such as `InvalidSignature` or `BlindingMismatch` means a bug, not a
  caller error. §10 has no condition for it, and `REQ-ER-1` forbids an unnamed one. Meanwhile the
  invocation aborts without commit. The command stays `Received` at ingress, and an incident is raised.
  The owner should decide whether §10 needs a condition.
- **`Treasury` limits.** `REQ-LA-2` lets a `Treasury` account go negative down to a declared limit. The
  kernel knows only "never negative" or "no check". The limit check belongs to `F-003`'s
  account-kind policy. This feature calls it but does not build it.
- **Proposal:** a linked chain of per-leg postings (design point 5). Alternative: one posting with 2N
  entries.
