<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-025 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-025-01 | Compact acknowledged results and acknowledgements on the standard AgentRuntime | `NFR-1`, `REQ-RT-3` | An Agent takes 10 000 Linear invocations with acknowledgements. Replay is unchanged. The per-result gas term is gone. PR opened from the fork |
| T-025-02 | Dispatch the curve precompile ECALLs 110–114 on the Agent path | `NFR-1`, `REQ-RT-3` | The kernel probe with `pvm-precompile` runs in an Agent. The S2 and S3 guest terms re-measured. PR opened |
| T-025-03 | Cache the prepared runtime and actor across invocations | `NFR-1` | The S4 fixed term re-measured. Replay is unchanged. PR opened |
| T-025-04 | Complete touched-state transport on the state-block path, without the per-execution runtime reload | `NFR-1`, `DEC-4` | S4 at 1, 2 and 4 MiB is flat in state size. PR opened |
| T-025-05 | Take the Authority Agent's cost off the per-invocation canonical path | `NFR-1` | `vosx space invoke-local` completes 1 000 invocations within the ABI path's cost plus the journal write |
| T-025-06 | A design note for a deterministic compiled Agent path, for @olanod (VOS's maintainer) | `NFR-1`, `REQ-RT-3` | Note in `proposals/vos/`. The maintainer's answer recorded |
| T-025-07 | CipherClerk patches 0001 and 0002 as pull requests from a fork | `REQ-RT-7`, `INV-4` | PRs opened. `F-001`'s pin updated once merged |
| T-025-08 | Re-measure the reservation budget with every accepted change | `NFR-15` | `notes/MEASUREMENTS-F002.md` records the reservation decision's p50 and p99 with each change, as an observation within completion (amendment 0007: there is no 50 ms budget for it) |
| T-025-09 | Let an actor declare its program, heap and stack within the owner's ceilings (`proposals/vos/0003`, found by `F-005`) | `DEC-4`, `REQ-RT-3` | The core installs and runs on the branch without a hand-raised limit. An undeclared or over-ceiling actor is refused. Replay is unchanged. PR opened |
| T-025-10 | Durable timers for Agents: a native scheduling component callable like an extension, without breaking the ABI | `REQ-RT-5`, `DEC-6` | Proposal and branch on the fork; the external ticker (`DEC-6`) can retire |
| T-025-11 | Outbound calls between Agents, restored on `saga/agents` | `REQ-RT-4` | Proposal and branch on the fork |
| T-025-12 | Chain observation in the substrate extension: expose the light client's finalized-block and storage subscriptions | `REQ-IN-5`, `REQ-SA-7` | The extension reports finalized events in order, with gaps detected |
| T-025-13 | An outbox VOS drains into an extension after commit ("consensus-committed extension outbox") | `DEC-18`, `REQ-RT-4` | An Agent's committed call reaches the settlement extension exactly once; `F-027` runs on it |

