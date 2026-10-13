<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-004 — Reservation model

| | |
|---|---|
| Milestone | M-1 — The book, in a process |
| Track | A |
| Home | `crates/ledger-core` (module `reservation`) |
| Depends on | `F-001`, `F-003`. `F-002` (numbers for the holding-option choice) |
| Unblocks | `F-007` (book effects of `Reserved`, `Settled`, `Released`), `F-010` (outbound legs reserve first), `F-011` (`reserved` in balances) |
| Realises | `DEC-2` |

## Goal
A reservation takes value out of spendable at once, in the same kernel step as the check that there
was enough. The kernel itself enforces that check. Capture and release are matched to the
reservation by identity, never by an amount a caller supplies. `reserved` is a derived number, read
from the same view as `available`. No schedule of concurrent commands can overdraw an account.

## Spec traces
- `REQ-KER-3`: three balance kinds. Reserved value is held as positions the account draws on.
- `REQ-KER-4`, `INV-3`: never negative, at the kernel.
- `REQ-BA-1`: moves among kinds are ordinary entries.
- `REQ-BA-2`, `REQ-BA-3`: one number decides a spend and is shown.
- `REQ-BA-4`: the reduction is atomic with the check.
- `REQ-BA-5`: identity-wide balances are served by `F-011` from this feature's view.
- `REQ-BA-6`: `reserved` derived, from the same view.
- `REQ-OP-2`, `INV-14`: reservation and capture or release are one object, matched by identity.
- `AC-B2.1`, `AC-B2.2`, `AC-C1.1`–`AC-C1.3`, `AC-C2.1`, `AC-C2.2`, `AC-C3.1`.

## Scope
**In:**
- the three kernel facts, pinned as regression tests;
- holding positions and the two `DEC-2` options behind one trait;
- `reserve`, `capture` (full and for less) and `release` as postings;
- the `Redeem` posting shape that `F-010` uses to settle an outbound leg;
- the open-reservation index;
- `BalanceView`;
- the randomised concurrency test.

