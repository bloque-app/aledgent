<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# CipherClerk 0006 — a compact sparse Merkle tree (design and estimate)

**Draft pull request description, 2026-10-03.** Not yet opened upstream. A design with an estimate, not a patch. The series: `0003-0006-guest-performance.md`.

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

## Prototyped (2026-10-04, experiment `D-COMPACT-SMT`)

Built in aledgent as `crates/compact-smt`, behind the feature `smt-v2` (the default since `DEC-25`,
2026-10-05; v1 is the feature `smt-v1`, a supported opt-in beside v2), not yet as a CipherClerk
patch: the root workspace pins CipherClerk unpatched, and leaves and leaf hashes do not change, so
the tree above them can live on either side. The design is the Diem/Aptos Jellyfish hashing over
a binary radix (a lone leaf `H(leaf-node ‖ key ‖ leaf hash)`, an empty subtree zero, otherwise
`H(node ‖ left ‖ right)`), a composite `H(state-root/v2 ‖ 0x02 ‖ six sub-roots)`, single-key
proofs and a witness multiproof. Measured (`notes/MEASUREMENTS-F031.md` §11) under the
JAM-compatible default with software curve: **137.9 M to 72.0 M of actor gas a reservation**, and
27.1 to 40.3 reservations a second at batch 128 under 8 G on an EPYC 7763. If adopted, the patch
moves the module into `cipher_clerk::merkle` and `state_root`, and gives `MemLedger::root` and
`SuccinctTransitionWitness` a v2.
