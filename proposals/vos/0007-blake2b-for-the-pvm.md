<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0007 — A BLAKE2b compression scheduled for the PVM, and the precompile option

**Draft pull request description, 2026-10-03.** Not yet opened upstream.

> **JAM status (2026-10-04): the compression is JAM-compatible and in the default; the
> precompile option is parked, outside JAM.** The scheduled compression is guest code with the
> same digests. The precompile (`blake2b-precompile`) is VOS's call 100, which is not in the Gray
> Paper's Refine set (GP 0.8.0 eq. B.6, p. 55) and which JIP-1 assigns to `log`. It stays on the
> fork, off by default.

| | |
|---|---|
| Branch | `perf/agent-stack` on aledgent's VOS fork: `a97f0921` (the compression, and precompile 100 served by Refine), `2ef31b17` (the compression as a naked function) |
| Files | `vos-protocol/src/blake2b.rs`, `vos-protocol/src/blake2b_rv64em.S`, `vos-protocol/tools/{gen,check}_blake2b_rv64em.py`, `vos-protocol/src/identity.rs`, `vos/src/crypto/blake2b.rs`, `pvm/runtime/src/{blake2b,inner,refine_host,spi}.rs` |
| Depends on | nothing; measured on top of 0006 |
| aledgent task | `F-025` (issue #66) |

**Read this first.** Every BLAKE2b a PVM guest computes in software pays the gas model's memory price:
a load or a store holds one of four ALU slots for 25 cycles. The portable compression in
`blake2b_simd`, built at the runtime's `opt-level = "s"`, spills its 32 working words and costs about
**9 700 gas a 128-byte block**. A compression scheduled for the 13-register PVM costs **4 800**:
half the gas, the same digests, no protocol change. It is the default for every PVM guest that
hashes through `vos-protocol`. A host precompile (VOS's ECALL 100, served by Refine natively) would
bring the block to about 1 000 gas. That moves the work out of the metered guest, so it is offered
as an option, not a default.

Everything below the line is the pull request text.

---

## Problem

After 0006, the standard runtime still hashes the actor program once per invocation (its `BlobRef`
check), plus every identity and state digest it computes. On aledgent's 2.36 MB core that one pass is
about 175 M gas, two thirds of what the runtime spends per Invoke. Actors that hash their own data
(Merkle paths, content addresses) pay the same price per block.

## Design

**The compression.** `vos-protocol/tools/gen_blake2b_rv64em.py` emits a straight-line compression:
twelve rounds in one basic block, so the gas model's out-of-order window sees all of it. The sixteen
working words share ten registers under an offline-optimal (Belady) replacement over the fixed G
order. The other six live in a stack frame, and each G loads its two message words. That is about
490 memory operations a block, where the compiled portable loop makes about 1 500.
`check_blake2b_rv64em.py` interprets the emitted RV64 against a reference compression for 64 random
states and both finalization flags. `vos_protocol::blake2b::digest` streams whole blocks in place
and buffers only the domain and the last partial block. On non-PVM targets it is `blake2b_simd`.
The compression is the body of a naked Rust function, so two copies of the crate in one LTO module
cannot define the same symbol.

**Precompile 100, as an option** (`outer-blake2b-precompile` in `vos-protocol`,
`runtime-blake2b-precompile` in `vos`, `blake2b-precompile` in the runtime). Refine serves VOS's
BLAKE2b compression call (φ7 = h, φ8 = m, φ9 = t, φ10 = f; a memory transform, registers
unchanged) to the outer program natively, at 1 000 gas a compression (`gas::BLAKE2B_COMPRESS`).
That price is the PEEK/POKE host-call base plus about four times the native compression's cycles.
`REFINE_PROFILE_HOST_CALLS` admits it in runtime packages.

## Guarantees

- **The default changes no definition.** The same digests, from the same guest, metered. Transitions
  and public I/O are byte-identical to the runtime with `blake2b_simd`; only gas changes.
- **The precompile option moves trust.** With it, the runtime's hashing (its `BlobRef` checks
  included) runs outside the metered guest. A Refine proof has to constrain it the way the curve
  precompiles are: `pvm/proof`'s `Blake2bChip` proves one compression per ECALL row. The Refine
  bundle tracer today accepts only calls 9–14 as host boundaries (`pvm/proof/src/refine.rs`,
  "observed non-standard Refine host boundary"). It would have to treat call 100 as an in-circuit
  chip row rather than a boundary: a boundary per block would also exceed the bundle's 1 024 slices.

## A bug found on the way

`pvm/proof/fixtures/hash-bench` issues `ecalli 100` for its BLAKE2b compression. In JAM's JIP-1,
host call 100 is `log`. A VOS guest run by a JAM host would have its compressions treated as log
lines. VOS's precompile numbering (100, 110–114) sits in the range JIP-1 uses; moving it to an id
JAM leaves unassigned would avoid the collision. **A question for @olanod (VOS's maintainer).**

## Before and after

aledgent's core, one reservation per Invoke, after 0006. Gas is exact. Time is in one hosted job (AMD
EPYC 7763, 2 vCPU, recompiler, run 37145564561):

| Runtime | Gas per block | Runtime gas per Invoke | Inputs/s, batch 1 | batch 32, 8 G |
|---|---:|---:|---:|---:|
| 0006 (`blake2b_simd`) | 9 722 | 274 M | 5.22 | 13.96 |
| + scheduled compression (default) | 4 794 | 178 M | 7.87 | 15.40 |
| + precompile 100 (option) | 1 000 + call | 113 M | 9.11 | 15.77 |

In aledgent's core the actor's own hashing is CipherClerk's (`blake2` crate). With CipherClerk on
this compression (aledgent `proposals/cipherclerk/0003`), the core's gas per reservation falls by
another 22 %.
