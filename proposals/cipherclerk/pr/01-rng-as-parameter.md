<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Take randomness as a parameter; move OsRng behind `os-rng`

The `signing` feature pulls in getrandom, which refuses to compile for the VOS guest target, so no guest
that signs can build CipherClerk. This PR makes `signing` enable `rand_core` alone, gives every function
that needs randomness a variant that takes the caller's RNG (random number generator), and moves the
operating-system RNG (`OsRng`) wrappers behind a new opt-in feature, `os-rng`. It answers the upstream question of
2026-09-23 on taking randomness as a parameter.

## Problem

At `e9a7b08`, `Cargo.toml:20` reads `signing = ["dep:rand_core", "rand_core/getrandom"]`. That puts
getrandom 0.2.17 in the graph of every consumer that signs (`rand_core 0.6.4 → getrandom 0.2.17` in
`cargo tree --no-default-features --features signing -e normal`), and getrandom stops the build with
`compile_error!("target is not supported")` on `riscv64em-vos`, the VOS guest target.

Every non-test use of randomness under `signing` goes to `OsRng`: `random()` on the six id types
(`src/ids.rs:36-41`), `Keypair::generate` (`src/crypto/sig.rs:249-250`), `CreateAccount::signed`
(`src/kernel/create_account.rs:36-37`), `proof::signature::sign` (`src/proof.rs:105-106`),
`Transfer::signed_by` (`src/types/transfer.rs:189-190`), and through `*::random()`:
`Account::{open,asset,liability}`, `Transfer::builder`, `TransferBuilder::new`, its `debit` and `credit`
(which draw an `EntryId` each) and its `signed_by`, `signed_with` and `try_signed_with`; plus the four
`disclosure::prove_*`, `SettlementClaim::sign`, `Voucher::sign`, `ShieldedVoucher::sign`,
`SpendKey::generate` and the three `seal*` functions in `src/viewing_keys.rs`.

| Build for `riscv64em-vos` | `e9a7b08` | This PR |
|---|---|---|
| `--no-default-features --features signing` | fails (getrandom) | builds |
| every roadmap feature except `os-rng`, `helpers`, `pvm-precompile` | fails | builds |
| with `os-rng` | fails | fails, as intended |

## Change

One commit, 34 files, +681 −92. Most test files change one `cfg` line.

- **`Cargo.toml`.** `signing = ["dep:rand_core"]` (`:24`). New `os-rng = ["signing", "rand_core/getrandom"]`
  (`:30`). `helpers = ["signing", "os-rng"]` (`:33`).
- **21 new public `*_with_rng` functions**, each gated on `signing` and taking `rng: &mut R` last, with
  `R: RngCore + CryptoRng`:
  - `random_with_rng` on every id type (`src/ids.rs:44`);
  - `Account::{open,asset,liability}_with_rng` (`src/types/account.rs`);
  - `Transfer::builder_with_rng`, `TransferBuilder::{new_with_rng, signed_by_with, signed_with_rng,
    try_signed_with_rng}` (`src/types/transfer.rs`);
  - `proof::signature::sign_with_rng`; `sign_with_rng` on `SettlementClaim`, `Voucher`, `ShieldedVoucher`;
  - `disclosure::prove_{balance_at_least,transfer_existed,transfer_amount_equals,aggregate_in_window}_with_rng`;
  - `SpendKey::generate_with_rng`, `seal_with_rng`, `seal_note_with_rng`, and
    `seal_note_to_address_with_rng` (which also needs `stealth-experimental`), in `src/viewing_keys.rs`.
- **The `OsRng` wrappers** listed above keep their names, signatures and behaviour behind `os-rng`, each a
  one-line shim over its `*_with_rng` variant. The existing `Keypair::generate_with`,
  `CreateAccount::signed_with` and `Transfer::signed_by_with` are unchanged.
- **`TransferBuilder` draws from its RNG once**, at construction: a 32-byte seed, then the transfer id.
  Each entry id is the first 16 bytes of `blake2b_256` over the domain
  `cipher-clerk/transfer-builder/entry-id/v1`, the seed and the entry's index as a little-endian `u64`
  (`next_entry_id`, `transfer.rs:336-342`). So `debit` and `credit` need no RNG and keep their
  signatures. The builder gains a private `entry_id_seed` field.
- `CHANGELOG.md` (a migration table and the derivation), `docs/architecture/zkvm-integration.md`, one doc
  comment in `state_root.rs`. Unit-test modules and 13 integration-test files that use `OsRng` wrappers
  are re-gated from `signing` to `os-rng`.

## Design choices and alternatives

