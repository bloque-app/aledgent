<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# CipherClerk patches — randomness as a parameter, the entry currency check, and the guest's cost

**Opened, 2026-09-24.** Both were opened as pull requests on `Virto/CipherClerk`, base `master`
(table below). Two patches against CipherClerk `e9a7b08` (the commit aledgent pins), answering two
upstream questions of 2026-09-23: question 15 (randomness as a parameter), which the maintainer
accepted, and question 16 (the entry currency check), whose scope he asked to have spelt out.

| Patch | Subject | Size |
|---|---|---|
| `0001-randomness-as-a-parameter.patch` | `feat(signing): take randomness as a parameter, move OsRng to os-rng` | 34 files, +681 −92 |
| `0002-check-entry-currency.patch` | `feat(kernel): refuse an entry whose ledger differs from its account's` | 5 files, +184 (the pushed branch, PR #2, is 4 files, +172: it has no `CHANGELOG.md` hunk, so it does not depend on #1) |

| PR | Branch | Description | Open the pull request |
|---|---|---|---|
| [#1](https://codeberg.org/Virto/CipherClerk/pulls/1) | `rng-as-parameter` | [`pr/01-rng-as-parameter.md`](pr/01-rng-as-parameter.md) | [compare](https://codeberg.org/Virto/CipherClerk/compare/master...pandres95/CipherClerk:rng-as-parameter) |
| [#2](https://codeberg.org/Virto/CipherClerk/pulls/2) | `check-entry-currency` | [`pr/02-check-entry-currency.md`](pr/02-check-entry-currency.md) | [compare](https://codeberg.org/Virto/CipherClerk/compare/master...pandres95/CipherClerk:check-entry-currency) |

**Opened, 2026-10-13.** The four patches for the kernel's cost in a VOS guest, from Pablo's fork, base
`master`, after Pablo briefed Dani with this document. Each branch is cut from `rng-as-parameter` (#1),
so each pull request says "Depends on #1" up front and its diff shows #1's commit until #1 merges.
`current-precompile-shim` is cut from `vos-blake2b` instead: 0005 does not apply without 0003 (it moves
0003's `vos-protocol` pin and edits its lines in `src/crypto/hash.rs`), so #5 depends on #1 and #3. The
patches went in with `git am`; three branches carry one extra `style:` commit each, because `cargo fmt
--check` failed on the patch as written: #3 (`tests/vos_blake2b.rs`), #4 (the empty-chain table, plus
twelve `needless_borrow` warnings the patch introduced) and #5 (the imports of `src/crypto/hash.rs`).
0006 is now a patch, the prototype of `crates/compact-smt` moved into CipherClerk as `merkle_v2`, offered
as the construction with v1 kept as an opt-in. Same base (`e9a7b08` with 0001);
`tools/cipherclerk-patched.sh` applies 0001 and 0003–0005 for aledgent's guest. The series and its
numbers: [`0003-0006-guest-performance.md`](0003-0006-guest-performance.md).

| PR | Patch | Branch | Description | Open the pull request |
|---|---|---|---|---|
| [#3](https://codeberg.org/Virto/CipherClerk/pulls/3) | `0003-vos-blake2b.patch` | `vos-blake2b` | [`pr/03-vos-blake2b.md`](pr/03-vos-blake2b.md) | [compare](https://codeberg.org/Virto/CipherClerk/compare/master...pandres95/CipherClerk:vos-blake2b) |
| [#4](https://codeberg.org/Virto/CipherClerk/pulls/4) | `0004-static-empty-chain.patch` | `static-empty-chain` | [`pr/04-static-empty-chain.md`](pr/04-static-empty-chain.md) | [compare](https://codeberg.org/Virto/CipherClerk/compare/master...pandres95/CipherClerk:static-empty-chain) |
| [#5](https://codeberg.org/Virto/CipherClerk/pulls/5) | `0005-current-precompile-shim.patch` | `current-precompile-shim` | [`pr/05-current-precompile-shim.md`](pr/05-current-precompile-shim.md) | [compare](https://codeberg.org/Virto/CipherClerk/compare/master...pandres95/CipherClerk:current-precompile-shim) |
| [#6](https://codeberg.org/Virto/CipherClerk/pulls/6) | `0006-compact-smt.patch` | `compact-smt` | [`pr/06-compact-smt.md`](pr/06-compact-smt.md) | [compare](https://codeberg.org/Virto/CipherClerk/compare/master...pandres95/CipherClerk:compact-smt) |

Checked on 2026-10-13 (stable 1.98.0), each against its base branch: `cargo test --workspace
--all-features` passes 363 (#3, #4, #5) and 372 (#6) tests against #1's 362; `cargo fmt --check` is
clean; `cargo clippy --all-features --all-targets` shows #1's 31 distinct warnings on #3 and #4, 30 on
#5 and 33 on #6. Both VOS pins (`6c79431` in #3, `c54fc1f` in #5) exist only on `pandres95/VOS`, not
on `Virto/VOS`; the descriptions say so. The working drafts of 2026-10-03 are kept:
[0003](0003-vos-blake2b.md), [0004](0004-static-empty-chain.md),
[0005](0005-current-precompile-shim.md), [0006](0006-compact-smt.md).

The base branch is `master`, not `main`. The descriptions in `pr/` are the ones to use on Codeberg;
the sections below keep the full working record.

Apply in order with `git am 0001-*.patch 0002-*.patch` on `e9a7b08`. The code in 0002 does not depend
on 0001; only its `CHANGELOG.md` hunk does. Applied alone with `git apply --exclude=CHANGELOG.md`,
0002 passes the full suite too.

## 0001 — randomness as a parameter

`signing = ["dep:rand_core", "rand_core/getrandom"]` puts getrandom 0.2 in the graph of every consumer
that signs, and getrandom stops the build with `compile_error!("target is not supported")` on the VOS
guest target (`riscv64em-vos`). aledgent's core signs kernel transfers inside the guest, so it cannot
build there today. The patch makes `signing` enable `rand_core` alone. Every function that needs
randomness gets a variant that takes the caller's `R: RngCore + CryptoRng` and is always available
with `signing`: `random_with_rng` on every id type, `Account::{open,asset,liability}_with_rng`,
`Transfer::builder_with_rng`, `TransferBuilder::{new_with_rng, signed_by_with, signed_with_rng,
try_signed_with_rng}`, `proof::signature::sign_with_rng`, `sign_with_rng` on `SettlementClaim`,
`Voucher` and `ShieldedVoucher`, `disclosure::prove_*_with_rng`, `SpendKey::generate_with_rng`, and
`seal_with_rng` / `seal_note_with_rng` / `seal_note_to_address_with_rng` on the envelopes. The
`OsRng` wrappers keep their names and behaviour behind a new opt-in feature,
`os-rng = ["signing", "rand_core/getrandom"]`, and `helpers` implies it. `TransferBuilder` now draws
from its RNG once, at construction: the transfer id and a 32-byte seed. Each entry id is derived from
that seed and the entry's index (`blake2b_256`, domain `cipher-clerk/transfer-builder/entry-id/v1`,
first 16 bytes), so `debit` and `credit` need no RNG and keep their signatures. **Compatibility:** no
function is removed or changes its signature, and users of `helpers` see no change. The one break is
at compile time: a consumer that enables `signing` without `helpers` and calls an `OsRng` wrapper
(`Keypair::generate`, `AccountId::random`, `Transfer::signed_by`, `CreateAccount::signed`, and the
rest listed above) must add `os-rng`. The builder's entry ids stay unpredictable and unique, but they
are no longer drawn one by one. No deterministic-nonce signing was added: every signing path takes an
RNG. If the maintainer prefers RFC 6979-style nonces, derived from the key and the message, for `sign`, that fits
this scheme and could be a follow-up. **Tested:** `cargo build -p cipher-clerk
--no-default-features --features signing` for `riscv64em-vos` fails at `e9a7b08` (getrandom) and
succeeds with the patch. So does the same build with every roadmap feature except `os-rng`, `helpers`
and `pvm-precompile`. With `os-rng` the guest build still fails, as intended. `cargo tree -e normal`
shows no getrandom under `signing`. `cargo test --workspace --all-features` passes 362 tests, 358
before the patch plus the new `tests/rng_parameter.rs`. That file runs on `signing` alone and drives
the kernel end to end with a seeded RNG: open accounts, sign, apply, same seed gives the same bytes,
and entry ids are distinct. Default features (`--lib --tests`) pass 35 tests, and `signing` alone
passes 38. Clippy with `--all-features --all-targets` shows the same 44 warnings as `e9a7b08`, and
`cargo fmt --check` is clean.

## 0002 — the entry currency check (question 16)

The kernel checks that a transfer balances per `(ledger, layer)`, but it groups the entries by their
own `ledger` field and never compares that field with the ledger of the account each entry posts to. So
a hand-built transfer can debit Alice's USD account and credit Carol's EUR account with both entries
labelled USD. The USD group balances and the kernel accepts it, and value crosses currencies at a rate
nobody signed. Question 16 asks for exactly one check. After the kernel loads the touched accounts,
every entry's `ledger` must equal its account's `ledger`. The patch adds it in `apply_one`
(`src/kernel/apply.rs`), between the journal check and the closure and signature checks, and refuses a
mismatch with a new `EventStatus::EntryLedgerMustMatchAccount = 44`, in the cross-field block. The
shadow model in `tests/proptest_shadow.rs` mirrors the check. **Compatibility:** `TransferBuilder`
already copies `account.ledger` into each entry, so builder-made transfers are unaffected. The only
transfers now refused are hand-built ones with an entry in a currency other than its account's. The new
status value is on the wire: a decoder built before the patch does not know 44. **Tested:** the new
`tests/kernel_entry_currency.rs` covers a mismatch on the credit leg and on the debit leg (refused,
nothing stored), the matching case (accepted), and a cross-currency builder transfer (still
`UnbalancedTransfer`). Run against `e9a7b08`, the two mismatched transfers come back `Created` and the
transfer is stored. `cargo test --workspace --all-features` passes 366 tests with both patches. The
guest builds above still succeed. A naming point for the maintainer: the existing `EntryLedgerMismatch` (41) is
what the kernel returns when an entry's `transfer_id` differs from the transfer's id. We left it alone
rather than change what an existing code means, but its name reads like this check.

## What this means for aledgent (not done here)

With 0001 upstream, `ledger-kernel` drops getrandom with no code change. It only uses
`SecretKey::from_bytes`, `SecretKey::sign` with its seeded ChaCha20 (`REQ-RT-7`) and the signing
payloads, none of which move behind `os-rng`. Checked with the pin pointed at the patched tree:
`cargo tree -p ledger-kernel --no-default-features -e normal` has no getrandom, and
`ledger-kernel --no-default-features` builds for `riscv64em-vos`. At `e9a7b08` it fails on
getrandom. Its dev-dependency on `helpers` keeps its tests on `OsRng`. The follow-up, once upstream merges:

1. Bump the CipherClerk `rev` in the root `Cargo.toml`, `spikes/native`, `spikes/guest/kernel-probe`
   and `tools/ci/fixtures/forbidden-kernel-feature`.
2. `crates/verify`: its tests call `disclosure::prove_*`, which now need `os-rng`. Add `os-rng` to its
   dev-dependency features, or move the tests to `prove_*_with_rng`. This is the only compile break the
   bump causes in the crates we checked (`ledger-kernel`, `verify`, `recovery`). A grep finds no other
   `OsRng` wrapper in non-test code.
3. Update F-001 `NOTES.md` decision 21 and its open point, and the `signing` row of
   `notes/MEASUREMENTS-F002.md` §5. The guest build of `ledger-kernel` could join CI.
4. Decide how `ledger-kernel`'s `book` reports `EntryLedgerMustMatchAccount`. It compiles unchanged
   against both patches: nothing in aledgent matches `EventStatus` exhaustively.

## Reproducing the guest build

Toolchain `nightly-2026-03-20` with `rust-src`. Target spec: the `riscv64em-vos.json` that `vosx actor
new` writes at VOS `saga/agents` `7bae926` (`vosx/src/commands/new_project.rs`, `TARGET`), as in
F-002 (`spikes/README.md`). No VOS checkout or container is needed for a library build.

    cargo +nightly-2026-03-20 build -p cipher-clerk --no-default-features --features signing \
        --release --target riscv64em-vos.json \
        -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem \
        -Zjson-target-spec

A note for the maintainer: `cargo test -p cipher-clerk` with default features already fails at `e9a7b08`. The
examples use `helpers` but declare no `required-features`. The default-feature runs above therefore
use `--lib --tests`.
