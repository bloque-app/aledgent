<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# perf(crypto): hash with VOS's BLAKE2b behind vos-blake2b

**Depends on #1.** This branch is cut from #1's branch (`rng-as-parameter`), so until #1 merges its diff
also shows #1's commit. The change of this PR is its last two commits: `perf(crypto): hash with VOS's
BLAKE2b behind vos-blake2b` and a `cargo fmt` of its test. They use nothing #1 adds; they sit on #1 because
that is the base we measured and the one our guest builds.

A new opt-in feature, `vos-blake2b`, routes `crypto::blake2b_256` and `crypto::blake2b_512` through
`vos_protocol::blake2b::digest`. In a VOS PVM guest that hashes a block for about a third of the gas of
the `blake2` crate. The digests do not change, and without the feature nothing changes.

## Problem

In a VOS PVM guest, one reservation in our ledger's core costs about 295 M gas of actor work. A
per-program-counter profile of one 16-reservation invocation puts about 46 % of it in CipherClerk's
`blake2b_256`: **8 470 one-block hashes per reservation**, about 16 k gas each. That is the `blake2`
crate's compression plus the `digest` wrapper's per-hash buffer copies (`memcpy` and `memset` alone are
26 % of the actor's gas). Almost all of those hashes are sparse-Merkle path nodes. Another 17 % is curve
arithmetic (#5 is about that).

## Change

Two commits, 3 files, +63 −4: the change, and `cargo fmt` on its new test (one array literal wrapped).

- **`Cargo.toml`.** A new feature, `vos-blake2b = ["dep:vos-protocol"]`, and an optional dependency,
  `vos-protocol` (no_std; its only dependencies are `blake2b_simd` and `rkyv`).
- **`src/crypto/hash.rs`.** With `vos-blake2b`, `blake2b_256` and `blake2b_512` return
  `vos_protocol::blake2b::digest::<32>` and `::<64>` over the same domain and parts. In a PVM guest that is
  a compression scheduled for the PVM's 13 registers and 25-cycle memory accesses, about 4 800 gas a
  block, streaming whole blocks without copies; on a host that serves VOS's BLAKE2b precompile (100) it is
  the precompile. Off the guest it is `blake2b_simd`. If both `vos-blake2b` and `pvm-precompile` are on,
  `vos-blake2b` hashes; `pvm-precompile` keeps hashing through `zkpvm_precompiles::blake2b_hash` when it
  is on alone. Without either feature the `blake2` crate hashes, as today.
- **`tests/vos_blake2b.rs`** (new). Holds both functions to the `blake2` crate at every block boundary
  (lengths 0 to 1 000, three ways of splitting the parts), with or without the feature.

## Design choices and alternatives

- **A feature, not a replacement.** Native consumers keep the `blake2` crate and its dependency graph; only
  a guest that wants VOS's hashing opts in.
- **The same digests.** Nothing that is hashed changes: roots, proofs, signatures and ids are identical
  with and without the feature, so a guest with it and a host without it agree.
- **Where the dependency points.** `vos-protocol` is pinned at `6c79431` on our VOS fork
  (`codeberg.org/pandres95/VOS`), because the BLAKE2b module it needs is not in `Virto/VOS` yet. The pin
  should move to `Virto/VOS` once it is there (question 1).

## Measured

Actor gas per reservation in our core, on the VOS-native runtime, one change at a time:

| Patch | Actor gas per reservation |
|---|---:|
| before | 293.5 M |
| #4, the static empty-subtree chain | 256.5 M (−12.6 %) |
| **this PR** | **227.5 M (−22.5 %)** |
| this PR and #4 | 204.4 M (−30.4 %) |
| and #5, with VOS's native curve calls | 154.0 M |
| and the guest at `opt-level = 3` (our build) | 104.8 M (−64 %) |

On one hosted runner (AMD EPYC 7763), at batch 128 under the Agent's 8 G limits, the core goes from 15.5
to 23.9 reservations a second with this PR alone, 26.5 with #4 too, and 33.4 with every step above. Gas is
exact and the same on every machine.

## Compatibility

- **Off by default.** Without `vos-blake2b` the code and the dependency graph are unchanged.
- **With it:** the same digests, so no wire format, root or signature changes.
- **`Cargo.lock`.** This commit does not add `vos-protocol` to `Cargo.lock`, so the first build with
  `--all-features` (or `vos-blake2b`) writes it there. #5 carries the lock entries, at its own pin.

## Tests

Run on 2026-10-13 (stable 1.98.0; the repository pins no toolchain), against #1's branch as the base:

| Command | #1 | This PR |
|---|---:|---:|
| `cargo test --workspace --all-features` | 362 passed | 363 passed |
| `cargo test --lib --tests` (default features) | 35 passed | 36 passed |
| `cargo clippy --all-features --all-targets` | 31 distinct warnings | the same 31 |
| `cargo fmt --check` | clean | clean |

The new test runs in both: with `vos-blake2b` in the all-features run, and on the `blake2` crate in the
default one.

## Relation to the other pull requests

Stacked on #1 only. Independent of #2, #4 and #6. #5 builds on this one: it moves `vos-protocol` to the
same VOS revision as the new precompile shim.

## Questions for you

1. **The VOS pin.** Should `vos-protocol` wait for the BLAKE2b module to land in `Virto/VOS`, or is a pin
   on our fork acceptable meanwhile?
2. **Precedence.** With both `vos-blake2b` and `pvm-precompile`, this PR hashes with VOS's BLAKE2b. Would
   you rather `pvm-precompile` imply it?

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent and decides each
balance-affecting command inside the guest, within a completion budget. Its throughput there is bound by
gas, and almost half of that gas is CipherClerk's hashing. This is the largest single step we found.