- **A parameter, not a global.** A caller-supplied `RngCore + CryptoRng` works on every target, and the
  guest decides where its randomness comes from (in VOS, eventually the platform's randomness source).
  A global hook (`getrandom`'s custom backend) would hide the source and still link getrandom.
- **Keep the old names behind a feature** rather than removing or renaming them. No existing call site
  changes; only consumers that sign without `helpers` must opt in.
- **Derive entry ids from a per-builder seed** rather than drawing each from the RNG. Drawing each would
  change the signatures of `debit` and `credit`. The derived ids stay unpredictable and unique within a
  transfer; transfer ids are still drawn directly.
- **Not in this PR: deterministic nonces.** Every signing path still takes an RNG. RFC 6979-style nonces
  (derived from the key and the message) fit this scheme and could be a follow-up.

## Compatibility

- **No function is removed and no signature changes.** Users of `helpers` see no change. Wire formats,
  kernel behaviour and the format of every id are unchanged.
- **One compile-time break.** A consumer that enables `signing` without `helpers`, directly or through
  `fmd-experimental` or `stealth-experimental` (which imply `signing` but not `os-rng`), and calls an
  `OsRng` wrapper (`Keypair::generate`, `AccountId::random`, `Account::open`, `Transfer::builder`,
  `Transfer::signed_by`, `CreateAccount::signed`, `proof::signature::sign`, and the rest in the
  CHANGELOG table) must add `features = ["os-rng"]`, or move to the `*_with_rng` variants.
- **Builder entry ids** are now derived rather than drawn one by one. A caller that reuses one seeded RNG
  across calls must make sure it does not repeat, or transfer ids repeat too (the kernel refuses a
  duplicate with `TransferIdAlreadyExists`).
- `docs/src/integration/embedding.md:101` (the feature table) and `docs/src/quickstart.md` still describe
  `signing` as before and have no `os-rng` row. Quickstart uses `helpers`, so it still works; we can
  update the table in this PR if you want.

## Tests

New: `tests/rng_parameter.rs`, on `signing` alone, with a seeded BLAKE2b-counter RNG:
`kernel_accepts_a_transfer_built_with_a_caller_rng` (open accounts, sign, apply),
`same_seed_gives_the_same_transfer`, `builder_entry_ids_are_distinct`, and
`envelope_seals_with_a_caller_rng` (also needs `viewing-keys`).

Re-run on 2026-09-29 (stable 1.98.0; the repository pins no toolchain):

| Command | `e9a7b08` | This PR |
|---|---:|---:|
| `cargo test --workspace --all-features` | 358 passed | 362 passed |
| `cargo test --lib --tests` (default features) | 35 passed | 35 passed |
| `cargo test --no-default-features --features signing --lib --tests` | 79 passed | 38 passed |
| `cargo clippy --all-features --all-targets` | 44 warnings (31 distinct) | the same 44 |
| `cargo fmt --check` | clean | clean |

The `signing`-only row drops because 13 integration files use `OsRng` wrappers and now need `os-rng`; the
all-features run covers them. `cargo tree -e normal` shows no getrandom under `signing`.

The guest builds in the table above were run on 2026-09-24 with `nightly-2026-03-20` and `rust-src`, and
the `riscv64em-vos.json` that `vosx actor new` writes:

    cargo +nightly-2026-03-20 build -p cipher-clerk --no-default-features --features signing \
        --release --target riscv64em-vos.json \
        -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem \
        -Zjson-target-spec

**Not run:** `cargo test -p cipher-clerk` with default features. It already fails at `e9a7b08`: the
examples use `helpers`, `notes`, `viewing-keys`, `voucher`, `fmd-experimental` and
`stealth-experimental` but declare no `required-features`. The guest builds were not repeated on
2026-09-29.

## Relation to the other pull request

Cut from `e9a7b08`, which is still `master`. Independent of #2 in code and in files except
`tests/proptest_shadow.rs`, which both touch in different places. `git merge-tree` merges the two cleanly
in either order to the same tree, and with both applied `cargo test --workspace --all-features` passes
366 tests. No merge order is required.

## Questions for you

1. The feature name `os-rng`, and whether `helpers` should keep implying it.
2. Derived entry ids from a per-builder seed, or drawing each from the RNG (which changes the signatures
   of `debit` and `credit`)?
3. RFC 6979-style deterministic nonces for `sign` as a follow-up?
4. Adding `required-features` to the examples so that default-feature `cargo test` passes. Not in this PR.
5. Should CI keep a `signing`-only job? Its coverage drops from 79 to 38 tests unless it also enables
   `os-rng` for the test targets.

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent and decides each
balance-affecting command inside the guest, within a completion budget. Its kernel signs CipherClerk
transfers there, so it must build CipherClerk with `signing` for `riscv64em-vos`. Today it cannot. With
this PR our kernel crate builds for the guest with no code change, since it signs with a seeded
generator that we supply.
