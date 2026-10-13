<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-026 — Settlement contract (ink!) for V0

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Home | `contracts/settlement` (ink!, its own workspace, excluded from the root like `pallets/`) |
| Depends on | `F-019` (the rules it ports), `DEC-17` |
| Unblocks | `F-027`, the end-to-end run |

## Goal
The on-chain side of settlement in V0, as a `pallet-contracts` (ink!) contract on Kreivo. It keeps the
pallet's rules: one position per deployment, deposits with account references, calls with operation
references and one recorded outcome each, anchors, and an ordered inbox. No runtime upgrade.

## Scope
- **Messages:** register the deployment and its keys; deposit (the contract pulls from the
  depositor through Kreivo's assets API, with no approval: owner ruling of 2026-09-24, `T-026-05`);
  a batch of calls, each with our reference and its own applied or refused outcome; anchor; set a
  withdrawal bound per asset; prune the inbox.
- **Value moves through Kreivo's contract API** (its chain extension) wherever it can; `call_runtime`
  only where that API has no equivalent (`T-026-05`).
- **Upgrade authority:** the licence-owning community, governed by `pallet-communities`' own
  membership governance before any real value; documented, with a deployment checklist
  (`T-026-07`).
- **Storage:** positions, anchors by sequence, the inbox by sequence, bounds. Nothing per holder.
- **Deployment:** uploaded by a community, instantiated through Kreivo's contracts store.

## Open points
- Whether the light client reads contract events or contract storage more reliably at finality. Test on
  zombienet first.
- The contract's review before it holds real value.