**Out:**
- Operation states, deadlines, `Needs-Review` and the release-safety rule `REQ-OP-9`
  (`F-007`, `F-010`; `AC-C3.2` is not in this feature's exit).
- Authority (`F-003`).
- Encumbrance (`DEF-11`, `F-024`): encumbered reads zero in V0.
- `can_spend` and query surfaces (`F-011`).

## Design

**1. What the kernel does, and why the obvious mapping fails** (CipherClerk
`e9a7b08`).

- **(a) Zero-sum is per (currency, layer).** Entries are grouped by the literal pair
  `(e.ledger, e.layer)` (`src/kernel/invariants.rs:22`), and each group must balance on its own
  (`:36-40`). Suppose a transfer debits account A on `Settled` and credits A on `Pending`. It forms
  two groups, and neither balances. A move between layers of one account needs a counter-account on
  each layer: at least four entries.
- **(b) `PENDING` debits the payer on `Pending`, and negativity is checked per touched layer.** A
  `PENDING` transfer must place every entry on `Layer::Pending` (`src/kernel/apply.rs:175-182`). For
  a flagged account, the check computes the net from that layer's own pair only
  (`src/kernel/invariants.rs:90-99`). A customer balance is a credit-normal liability. Reserving
  debits it on `Pending`, so the `Pending` net is `−amount`. The kernel rejects it with
  `BalanceMustNotBeNegative` (`tests/kernel_pending_flagged_invariants.rs:78-114`). The CLI's own
  help says the source must allow negative (`cli/src/main.rs:240-241`).
- **(c) There is no kernel notion of available.** The `Settled` check reads `balances[Settled]` and
  never `balances[Pending]` (`src/kernel/invariants.rs:90-99`). So a second spend is checked against
  the full settled balance while a pending debit is outstanding. `Encumbrance` has no lifecycle at
  all.

Two shapes are left. One is to drop the flag and use `PENDING`. That gives up
the kernel's only overdraft protection, so `INV-3` would fall to a service check, which `REQ-AR-4`
forbids. The other is a Settled-to-Settled transfer into a separate holding account. `DEC-2` is the
second.

**2. How holding positions meet each constraint.**

| Constraint | Result |
|---|---|
| (a) zero-sum per (currency, layer) | Every reservation entry is on `(asset tag, Settled)`. Each kernel transfer is one group, balanced by `F-001`'s builder. No transfer uses `PENDING`, `POST_PENDING_TRANSFER` or `VOID_PENDING_TRANSFER` |
| (b) per-layer negativity | The payer's sub-account is flagged (Customer, `F-003`). A reserve debits only its `Settled` layer, so the kernel's check runs on exactly the number we call `available`, inside the same transfer that moves it (`REQ-BA-4`, `INV-3` at kernel) |
| (c) no kernel `available` | Not needed. Reserved value has left the payer's `Settled`, so a second spend sees only what is left |
| `Pending` and `Encumbrance` layers | Stay zero on every account in V0. A conformance case asserts that no posting touches them |

**3. The two holding options.** PLAN leaves the choice to this feature, after measurement.

- **Option A — one holding account per (operation, asset).** Its kernel id is derived from the
  operation reference (`F-001` §2). It is created in the same apply step as the reserve, and it is
  flagged `BALANCE_MUST_NOT_BE_NEGATIVE`. The last posting that empties it carries `CLOSING_DEBIT`,
  which marks it `CLOSED` in the kernel. This gives three guarantees at kernel
  strength:
  - capture plus release can never exceed what was reserved;
  - a second capture or release is refused with `DebitAccountAlreadyClosed`;
  - the account's identity is the reservation's identity (`INV-14`).

  *Cost:* one account creation per reservation. That means a registrar signature check and one
  leaf that is never deleted.
- **Option B — one pooled holding account per asset**, flagged, with each reservation keyed by
  operation reference in the open-reservation index. It needs no creation per reservation. But the
  kernel only protects the pool, so matching a capture to its reservation rests on the service
  (`INV-14`, which §9 names Service).

**Proposal:** Option A, unless `F-002` shows that the extra creation pushes the reserve path's p99
past its share of `NFR-1`, or that leaf growth (S3, 12 months at `NFR-2`) breaks `F-006`'s budget.
(Amendment 0007: that share was a misreading; the decision is part of completion, `NFR-15`.)
The choice goes to the owner, then to PLAN. Both options sit behind one
trait, so the choice and a later kernel primitive (`DEP-4`) stay local changes:

```rust
pub trait ReservationBook {
    fn reserve(&mut self, u: &mut UnitBuilder, op: OpRef, payer: AccountId, asset: AssetId, amount: NonZeroMinor) -> Result<(), Refusal>;
    fn capture(&mut self, u: &mut UnitBuilder, op: OpRef, destination: AccountId, cleared: NonZeroMinor) -> Result<(), Refusal>;
    fn release(&mut self, u: &mut UnitBuilder, op: OpRef) -> Result<(), Refusal>;          // no amount parameter
    fn settle_outbound(&mut self, u: &mut UnitBuilder, op: OpRef) -> Result<(), Refusal>;  // used by F-010
    fn view(&self, account: AccountId, asset: AssetId) -> BalanceView;
}
```

**4. Postings** (Option A shown; `H` is the operation's holding account, `P` the payer, `D` the
destination, `I` the issuance counterparty; `a` is the reserved amount, from the index).

| Step | Kernel transfer | Posting kind (SPEC §5.6) |
|---|---|---|
| reserve | create `H`; then debit `P` a, credit `H` a | `Reserve` |
| capture, full | debit `H` a, credit `D` a; `CLOSING_DEBIT` | `Capture` |
| capture for less, `c < a` | debit `H` a; credit `D` c; credit `P` (a − c); `CLOSING_DEBIT` | `Capture`: **one posting**, three entries |
| release | debit `H` a, credit `P` a; `CLOSING_DEBIT` | `Release` |
| outbound settled (`F-010`) | debit `H` a, credit `I` a; `CLOSING_DEBIT` | `Redeem` |

Capture for less is one three-entry transfer, with one debit and two credits in one
(currency, `Settled`) group. That meets `AC-C2.1`'s "in the same posting" literally. **Proposal:**
this form. *Alternative:* the two-transfer linked chain that `DEC-2`'s text describes. It is also
atomic (`REQ-KER-10`), but it is two postings.

`release` takes no amount, and `capture` takes only the cleared amount. The remainder always comes
from the index, never from a caller (`REQ-OP-2`, `INV-14`). If the cleared amount exceeds the
reserved amount, the service refuses with `ERR-AmountMismatch` before any kernel call, and nothing
changes (`AC-C2.2`). The flagged holding account refuses the same thing at the kernel, as defence in
depth.

**5. The decision and its refusals.** The reserve is one kernel transfer. The binding does not
pre-check the balance (`F-001` §8). The kernel's `BalanceMustNotBeNegative`, when the plaintext net
is negative, is classified `Insufficient` and becomes `ERR-InsufficientAvailable` (`AC-C1.3`). If
authority cannot be evaluated, the refusal is `ERR-NotAuthorised` (`REQ-AU-1`). Refusal is the
default whenever the decision cannot be made. Each kernel transfer's `external_id` is derived from
(operation, step) (`F-001` §3), so the kernel refuses a repeated step for ever. `F-007` turns that
`Duplicate` into "return the original outcome" (`AC-C2.1`, `REQ-OP-3`).

**6. The derived `reserved`** (`REQ-BA-6`, `REQ-BA-3`). The open-reservation index is hot core state:

```
open_reservations: Map<(AccountId, AssetId), Map<OpRef, NonZeroMinor>>
```

It is updated in the same apply unit as the kernel posting, so inside one commit (`REQ-RT-1`,
`DEC-4`). `reserved(P, asset)` is the sum of that inner map.

`BalanceView { available, reserved, encumbered: 0, total }` is read from one committed state version:

- `available` is the plaintext net of `P`'s `Settled` layer, which the core holds as oracle
  (`DEC-11`, `F-001` §6);
- `reserved` comes from the index at the same version;
- `total = available + reserved + encumbered`.

`available` is the number the kernel checked, so what a holder sees and what a spend is checked
against are the same (`REQ-BA-2`, `REQ-BA-3`, `AC-C1.2`). Two checks keep the index honest (`INV-6`,
run by `F-015` and `F-018`): the index equals the sum of open holding balances in the kernel, and
every closed holding account has a zero net.

**7. Concurrency.** The core is a single writer: one lane per Agent. So
"concurrent" means any order of arrival and any split into apply units and chains. A `proptest`
model generates a few accounts and several hundred reserve, capture, release and transfer commands.
It shuffles them, splits them at random into units and linked chains, and checks every unit against
`F-015`'s `ShadowBook`.

## Test plan
- **Kernel facts (a)–(c)** of §1 are pinned as three tests on raw kernel transfers. If
  a CipherClerk upgrade changes any of them, we know before the design silently changes meaning
  (`RISK-3`, `DEP-4`).
- **`AC-B2.1`, `INV-3`:** the randomised concurrency test. Exactly as many reserves succeed as the
  balance covers, in the model's sequence order. No Customer `available` is ever negative. The trial
  balance is zero after every unit (`INV-5`). It runs 10 000 cases on both stores. A kernel-layer
  case bypasses `ledger-core` and tries to overdraw a payer with open reservations. The kernel
  refuses it.
- **`AC-B2.2`, `AC-C1.1`:** the reserve's kernel transfer is the only place `available` falls. A
  refused reserve leaves the store and the index unchanged, byte for byte.
- **`AC-C1.2`:** after a reserve, every `BalanceView` read shows `available` reduced.
- **`AC-C1.3`:** an insufficient balance gives `ERR-InsufficientAvailable`. Authority that cannot be
  evaluated gives `ERR-NotAuthorised`.
- **`AC-C2.1`, `AC-C2.2`, `AC-C3.1`:** capture for less is one posting with three entries. Capture
  over the reserved amount gives `ERR-AmountMismatch`, with no change. A release is one posting.
- **`INV-14`:** there is no API path that passes an amount to `release`. A repeated capture or
  release is refused by the kernel (closed holding or duplicate `external_id`). A capture for another
  operation's reference cannot reach this holding account.
- **Layers:** no posting touches `Pending` or `Encumbrance`. Every closed holding has a zero net.

## Exit criterion
From PLAN §4: *`AC-B2.*`, `AC-C1.1`–`C1.3`, `AC-C2.*` and `AC-C3.1` pass, and a randomised
concurrency test never overdraws.* Made concrete:

- those cases, plus `INV-3` (kernel), `INV-14`, `ERR-InsufficientAvailable` and
  `ERR-AmountMismatch`, are green in the `m1` profile on both stores;
- the concurrency test passes 10 000 cases in CI;
- the holding-option decision is recorded with its `F-002` numbers.

**Build order.** This feature builds the reservation postings, not the operation states. The half of
`AC-C2.1` that says a repeated capture returns the original outcome needs `F-007`, and closes there.

## Open points
- **Holding option.** A is the proposal, B the alternative. The owner confirms after `F-002`.
- **Capture for less.** The proposal is one three-entry posting, where `DEC-2`'s text says "two
  entries in one chain". Both are atomic. The proposal matches `AC-C2.1` more literally.
- **Which error on a binding fault.** SPEC §10 has no error for "the book could not decide". A fault
  classified by `F-001` during a reserve is refused as `ERR-NotAuthorised`, on the reading that an
  authority that cannot be evaluated counts as absent (`REQ-AU-1`). The owner may prefer an
  amendment adding a named condition. `REQ-ER-1` forbids an unnamed one.
- **Growth under Option A.** Holding accounts are never deleted, because the kernel has no delete. At `NFR-2`'s volume that adds millions of leaves a year. `F-002` S3 sizes it,
  and `F-006` must carry it.
- **`DEP-4`.** An upstream primitive for moves between layers inside one account would let `reserved`
  become a layer again. The trait keeps that a local change. Nothing waits on it.
