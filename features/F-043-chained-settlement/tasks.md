<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-043 — tasks

**Proposed** (amendment 0040, draft). Nothing below `T-043-01` starts until the owner rules the amendment's
open questions. Traces: `REQ-IN-5`, `REQ-SL-9`, `REQ-SA-7`, `AC-D2.1`, `AC-D2.2` (as amendment 0040 restates
them), `REQ-SL-5`, `REQ-SL-10`, `REQ-AT-3`, `REQ-PR-9`, `NFR-22`.

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-043-01 | **The design.** Amendment 0040, this plan, these tasks and the `PLAN.md` row. Docs only | `REQ-IN-5`, `REQ-SL-9` | The files are in `amendments/` and `features/F-043-chained-settlement/`, the row is in `PLAN.md`, `cargo xtask spec-ids --check` passes, and no other file changed |
| T-043-02 | **Contract v3.** `entry_head` (`0x0000_000C`), the chain digest in `append`, `Entry` and `EntryChainStarted` events, lazy genesis, a new genesis in `set_anchor_head`, `entry_head()`, `prune_inbox` below `from_seq` only, `check_invariants` | `REQ-SL-9`, `REQ-SL-10`, `REQ-SL-5` | The unit tests of the plan's test plan pass. No stored entry at or above `from_seq`. The v2 layout is byte-identical after the first v3 message. The build's code hash is recorded |
| T-043-03 | **The upgrade, rehearsed.** zombienet: v2 to v3 with entries before and after, a failing frame, a reset. Chopsticks: the exact `publish_upgrade` and `upgrade` calls dry-run on forks of the test and prod instances, each with the origin it requires | `REQ-SL-9` | The recomputed digest equals `entry_head` at every step on both. Every dry run passes as the required origin and fails as any other |
| T-043-04 | **Settle reads the head and verifies new entries only.** `Chain::entry_head` and `Chain::entries` (rpc, light client, fake); `history_endpoint`; the accepted head in `settle_cursors`; `observe` and the provisional check on the verified range; the first start at `from_seq` (`T-036-15`) | `REQ-IN-5`, `REQ-SA-7`, `REQ-IN-3`, `REQ-IN-4` | Idle sync is one read (counted in a test). A burst is verified once. Hidden, extra and altered entries each stop the adapter at `n` with an alert. Behind more than 256 blocks with no history endpoint means unreadable; with one, it catches up |
| T-043-05 | **Settle's re-scan removed.** `Adapter::rescan`, the timer, `rescan_every` and `rescan_window` (ignored with a warning for one release, then refused), `settle.json` | `REQ-IN-5` | No code path re-reads a range on a timer. The settle tests that covered re-scans cover the chain instead |
| T-043-06 | **The core's chain record.** `SettlementChain` per instance, `HeadBody.digest`, the bounded recomputation over newly final positions, the book check of the first head, refusal below `from_seq`, the break, and clearing it by re-read or by operator intervention, with an audit record | `REQ-IN-5`, `AC-D2.1`, `REQ-AT-3` | The `ledger-core` cases of the test plan pass. The heap test keeps the arena flat over 10,000 entries |
| T-043-07 | **The re-scan path and the completeness incidents removed from the core.** `Ledger::rescan`, the ingress route, and new writes of `Missing`, `Unknown` and `Gap`. `Call::Rescan` stays decodable and is refused | `REQ-IN-5`, `NFR-22` | An old input log with re-scans replays under its own code. The new code refuses `Rescan` |
| T-043-08 | **The remaining incidents bounded** (as Q2 rules; with `T-032-26`). Keyed leaves for the other incident kinds, and for gaps as Q12 rules. The signals show the open break and the incident counts (#74 item 3) | `REQ-IN-5`, `NFR-9` | Writing an incident touches one leaf. No leaf grows with the count of incidents |
| T-043-09 | **The core change for a running book** (as Q11 rules). A stateful migration through `F-037` (`T-037-19`), or the re-founding procedure. The export format's change | `NFR-22`, `REQ-RT-17` | A book with incidents in the old layout upgrades, or re-founds, with every incident carried over and the same per-asset totals |
| T-043-10 | **Readers and runbooks.** `crates/verify` reads the superseded same-block anchor from `Entry` events. The runbooks and alerts that name the re-scan are rewritten (`AledgentSettlementChainBroken`). `F-018`'s restore step | `REQ-PR-4`, `NFR-9` | `ops/tests/runbooks.rs` passes. The verifier's reset tests pass on v3 |
| T-043-11 | **Rollout on test.** v3 by the licence owner's fast track (the admin's calls are each one signed call); settle and the core with `F-043` | `REQ-IN-5` | Two idle syncs of one read each, and one sync with new entries verified, on the re-founded test book |
| T-043-12 | **Rollout on prod**, after the Root referendum enacts: v3 by the community's referendum; the reset under v3 if not yet made (Q8); then settle and the core | `REQ-IN-5` | As `T-043-11`, on prod. Until then, prod's settle stays off or runs with `rescan_every: 0` |

## Order

`T-043-01`. Then `T-043-02` and `T-043-06` side by side; `T-043-03` after `T-043-02`; `T-043-04` after
`T-043-02`; `T-043-05` with `T-043-04`; `T-043-07` after `T-043-06`; `T-043-08` and `T-043-09` after
`T-043-07`; `T-043-10` beside them; `T-043-11`; `T-043-12`.
