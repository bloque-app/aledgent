<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Refuse an entry whose ledger differs from its account's

The kernel checks that a transfer balances per `(ledger, layer)`, but it groups entries by their own
`ledger` field and never compares that field with the ledger (currency) of the account each entry posts
to. So a hand-built transfer can move value across currencies at a rate nobody signed. This PR adds that
one check. It is question 16 from our call of 2026-09-23, where you asked what exactly it wants.

## Problem

At `e9a7b08`, `check_zero_sum` (`src/kernel/invariants.rs:19-42`) groups entries by
`(e.ledger, e.layer)` taken from the entry itself (`:22`); `apply_one` calls it at
`src/kernel/apply.rs:259-262`. It then loads the touched accounts (`:264-277`) and checks that they belong
to the transfer's journal (`:278-282`), but nothing compares `entry.ledger` with `account.ledger`.

Alice has a USD account and Carol a EUR account. A transfer debits Alice and credits Carol with both
entries labelled USD. The USD group balances, the kernel accepts it, and USD becomes EUR one for one.
Checked on `e9a7b08`:

| Transfer | Result at `e9a7b08` |
|---|---|
| credit leg posts to an account of another currency | `Created`, stored |
| debit leg posts to an account of another currency | `Created`, stored |

`TransferBuilder` copies `account.ledger` into each entry, so this only happens with hand-built entries.
The kernel is still the place that should refuse them, since it is the one component every transfer goes
through.

## Change

One commit, 4 files, +172; 153 lines are tests.

- **`src/kernel/apply.rs`** (+14, `:284-296`): a new section, "currency: every entry posts in its
  account's ledger". For each entry, the loaded account must have the same `ledger`, or the transfer is
  refused with the new status. It runs after the zero-sum check, the account load and the journal check,
  and before the closure check, signatures, imported-timestamp and negativity checks.
- **`src/error.rs`** (+5): `EventStatus::EntryLedgerMustMatchAccount = 44`, in the cross-field block after
  `VoidEntriesMustMirror = 43`, with a doc comment.
- **`tests/proptest_shadow.rs`** (+5): the shadow model mirrors the check, after its journal check.
- **`tests/kernel_entry_currency.rs`** (+148, new).

## Design choices and alternatives

- **One equality check after loading the accounts**, rather than grouping the zero-sum by the account's
  ledger. Grouping by account would make the mislabelled transfer fail as `UnbalancedTransfer`, which
  hides the actual fault; an explicit status says what is wrong.
- **A new status, 44, not a reuse of 41.** The existing `EntryLedgerMismatch` (41, `src/error.rs:47`) is
  what the kernel returns when an entry's `transfer_id` differs from the transfer's id
  (`apply.rs:233-234`). Its name reads like this check, but changing what an existing code means would
  break every consumer that decodes it. We left it alone (question 1).
- **Position before signatures.** A mislabelled transfer is refused on its structure alone, before any
  signature work.

## Compatibility

- **Builder-made transfers are unaffected**: the builder already copies each account's ledger.
- **Newly refused:** hand-built transfers with an entry in a currency other than its account's. Nothing is
  stored for them. Where such a transfer previously failed on a later check (a closed account, signature
  count or validity, negativity, an imported timestamp), it now returns 44 instead. Unbalanced transfers
  and missing accounts still return their old codes first.
- **Wire format: one new status value.** `EventStatus` archives as a one-byte tag equal to its
  discriminant, so 44 is `0x2c`; every existing value is unchanged. A decoder built before this PR
  rejects tag 44 during validation rather than misreading it.
- **Rust source:** `EventStatus` is not `#[non_exhaustive]`, so an exhaustive `match` in a downstream
  crate needs a new arm.
- **History.** A mismatched transfer already stored before this check behaves differently afterwards:
  voiding it goes through `apply_one` and now fails with 44; a pending transfer created before the
  upgrade can still be finalised, because `apply_pending_finalize` (`apply.rs:168-169`) runs before the
  new check; and replaying history that contains one gives different results and state roots. Nothing
  in our use has such a transfer, but a deployment with history should check.

## Tests

New, `tests/kernel_entry_currency.rs` (`helpers`):

- `credit_leg_in_another_currency_is_refused`: status 44 and nothing stored;
- `debit_leg_in_another_currency_is_refused`: status 44;
- `entries_in_their_accounts_currency_are_accepted`;
- `builder_transfers_carry_the_accounts_currency`: a cross-currency builder transfer is still
  `UnbalancedTransfer`.

Re-run on 2026-09-29 (stable 1.98.0):

| Command | Result |
|---|---|
| `cargo test --workspace --all-features`, this PR alone | 362 passed (358 at `e9a7b08`) |
| the same with #1 merged | 366 passed |
| `cargo clippy --all-features --all-targets` | the same 44 warnings as `e9a7b08` |
| `cargo fmt --check` | clean |

With the check removed (at `e9a7b08`), both mismatched transfers come back `Created` and are stored.

**Not run:** the debit-leg test asserts the status but not that nothing was stored; the shadow model's
check is never exercised by the property tests, because their generator puts every account and entry in
ledger 840. A generator with several ledgers would cover it.

## Relation to the other pull request

Cut from `e9a7b08`, which is still `master`. Independent of #1: it shares only
`tests/proptest_shadow.rs`, in a different place. `git merge-tree` merges the two cleanly in either order
to the same tree. Each applies alone to `master`, and no merge order is required.

## Questions for you

1. **Naming.** `EntryLedgerMismatch` (41) names the transfer-id check; you may prefer to rename 41 and
   give this check the clearer name, accepting the source break.
2. **The value 44** in the cross-field block: right place?
3. **Stored history.** Should voids and pending finalisation of a transfer stored before this check be
   exempt, or is refusing them the intended outcome?

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent and decides each
balance-affecting command inside the guest, within a completion budget. It holds balances in several
currencies, and the kernel's decision is the one every downstream record relies on. We build our entries
with the builder, but a ledger's core invariant should not depend on every caller doing so: balanced per
currency must mean per the accounts' currencies.
