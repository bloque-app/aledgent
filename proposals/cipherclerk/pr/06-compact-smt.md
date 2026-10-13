<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# feat(merkle): a compact sparse Merkle tree with v2 roots

**Depends on #1.** This branch is cut from #1's branch (`rng-as-parameter`), so until #1 merges its diff
also shows #1's commit. The change of this PR is its last commit, `feat(merkle): a compact sparse Merkle
tree with v2 roots`. That commit uses nothing #1 adds; it sits on #1 because that is the base our guest
builds.

This PR adds a compact sparse Merkle tree, `cipher_clerk::merkle_v2`, over the same 128-bit keys and the
same leaf hashes as `SparseMerkleTree`, and a composite state root over it,
`state_root::composite_state_root_v2`. It is the tree our ledger uses by default since 2026-10-05 (its
"v2 roots"). **We
offer it as CipherClerk's construction, with the full-depth tree (v1) kept as an opt-in.** This PR is
additive: v1 and every existing root, proof and function are unchanged, and nothing calls v2 yet. If you
agree with the direction, a follow-up would make v2 what `MemLedger::root` and
`SuccinctTransitionWitness` produce by default, with v1 behind a feature.

## Problem

Every path in `SparseMerkleTree` is 128 node hashes, almost all of them over one non-empty child and an
empty sibling. #4 stops rebuilding the empty-subtree chain, but each touched key still costs 128 hashes
to prove and 128 to recompute. A compact tree hashes only branching levels, so a path is about
log₂ n + 1 hashes. Counted on random 128-bit keys:

| Leaves n | Keys touched | Full-depth path hashes | Compact | Fewer |
|---:|---:|---:|---:|---:|
| 1 000 | 8 | 999 | 65 | 15× |
| 100 000 | 8 | 1 003 | 127 | 7.9× |
| 1 000 000 | 8 | 1 002 | 144 | 7.0× |

A full-depth root commits to the 128-level chain, and no shortcut reproduces it without computing that
chain. So a compact tree cannot keep today's roots: it needs a versioned root.

## Change

One commit, 4 files, +1 114, nothing removed.

- **`src/merkle_v2.rs`** (new, +650). The definition: over a set of `(key, leaf hash)` (a leaf whose hash
  is `SMT_EMPTY_LEAF` is absent), an empty subtree hashes to 32 zero bytes; a subtree with one leaf is its
  shortcut node `leaf_node(key, leaf hash)`, the same at every depth; anything else is
  `node(left, right)`. Domains `cipher-clerk/smt/leaf-node/v2` and `cipher-clerk/smt/node/v2`. This is
  the hashing of Diem's and Aptos's Jellyfish Merkle tree over a binary radix, with CipherClerk's 16-byte
  keys and leaf hashes.
  - `root_sorted`, `summary_sorted`: the root over sorted leaves.
  - `prove` and `Proof::verify`: inclusion and non-inclusion proofs, a list of siblings whose length fixes
    the position, so a verifier needs no levels.
  - `MultiProof`: a witness multiproof for a set of touched keys, the frontier of untouched non-empty
    subtrees beside their paths. With the touched leaves before and after a transition it gives the root
    before and after. A frontier entry is a lone leaf given whole, a node hash taken on trust (sound for
    trees that never remove a leaf), or a node opened one level (for trees that do).
  - `PackedProof`: the multiproof's wire form, one 50-byte record a frontier node as in v1's, plus a kind
    byte.
  - `composite_root`: the six sub-roots folded under the domain `cipher-clerk/state-root/v2` with a
    version byte, `2`, so a v2 root never equals a v1 root.
- **`src/state_root.rs`** (+69). `composite_state_root_v2`: the same six leaf sets as
  `composite_state_root`, each sub-root a compact tree's. A test checks it against the trees and against
  v1.
- **`src/lib.rs`** (+1). `pub mod merkle_v2`.
- **`tests/merkle_v2.rs`** (new, +394). An independent reference that builds the tree bottom-up over all
  128 levels, as the definition reads, sharing nothing with the implementation but the two hash
  functions. Hand-built small trees and a thousand leaves against the reference; property tests (64
  cases each) for roots in any order, inclusion and exclusion proofs, multiproofs giving the roots before
  and after, and tampered multiproofs refused; where an opaque entry is bound
  (`opaque_entries_are_bound_only_where_the_touched_side_is_empty`); the hash count against v1's; packed
  proofs round-tripping at v1's size.

## Design choices and alternatives

- **The standard compact tree.** One leaf-node hash and one node hash a branching level; the root is a
  function of the set of leaves alone; an empty subtree costs nothing. A Patricia trie that also skips
  one-child internal nodes would save about one hash a path more, but its nodes would then have to commit
  to their levels for non-inclusion to be sound, a format of our own.
- **Leaves do not change.** v2 hashes the same leaf hashes (`smt_leaf_hash`), so v2 roots can be
  recomputed from any v1 state at any time. A deployment migrates by recomputing, not by rewriting data.
- **v1 stays supported beside v2, as an opt-in, not only to verify roots already published.** v1 is not
  weaker, only costlier. A deployment may choose it, for instance at genesis.
- **Additive first.** This PR does not switch any existing function to v2, so it can merge without a
  decision on defaults (question 1).

## Measured

In our ledger's core, in a VOS PVM guest under a JAM-compatible runtime (VOS's BLAKE2b from #3, the
static chain from #4, curve arithmetic in software, `opt-level = 3`), one hosted runner (AMD EPYC 7763),
v1 against v2 in the same job:

| | v1 | v2 (this PR) |
|---|---:|---:|
| Actor gas per reservation | 137.9 M | **72.0 M (−48 %)** |
| Reservations a second, batch 128, under the Agent's 8 G limits | 27.1 | **40.3 (+49 %)** |
| Witness per reservation, batch 8 | 8.2 KB | 10.4 KB |

Gas is exact and the same on every machine. Three more runs on other machines (EPYC 9V74, Xeon 8573C)
gave the same gas and v2 ahead by 30 % to 50 % under 8 G. The witness grows because the service tree's
multiproof carries its nodes opened and a lone-leaf entry carries its key; the throughput figure is net
of that.

## Compatibility

- **Nothing existing changes.** v1's roots, proofs, `composite_state_root`, the wire formats and the
  public API are untouched; this PR only adds.
- **A v2 root is a different value** from the v1 root of the same state, by design, and carries a version
  byte. Anyone who anchors roots and adopts v2 must say which version a root is, or cut over once.

## Tests

Run on 2026-10-13 (stable 1.98.0; the repository pins no toolchain), against #1's branch as the base:

| Command | #1 | This PR |
|---|---:|---:|
| `cargo test --workspace --all-features` | 362 passed | 372 passed |
| `cargo test --lib --tests` (default features) | 35 passed | 45 passed |
| `cargo clippy --all-features --all-targets` | 31 distinct warnings | 33 |
| `cargo fmt --check` | clean | clean |

The ten new tests need no feature. The two new clippy warnings are lints the crate already has elsewhere:
`sort_unstable_by_key` in `composite_state_root_v2` and `field_reassign_with_default` in a test.

## Relation to the other pull requests

Stacked on #1 only. Independent of #2, #3, #4 and #5 in code: it uses `crypto::blake2b_256`, so with #3
and `vos-blake2b` it hashes through VOS's BLAKE2b like the rest of the crate.

## Questions for you

1. **Defaults.** Should v2 become what `MemLedger::root` and `SuccinctTransitionWitness` produce, with v1
   behind a feature, in a follow-up? Or should v2 stay a parallel API?
2. **Placement.** A separate `merkle_v2` module, or v2 inside `merkle` and `state_root` behind a version
   parameter?
3. **Opaque entries.** `Entry::Many` (a node hash taken on trust) is sound only for trees that never
   remove a leaf; trees with removals use opened entries. Is that split acceptable, or would you rather
   have opened entries only, at the witness's cost?

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent and decides each
balance-affecting command inside the guest, within a completion budget. After #3 and #4 the remaining
hashing is mostly full-depth paths, and this tree halves a reservation's gas. Our ledger already uses v2
roots by default, with v1 as a supported opt-in; we would like CipherClerk to own the construction rather
than keep it in our tree.
