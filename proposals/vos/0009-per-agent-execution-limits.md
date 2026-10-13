<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0009 — Per-Agent execution limits

**Draft pull request description, 2026-10-03.** Not yet opened upstream.

> **JAM status (2026-10-04): JAM-compatible, in the default.** The limits are Agent state and
> runtime bounds; no host call is added.

| | |
|---|---|
| Branch | `perf/agent-stack` on aledgent's VOS fork, commits `6c794311` (`feat(agent): per-Agent execution limits`) and `b4dddcc0` (the Shared journal and Local SDK host arms) |
| Files | `vos-agent-sdk/src/{contract,runtime,authority,wire}.rs`; `vos/src/agent/{execution,standard,wire,driver,replay,local_journal_driver,shared_journal_driver,local_sdk_host,journal,host,clean_bootstrap,supervisor_adapters,external_local_executor,actor_storage}.rs`; `vos/src/actors/run.rs` |
| Depends on | 0006 (the control image's trailing sections) |
| aledgent task | `F-025`, `F-031` lever 1 (batching) |

**Read this first.** One actor invocation is bounded by constants: 1 000 M of actor gas, 16 FETCH
reads besides one per caller blob, and 48 KiB a state lane. A batching Agent needs more than all
three: aledgent's core runs out of gas at three reservations, and at 128 it needs a larger ring than
48 KiB holds. This makes the three bounds Agent state, settable by the Agent's authority. The
defaults are exactly today's, under hard ceilings that the runtime's ProgramId covers.

Everything below the line is the pull request text.

---

## Design

- **`ExecutionLimits { max_invocation_gas, max_invocation_reads, max_lane_state_bytes }`**
  (`vos-agent-sdk/src/contract.rs`). `STANDARD` = 1 000 M, 16, 48 KiB: the old constants.
  `MAXIMUM` = 10^12, 65 536, 4 MiB: the runtime's ceilings. Gas is bounded besides by the host's own
  outer budget, reads by the native work they cause, and lanes by the 4 MiB state image.
- **Setting them**: `ManagementRequest::SetExecutionLimits(limits)` (wire tag 12), authorized by a
  direct receipt of the new `AuthorityOperationKind::SetExecutionLimits` (16, in the management
  decision journal), and answered `ManagementReply::ExecutionLimitsSet` (tag 12). It is refused while
  any actor's current lane exceeds the requested lane bound. It has no ACC3 authorization plan yet,
  so the system-authority actor cannot issue it.
- **Keeping them**: an optional trailing section of the control image (`SXL1`) after 0006's `SPB1`.
  It is absent at the standard bounds, so images that never set them encode as before. A restored
  image carrying the standard bounds explicitly is refused as non-canonical.
- **Applying them**: the runtime derives `ExecutionBounds` (gas, FETCH calls and bytes, lane) from
  the Agent's limits and runs every actor invocation under them (`run_inner_actor_with_bounds`). Lane
  checks in preparation, commit, yield and restore use the Agent's bound. Host admission bounds work
  by the gas ceiling (`MAX_INVOCATION_GAS_CEILING`); the runtime refuses an invocation above the
  Agent's own bound. The actor-side loader accepts lanes up to the ceiling. Inline row storage keeps
  the standard 48 KiB.

## Guarantees

- **Deterministic**: the limits are Agent state, so every replica and the prover apply the same
  bounds to the same invocation.
- **Safe defaults**: an Agent that never sets them runs exactly as before, byte for byte.
- **Owner-controlled**: only the Agent's authority can set them, within ceilings fixed by the
  runtime's ProgramId.

## aledgent's values

The core host sets 8 000 M gas, 1 024 reads and 1 MiB a lane right after creating the Agent:

- **Gas**: at 8 G a batch is bound by its caller blobs, about twenty reservations, rather than by gas
  (`notes/MEASUREMENTS-F031.md` §1).
- **Reads**: enough for a committed map's few reads a leaf over a batch of 32.
- **Lanes**: room for a 256-entry ring at batches of 128.

## Before and after

Hosted runs (EPYC 7763, 2 vCPU, recompiler), aledgent's core on the JAM-native runtime, inputs/s:

| Bounds | batch 8 | batch 32 | batch 128 |
|---|---:|---:|---:|
| Standard (1 000 M: three reservations an invocation) | 11.63 | 11.88 | 11.86 |
| Agent limits, 8 000 M | 14.58 | 15.40 | 15.44 |

## For the maintainer to decide

1. The authorization surface: a new operation kind (as here), or the runtime's `UpgradeRuntime`.
2. Whether `RuntimePackageContract` should carry ceilings below `MAXIMUM`, so that a runtime
   publisher can narrow what an owner may select.
