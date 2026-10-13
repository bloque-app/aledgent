<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0008 — Two profiles of the standard runtime: JAM-native and VOS-native

**Draft pull request description, 2026-10-03.** Not yet opened upstream.

> **JAM status (2026-10-04): the JAM-native profile is JAM-compatible and is the default; the
> VOS-native lookup is parked until it keys and prices as the Gray Paper does.** It uses
> `historical_lookup` with the Gray Paper's number (7) and registers (φ7 service, φ8 h, φ9 o,
> φ10 f, φ11 z; GP 0.8.0 appendix B.6, p. 60), which the ruling allows. But it passes the
> ProgramId as h, a digest under the `vos/program/standard-pvm` domain, where Λ keys a preimage by
> its plain BLAKE2b-256, h = H(p) (eq. 9.5 and 9.7, p. 17): a JAM node would answer NONE. And the
> host charges 600 + 248 gas a KiB, the constants of `lookup` (Ω_L), where `historical_lookup`
> costs M_H,c + G(M_H,ℓ, z) = 1 125 + 264 a KiB (I.4.5, p. 78). Both can be
> fixed; until then `vos-native` stays on the fork, off by default.

| | |
|---|---|
| Branch | `perf/agent-stack` on aledgent's VOS fork, commit `a97f0921` (the profile features and `refine_host::serve_historical_lookup`) |
| Files | `services/agent-runtime/Cargo.toml`, `vos/Cargo.toml`, `vos/src/agent/standard.rs` (`lookup_program`), `vos/src/agent/package_admission.rs`, `pvm/runtime/src/{inner,refine_host,spi}.rs` |
| Depends on | 0006 and 0007 |
| aledgent task | `F-025` (issue #66); the owner's ruling of 2026-10-03 on the two profiles |

**Read this first.** One runtime source, two builds, with the same transition semantics. Each Agent
picks its build through the runtime package it pins.

- **JAM-native** (the default): the actor program travels in the work and the metered guest verifies
  it, once (0006), with the scheduled BLAKE2b (0007). The host is not trusted with the program.
- **VOS-native** (`vos-native`): the runtime obtains its actor's program by its ProgramId through the
  Gray Paper's Refine `historical_lookup` (call 7). The host serves it from preimages it verified
  natively. JAM guarantees `h = H(d)` for the preimages its state holds in the same way (GP 0.8.0
  §9.2.1, eq. 9.6). The guest never hashes the program. The work shrinks from 2.37 MB to 14 KB for
  aledgent's core, and the runtime's gas per Invoke falls from 175 M to 90 M.

Everything below the line is the pull request text.

---

## Why two profiles

An Agent hosts actor programs that JAM's preimage store does not hold, so a runtime that must run on
any Refine host verifies them itself. That is the JAM-native profile, and it stays the default. A VOS
node that keeps the Agent's catalog has the program bytes and their identity already: it verified
them when the package was admitted. For such a node, re-hashing megabytes in the metered guest on
every invocation is pure overhead. That is the VOS-native profile. Both run the same actor code
through the same runtime logic, and differ only in where the program comes from.

## Design

- **The lookup** (`pvm/runtime`): `refine_host::serve_historical_lookup(machine, preimage)` implements
  Ω_H for an embedder: φ7 = service (`2^64 − 1`, this one), φ8 = h, φ9 = o, φ10 = f, φ11 = z. It
  charges 600 + 248 gas per KiB of z, writes `v[f..f + l]` and returns `|v|`, or NONE. The embedder
  passes a source of natively verified preimages. aledgent's host keeps each attached actor's
  program by ProgramId, checked with `ProgramId::of_pvm` when it is attached.
- **The runtime** (`vos/runtime-program-lookup`, the runtime's `vos-native` feature):
  `resolve_clean_invocation` skips the availability scan for the program and calls `lookup_program`.
  That calls `historical_lookup` with the work's ProgramId, first for the length and then for the
  bytes. Nothing else in resolution or execution changes. Schema, policies, installation data and
  caller blobs still travel and are verified as before.
- **Admission**: runtime packages may use calls 7 and 100 besides 9–14
  (`REFINE_PROFILE_HOST_CALLS`). A host that does not serve 7 makes a VOS-native runtime stop at the
  call, deterministically.

## Guarantees

- **Same transitions, apart from the binding memo.** Outcomes, actor lanes, results and
  acknowledgements are those of the JAM-native build for the same requests. The JAM-native build also
  records 0006's program binding in its control image; the VOS-native build does not need it.
- **Determinism.** The lookup returns the same bytes on every replica for the same ProgramId, or
  NONE. Gas depends only on z. A replica that lacks the preimage stops at the call; it does not
  diverge.
- **Trust and proofs.** VOS-native moves the program's authentication to the host, as JAM's Refine
  does: Λ and `historical_lookup` are served by the invocation, not the guest. A transition proof of
  this profile has to constrain the lookup against an anchor of the preimages, as a JAM auditor
  re-executes Refine against the same lookup anchor. The public I/O (0006) binds the ProgramId and
  every other reference of the work, so a verifier holding the program can check it.

## Before and after

aledgent's core (2.36 MB), one reservation per Invoke. Gas is exact. Time is in one hosted job (AMD
EPYC 7763, 2 vCPU, recompiler, run 37145564561), with 8 G of actor gas where noted:

| Runtime | Work bytes | Runtime gas per Invoke | Inputs/s, batch 1 | batch 32, 8 G | batch 128, 8 G |
|---|---:|---:|---:|---:|---:|
| JAM-native | 2.37 MB | 178 M | 7.87 | 15.40 | 15.44 |
| VOS-native | 14 KB | 95 M | 10.09 | 15.70 | 14.45 |

Program size barely matters to VOS-native: a 1.97 MB hot-path-only build of the core costs 82 M
where the full 2.36 MB build costs 91 M. Under JAM-native the same builds cost 152 M and 175 M.

## For the maintainer to decide

1. Whether the standard runtime ships both builds, and how a package names its profile. Today the
   runtime's ProgramId tells them apart; a capability flag in `RuntimePackageContract` would let
   admission check that the host serves call 7.
2. Whether `historical_lookup` should be keyed by the ProgramId (as here) or by the program's
   `BlobRef`, so that one preimage store serves both.
