<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# CipherClerk 0004 — the empty-subtree chain as a static table

**Draft pull request description, 2026-10-03.** Not yet opened upstream. Patch `0004-static-empty-chain.patch`, against `e9a7b08` with 0001 applied;
`tools/cipherclerk-patched.sh` applies it for aledgent's guest. The series: `0003-0006-guest-performance.md`.

**Why.** In a VOS PVM guest, a reservation in aledgent's core costs about 295 M gas of actor work. A
per-PC profile of one 16-reservation invocation (`notes/MEASUREMENTS-F031.md` §8.3) puts about 46 % of
it in CipherClerk's `crypto::hash::blake2b_256`. That is **8 470 one-block hashes per reservation**,
at about 16 k gas each: the `blake2` crate's compression plus the `digest` wrapper's per-hash buffer
copies (`memcpy` and `memset` are 26 % of the actor's gas). Another 17 % is curve arithmetic.
`merkle::build_empty_chain` alone is 21 % of the hashes: 14 rebuilds a reservation, 128 hashes each.

| Patch | Subject | Actor gas per reservation (core, VOS-native) |
|---|---|---:|
| | before | 293.5 M |
| 0004 | `perf(merkle): the empty-subtree chain as a static table` | 256.5 M (−12.6 %) |
| 0003 | `perf(crypto): hash with VOS's BLAKE2b behind vos-blake2b` | 227.5 M (−22.5 %) |
| 0003 + 0004 | | 204.4 M (−30.4 %) |
| 0005 | `build(precompile): the current VOS precompile shim for pvm-precompile`, with VOS's native curve precompiles (VOS 0010) | 154.0 M |
| | and the guest at `opt-level = 3` (aledgent) | 104.8 M (−64 %) |

## The change

`build_empty_chain()` recomputed the 129 empty-subtree hashes on every `SparseMerkleTree::root`,
`prove`, `BatchProof::build` and `root`, and in `state_root`. It also returned the 4 KiB table by
value. The chain is a constant of the tree's definition, so it is now `SMT_EMPTY_CHAIN`, a static
table, borrowed by every caller. `build_empty_chain()` keeps its signature and returns a copy.
`compute_empty_chain()` is the recurrence, and a test holds the table to it. **Roots and proofs do
not change.**
