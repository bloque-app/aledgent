<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0010 — The curve precompiles served natively, and room for a fast actor build

**Draft pull request description, 2026-10-03.** Not yet opened upstream.

> **JAM status (2026-10-04): parked, outside JAM.** Calls 110–114 served by Refine are VOS's own,
> not in the Gray Paper's Refine set (GP 0.8.0 eq. B.6, p. 55); `native-curve` stays on the fork,
> off by default. Two parts of this branch remain usable within JAM: the 4 608-KiB program limit
> (a runtime constant), and PR #9's in-guest service of an actor's 110–114 (the runtime answers
> them as guest code, so the outer runtime still issues only calls 9–14), although at the
> interpreted price software curve arithmetic is cheaper.

| | |
|---|---|
| Branch | `perf/agent-stack` on aledgent's VOS fork, commits `04ccf654` (`feat(agent): serve actors' curve precompiles natively`) and `b77fe2e2` (immediate ids; 4 608-KiB programs) |
| Files | `pvm/runtime/{Cargo.toml,src/inner.rs,src/refine_host.rs,src/spi.rs}`, `vos/src/agent/execution.rs`, `vos/Cargo.toml`, `services/agent-runtime/Cargo.toml`, `vos-agent-sdk/src/runtime.rs` |
| Builds on | 0002 (PR #9: the runtime serves 110–114 to actors as guest code) and 0008 |
| aledgent task | `F-025`, `T-025-02` (issue #66) |

**Read this first.** PR #9 made an actor's curve precompiles (ECALLs 110–114) work on the Agent path.
The runtime computes them as its own guest code, at the interpreted price: 3.9 M gas a Ristretto
scalar mult. That gives parity, not speed. Here Refine serves them natively to the outer program,
and the runtime (feature `native-curve`) forwards an actor's call outward and charges the native
price. A scalar mult then costs 250 k. On aledgent's core, actor gas per reservation falls by a
quarter, 204 M → 154 M. A second commit raises the program limit to 4 608 KiB. An actor built for
speed (`opt-level = 3`) fits under it, and that takes off another third, 154 M → 105 M.

Everything below the line is the pull request text.

---

## Design

- **Refine** (`pvm/runtime`, std hosts): calls 110–114 are memory transforms with the shims'
  semantics (`vos_pvm_precompiles`). 110 is φ9 ← φ7·φ8 (scalar, point); 111 is φ9 ← φ7 + φ8
  (points); 112 is φ8 ← wide(φ7); 113 and 114 multiply and add scalars mod l. A non-canonical scalar
  or an invalid point gives the all-zero sentinel. The prices are `gas::CURVE` = 250 000, 40 000,
  5 000, 5 000 and 5 000, about the native time of each (45 µs, 6.5 µs and under 1 µs on an M2 Pro)
  at the recompiler's 4–5 G gas a second, rounded up.
- **The runtime** (`runtime-native-curve`, the runtime's `native-curve` feature): the actor's 110–114
  arm copies the operands into runtime memory and issues the same call outward. It has one call site
  per id, because the PVM compiler needs an immediate id in t0. It copies the result back through the
  actor's output pointer and charges the actor the native price. Without the feature, PR #9's in-guest
  path is unchanged.
- **Admission**: 110–114 join `REFINE_PROFILE_HOST_CALLS`.
- **Program limit**: `MAX_RUNTIME_PROGRAM_BYTES` 2 560 → 4 608 KiB. With the program hashed once
  (0006) or looked up (0008), the 32 MiB arena holds a 4.3 MB actor. This was measured with small
  state, **not yet at the 4 MiB state ceiling**.

## Guarantees

- **Deterministic**: each operation is a pure function of its operands, with a fixed price.
- **Proofs**: the operations leave the metered guest. A Refine proof constrains them as the kernel
  path already does, with `pvm/proof`'s `RistrettoChip` and scalar chips, one row per ECALL. As for
  0007's call 100, the bundle tracer must treat 110–114 as in-circuit rows, not host boundaries.

## Before and after

aledgent's core on the VOS-native runtime, actor gas per reservation, exact:

| Core build | Curve | Actor gas per reservation |
|---|---|---:|
| CipherClerk with VOS's BLAKE2b and the static empty chain, `opt-level = "z"` | software (dalek) | 204 M |
| + `pvm-precompile`, PR #9 in-guest | interpreted price | 241 M |
| + `pvm-precompile`, this change | native | 154 M |
| + `opt-level = 3` (4.33 MB program) | native | 105 M |

Inputs/s on one hosted runner per build: `notes/MEASUREMENTS-F031.md` §8.
