<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-001 — implementation notes

Written while implementing `F-001` (issue #1), 2026-09-22. The plan's proposals were followed as
accepted. Where the plan was silent, the most conservative behaviour was chosen (refuse rather than
allow); each such choice is listed here for review.

## Decisions taken during implementation

### Storage trait

1. **One concrete `StoreError`, not an associated `type Error`.** The plan sketches
   `type Error: Debug`. The binding has to tell a refused write (`StoreError::Refused(WriteRefusal)`)
   from an engine fault (`Io`, `Corrupt`) and from an external-id slot collision, and the conformance
   store cases assert the exact refusal. `F-006` maps its engine errors into `StoreError::Io`.
2. **The trait gained `next_posting_seq` and `log_record`**, and `BookDelta` carries the posting
   records. The store refuses a delta whose posting sequence numbers do not continue its own without a
   gap, or that adds a transfer without exactly one posting record, so `REQ-PO-2` holds at the store,
   not only in the binding.
3. **Accounts only roll.** The plan's rule is "insert, then overwrite (balances roll)". The store
   refuses any account overwrite that changes anything but the balances, the timestamp, and the
   gaining of `CLOSED`: identity, journal, key, asset, code, normal balance, user data and the other
   flags are immutable, and `CLOSED` is never cleared. This is exactly what the kernel does, and it
   keeps a raw write from rewriting an account (`INV-2`).
4. **Pending status in one delta.** `Pending → Posted | Voided` only. A status may first appear as
   `Posted` or `Voided` only when the same delta adds the `PENDING` transfer itself (a pending
   transfer and its finalisation in one unit). Unused under `DEC-2`; kept faithful for the root.
5. **Leaves are re-hashed on load** in `FileStore` and compared with the committed leaf node; a
   mismatch is `StoreError::Corrupt`.

### Identifiers, keys and randomness

6. **Derivations the plan did not give**, all domain-separated blake2b, first 16 bytes:
   transfer id `H("aledgent/transfer-id/v1" ‖ external id)`, so a repeated posting reuses the id and
   the kernel refuses it (`TransferIdAlreadyExists`, which is checked before the external id; both are
   classified `Duplicate`); entry id `H("aledgent/entry-id/v1" ‖ transfer id ‖ index)`; journal id
   `H("aledgent/journal-id/v1" ‖ registrar key)`, so one core root gives one book.
7. **Blindings** are keyed on (operation reference, **posting kind**, ordinal, entry index) — the kind
   is added to the plan's triple so two postings of different kinds can never share blindings. The
   last entry of each asset has its blinding solved so the asset's commitments balance.
8. **Signature nonces** come from ChaCha20 seeded by `H(root ‖ signer ‖ H(message))`: deterministic,
   message-bound and secret. Registrar signatures use the same scheme.
9. **`KernelAccountSpec` takes a `BalanceRule`** (`NeverNegative` | `Unchecked`) instead of raw
   `AccountFlags`, so `ISSUER` and `CLOSED` cannot be requested. The role's code is stored as the
   kernel account's `code`; `user_data` stays zero.
10. **Every entry the binding builds is on the settled layer.** Reserved and encumbered value are
    positions (`DEC-2`, `F-004`); the `Pending` and `Encumbrance` layers stay zero.

### Apply and classification

11. **A creation the kernel refuses is a `BindingFault`.** Creating an existing account
    (`AccountIdAlreadyExists`) falls under "anything else" in the plan's table. Callers read
    `Book::kernel_account` and create only what is absent (`F-003`'s lazy sub-accounts).
12. **A refused step does not stop the unit**; later steps see the real post-state, as the plan
    describes. A `BindingFault` discards the whole unit and nothing is committed.
13. **`Insufficient` names the first flagged account** whose plaintext net went below zero after the
    refused event.
14. **`BindingFault::OutOfRange`.** When the kernel refuses a net that is not negative but is above
    `u64::MAX` (no `u64` opening exists), the fault says so instead of `Oracle`. The amount is
    refused, never wrapped (`REQ-KER-7`). This is the range Open point made visible.
15. **`StepOutcome` carries the kernel's raw results** next to the labels, for audit and for the
    differential test.
16. **Raw `KernelPort::submit` splits the batch chain by chain**, as `Book::apply` does, with the
    same stamps as one batch; an open chain is passed to the kernel whole and refused by it.

### Durability

17. **The vault keeps only the undo record of its last commit.** It is never more than one commit
    ahead of the store. `Book::open` rolls it back by one if needed and refuses (`VaultDiverged`) if
    it is further ahead or behind. A failed store commit reverts the vault in process.
18. **`FileVault` is a separate redb file** from the leaf store (`REQ-RT-6`).
19. **The 10 000-case differential runs `FileStore` and `FileVault` on redb's in-memory backend** —
    the same code without the fsync — so it finishes in about a minute. The disk is covered by the
    file-backed tests: rule suite after reopen, crash injection, golden vectors on a file.

### Crate shape

20. **`ledger-kernel` is `no_std + alloc`** with a default `std` feature for `FileStore` and
    `FileVault`, so the binding itself can run in the core's guest (`F-005`). CI lints both
    configurations.
21. **`signing` is enabled on `cipher-clerk`** (needed for `SecretKey` and `sign`, `DEC-3`). It pulls
    `rand_core/getrandom` into the graph; nothing here calls the OS generator. Whether it builds for
    the guest is `F-002`'s to confirm (Open point, unchanged).

### Apply cost (issue #37)

22. **`MemStore` no longer holds CipherClerk `SparseMerkleTree`s** (plan §10). That tree's `root()`
    rebuilds the root from every leaf on each call, and `MemStore` asked for one per touched
    sub-tree on every commit; since each posting adds a transfer and an external-id leaf, one
    posting cost about 5 ms on an empty book and over 60 ms after 1 600. `MemStore` now keeps each
    sub-tree as an in-memory node table updated along the written leaf's path (`store::smt`, the
    same code as `FileStore`), so a commit costs the same at any size. Roots are unchanged: the
    differential test still checks them against `MemLedger` and against `recomputed_root`, which
    still uses the kernel's `composite_state_root`. The price is memory: 129 nodes per leaf, where
    the kernel's tree kept one; the 10 000-posting test peaks at about 310 MB. `book::cost_tests`
    asserts the flat cost on both stores.

## Conformance (T-001-09)

- `conformance-ports` exposes `KernelPort` (implemented by `Book`) and `StorePort` (implemented for
  every `LeafStore`). `tools/ci/check-conformance-ports.sh` fails if any workspace member other than
  `conformance` enables the feature over normal or build edges; a CI self-test proves it rejects a
  fixture crate that does.
- `tests/conformance_m0.rs` holds the kernel and store cases of `INV-1`, `INV-2`, `INV-3` (kernel
  part), `INV-4`, `INV-7` and `INV-25`, on both stores; the `conformance-m0` CI job runs them.
  `F-015` owns the `m0` gate profile and the case registry (`TODO(F-015)` in that file).
- **`INV-4` at kernel strength is not claimed.** The case
  `inv_4_open_point_kernel_does_not_check_entry_asset_against_account__kernel` pins today's kernel
  behaviour: an entry tagged with another asset than its account's is accepted. The builders cannot
  express it. The owner's choice between a pinned fork and an upstream patch (plan Open points,
  `RISK-3`) is still open; when the check lands, that case flips to a refusal.

## Deferred

- `TODO(F-015)`: register the M-0 cases in the suite and replace the `conformance-m0` job with the
  `m0` gate profile.
- `F-006`: the production leaf store, history and indexes. `FileStore` stays the durability
  reference.
- `F-005` / `F-006`: the production home of the openings vault.
