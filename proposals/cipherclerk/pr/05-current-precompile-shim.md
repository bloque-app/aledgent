<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# build(precompile): the current VOS precompile shim for pvm-precompile

**Depends on #1 and #3.** This branch is cut from #3's branch (`vos-blake2b`), which is cut from #1's
(`rng-as-parameter`), so until they merge its diff also shows their commits. The change of this PR is its
last two commits: `build(precompile): the current VOS precompile shim for pvm-precompile` and a
`cargo fmt` of the imports it touches. It needs #3: it moves #3's `vos-protocol` pin and edits the lines
#3 adds to `src/crypto/hash.rs`.

**This targets the current VOS precompile interface, and may change with the new VOS.** It points
`pvm-precompile` at VOS's current shim, `vos-pvm-precompiles` (pinned on our VOS fork), so that a guest
built with the feature runs at all. When the
new VOS settles how a guest reaches the curve and hash precompiles, this dependency will likely move
again; we are not proposing it as the long-term interface.

## Problem

At `e9a7b08`, `pvm-precompile` takes `zkpvm-precompiles` from `Virto/VOS` at `9f5337f`. That shim emits a
bare `ecall`, and today's PVM compiler translates a bare `ecall` into a trap. So a guest built with
`pvm-precompile` panics at its first curve operation. Without the feature the guest does its curve
arithmetic in software: in a VOS PVM guest a Ristretto scalar multiplication costs about 3.9 M gas, and
curve arithmetic is 17 % of one reservation's actor gas in our ledger's core.

## Change

Two commits, 3 files, +46 −12: the change, and `cargo fmt` ordering the imports in `src/crypto/hash.rs`.

- **`Cargo.toml`.** The `zkpvm-precompiles` dependency becomes
  `{ package = "vos-pvm-precompiles", git = "https://codeberg.org/pandres95/VOS.git", rev = "c54fc1f" }`:
  the current shim, with the same API, which marks its ECALLs (`csrw 0x801`) so the PVM compiler emits a
  host call instead of a trap. The crate keeps its local name, so no `use` changes. `vos-protocol`
  (from #3) moves to the same revision.
- **`Cargo.lock`.** `vos-pvm-precompiles` and `vos-protocol` at `c54fc1f`, with `blake2b_simd`,
  `arrayvec` and `constant_time_eq`.
- **`src/crypto/hash.rs`.** `blake2::Digest` is imported only when neither `pvm-precompile` nor
  `vos-blake2b` is on, so the feature builds without an unused import.

The precompiles the shim calls (110 to 114, the curve operations) are served by Agent runtimes on our
VOS fork's branch `agent/execution-limits`, natively with the runtime feature `native-curve`. Served
natively, a Ristretto scalar multiplication costs about 250 k gas instead of 3.9 M.

## Design choices and alternatives

- **Keep the crate's local name** (`zkpvm-precompiles`, via `package =`), so this is a dependency change
  and not a code change.
- **Pinned on our VOS fork.** Revision `c54fc1f` exists on `codeberg.org/pandres95/VOS`, not yet on
  `Virto/VOS`. Both pins should move to `Virto/VOS`, or to whatever interface the new VOS settles on
  (question 1).

## Measured

Actor gas per reservation in our core, on the VOS-native runtime, one change at a time:

| Patch | Actor gas per reservation |
|---|---:|
| before | 293.5 M |
| #4, the static empty-subtree chain | 256.5 M (−12.6 %) |
| #3, VOS's BLAKE2b | 227.5 M (−22.5 %) |
| #3 and #4 | 204.4 M (−30.4 %) |
| **and this PR, with VOS's native curve calls** | **154.0 M** |
| and the guest at `opt-level = 3` (our build) | 104.8 M (−64 %) |

On one hosted runner (AMD EPYC 7763), at batch 128 under the Agent's 8 G limits, the core goes from 26.5
reservations a second with #3 and #4 to 30.7 with this PR. Gas is exact and the same on every machine.
The native curve calls are outside JAM's host calls: under a JAM-compatible runtime a guest leaves
`pvm-precompile` off and does its curve arithmetic in software.

## Compatibility

- **Only `pvm-precompile` builds change.** Without the feature, nothing changes. With it, the same API
  and the same results; only the dependency's source and revision differ.
- **The runtime must serve 110 to 114**, or the guest's first curve call fails. That was already true of
  the feature; the old shim simply never reached the runtime.

## Tests

Run on 2026-10-13 (stable 1.98.0; the repository pins no toolchain), against #3's branch as the base:

| Command | #3 | This PR |
|---|---:|---:|
| `cargo test --workspace --all-features` | 363 passed | 363 passed |
| `cargo test --lib --tests` (default features) | 36 passed | 36 passed |
| `cargo clippy --all-features --all-targets` | 31 distinct warnings | 30 (the unused `Digest` import is gone) |
| `cargo fmt --check` | clean | clean |

Off the guest, the shim falls through to dalek, so the tests exercise the same code path as before. The
guest run (a reservation with the curve served natively) is the measurement above; it was not repeated
for this PR.

## Relation to the other pull requests

Stacked on #3 (and so on #1). Independent of #2, #4 and #6.

## Questions for you

1. **The interface.** Is it worth merging this for the current VOS, so that `pvm-precompile` works
   today, or would you rather wait for the new VOS's interface and drop this?
2. **The pins.** Both VOS dependencies point at our fork. Which `Virto/VOS` revision should they move to,
   and when?

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent and decides each
balance-affecting command inside the guest, within a completion budget. With `pvm-precompile` broken, its
guest pays for every curve operation in software. This makes the feature usable on today's VOS.
