<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# CipherClerk 0003–0006 — the kernel's cost in a VOS guest

**Draft pull request descriptions, 2026-10-03.** Not yet opened upstream. Patches 0003–0005 are against `e9a7b08` with 0001 applied (0002 is
independent). `tools/cipherclerk-patched.sh` applies 0001 and 0003–0005 for aledgent's guest. 0006 is a
design with an estimate, not a patch.

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

Measured one patch at a time on one hosted runner (`notes/MEASUREMENTS-F031.md` §8.4), at batch 128 under
the Agent's 8 G limits, the core goes from 15.5 to 33.4 reservations a second: 17.8 with 0004 alone,
23.9 with 0003 alone, 26.5 with both, 30.7 with 0005 and the native curve, 33.4 at `opt-level = 3`.

## 0003 — VOS's BLAKE2b behind `vos-blake2b`

A new feature routes `blake2b_256` and `blake2b_512` through `vos_protocol::blake2b::digest`. On a PVM
guest that is a compression scheduled for the PVM's 13 registers and 25-cycle memory accesses (VOS
0007), about 4 800 gas a block, streaming whole blocks without copies; on a host that serves VOS's
precompile 100 it is the precompile. Off the guest it is `blake2b_simd`. **The digests do not
change**: `tests/vos_blake2b.rs` holds both functions to the `blake2` crate at every block boundary,
with and without the feature. The feature adds an optional dependency on `vos-protocol` (no_std, no
other dependency than `blake2b_simd` and `rkyv`).

## 0004 — the empty-subtree chain as a static table

`build_empty_chain()` recomputed the 129 empty-subtree hashes on every `SparseMerkleTree::root`,
`prove`, `BatchProof::build` and `root`, and in `state_root`. It also returned the 4 KiB table by
value. The chain is a constant of the tree's definition, so it is now `SMT_EMPTY_CHAIN`, a static
table, borrowed by every caller. `build_empty_chain()` keeps its signature and returns a copy.
`compute_empty_chain()` is the recurrence, and a test holds the table to it. **Roots and proofs do
not change.**

## 0005 — the current precompile shim

`pvm-precompile` pinned `zkpvm-precompiles` at VOS `9f5337f`, whose shim emits a bare `ecall`. Today's
PVM compiler translates that into a trap, so a guest built with the feature panicked at its first curve
operation (VOS 0002 found this). The feature now uses `vos-pvm-precompiles`, the current shim with the
same API, which marks its ECALLs. Served natively by the runtime (VOS 0010), a Ristretto scalar mult
costs 250 k gas instead of about 3.9 M. `vos-protocol` and the shim are pinned at the same VOS
revision as the consumer's (aledgent pins `perf/agent-stack`).

## 0006 — a compact sparse Merkle tree (design and estimate)

Even with 0004, every path a proof recomputes is the full 128 levels: a touched key costs 128 node
hashes, almost all of them over one non-empty child and an empty sibling. A compact SMT stores a
subtree that holds one leaf as a shortcut leaf, `H(key ‖ leaf)` at the height where it becomes alone.
It hashes only branching nodes. Path length is then about log₂ N + 1 instead of 128. Counted on random
128-bit keys:

| Leaves N | Keys touched | Full-depth path hashes | Compact | Fewer |
|---:|---:|---:|---:|---:|
| 1 000 | 8 | 999 | 65 | 15× |
| 100 000 | 8 | 1 003 | 127 | 7.9× |
| 1 000 000 | 8 | 1 002 | 144 | 7.0× |

So it is an order of magnitude for books of thousands of accounts, and about **7× at a million**.
After 0003–0005 the core's remaining hashing is about 6 700 hashes, roughly 33 M gas of a 105 M
reservation. A compact tree would leave about 5 M, so the reservation would fall to about 77 M
(−27 %).

**It cannot keep the same roots.** A root over the full-depth tree commits to the 128-level chain; no
shortcut reproduces it without computing that chain. The root format would be versioned: a new
domain for nodes and shortcut leaves, and a version byte in `state_root`. aledgent's anchors carry
the root, so a migration would recompute the v2 roots from the leaf store (the leaves do not change)
and anchor both once, at a cut-over. That is a CipherClerk design decision, and an aledgent plan
change (anchoring and disclosure formats). It is not prototyped beyond the count above.
