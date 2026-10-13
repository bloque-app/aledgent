<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-019 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-019-01 | Pallet skeleton in frame-contrib: config, types, storage, `register_instance`, `set_keys`, mock runtime | `DEC-9`, `REQ-SL-10`, `REQ-SL-5` | An instance registers with a deposit. Its position account exists without a native balance. Keys rotate under the admin only |
| T-019-02 | `anchor` call with strict ordering and safe replay | `REQ-SL-1`, `REQ-PR-1`, `REQ-PR-2`, `INV-19`, `AC-J1.2` | Out-of-order and conflicting anchors are refused. An identical replay is a no-op. Each anchor writes one inbox entry |
| T-019-03 | `deposit` call with an opaque account reference and an inbox entry | `REQ-SL-2`, `REQ-SL-7`, `REQ-SL-9`, `AC-D1.4` | Every deposit moves value to the position and writes exactly one inbox entry. No storage is keyed by the account reference |
| T-019-04 | `withdraw` call: events keyed by operation reference, refusals recorded, withdrawal bound | `REQ-SL-8`, `REQ-SO-3`, `REQ-SA-4`, `NFR-8`, `AC-E1.4` | A failed transfer writes `WithdrawRefused` with the chain's error. A withdrawal over the bound is refused. A success writes `Withdrawn` |
| T-019-05 | Property tests and the storage-key audit | `REQ-SL-5`, `REQ-SL-9`, `REQ-SL-10`, `INV-17` | Sequences are gapless, anchors are ordered, and no storage key derives from an account reference, an operation reference or holder data |
| T-019-06 | Benchmarks and generated weights, with a regression check in CI | `DEC-9`, `REQ-SL-5` | Every call has a worst-case benchmark. Weights are generated and checked in CI |
| T-019-07 | Kreivo runtime branch, zombienet setup, `try-runtime` check | `DEC-9`, `REQ-SL-3`, `RISK-4` | A local relay and Kreivo with the pallet produce finalized blocks. `try-runtime` passes against a live-state snapshot |
| T-019-08 | End-to-end run with the adapter and the core on zombienet | `AC-D1.1`–`AC-D1.4`, `AC-E1.1`–`AC-E1.5` | The exit criterion in `plan.md` passes. Any gap in reproducing `AC-D1.3` is recorded with its fallback |
| T-019-09 | Runtime upgrade proposal for Kreivo, drafted | `DEC-9`, `RISK-4` | The draft names the pallet index, config, weights, governance track and version target. The owner has reviewed it. Nothing is sent upstream without the owner |
