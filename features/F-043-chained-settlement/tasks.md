<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-043 — tasks

**Proposed** (amendment 0040; the owner ruled the design on 2026-10-26). Traces: `REQ-IN-5`, `REQ-SL-9`,
`REQ-SL-10`, `REQ-SA-7`, `REQ-AT-1`, `AC-D2.1` (as amendment 0040 restates them), `REQ-SL-5`, `REQ-PR-9`, `NFR-6`, `NFR-22`.

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-043-01 | **The design.** Amendment 0040, this plan, these tasks and the `PLAN.md` row. Docs only | `REQ-IN-5`, `REQ-SL-9` | The files are in `amendments/` and `features/F-043-chained-settlement/`, the row is in `PLAN.md`, and `cargo xtask spec-ids --check` passes |
| T-043-02 | **Contract v3.** The head cell (`0x0000_000C`) and its read message; the fold in `append`; `(0, zeros)` on reset; `from` on `Deposited` and `call` on `Refused`; `Head.next_seq` frozen as the end of v2's stored range; the superseded-anchor removals dropped; `prune_inbox` below the frozen end only, bind entries and the latest anchor's position included (amendment 0040's addition); `check_invariants` | `REQ-SL-9`, `REQ-SL-10`, `REQ-SL-5` | The contract unit tests of the plan pass. For every entry kind, the entry rebuilt from its event and block hashes to the same `e_n`. No stored entry is written. The v2 layout is byte-identical after the first v3 message. The build's code hash is recorded |
| T-043-03 | **The upgrade, rehearsed.** zombienet: v2 to v3 with entries before and after, a failing frame, a reset. Chopsticks: `publish_upgrade`, `upgrade` and `set_anchor_head` dry-run on forks of the test and prod instances | `REQ-SL-9` | The fold of the events equals the head at every step. A failed frame's events are gone with it. Each dry run passes as the required origin and fails as any other |
| T-043-04 | **Settle verifies at sync.** Head read and events read (rpc, light client, `fake`); an optional history endpoint for catch-up; the accepted head and block in `settle_cursors`; `observe` folds only new entries and accepts a head after the core took in every entry; a mismatch is re-read once, then alerts and stops; every reader of `next_seq` (cursor, provisional range, positions cut, head report, `ops` reconcile and signals) takes the chain position; the operator's gap through the adapter's source, after which the adapter accepts the layer's head; the adapter's own `Pruned` gap removed; first start just after the reset, or at instantiation if never reset | `REQ-IN-5`, `REQ-SA-7`, `REQ-IN-3`, `REQ-IN-4` | An idle sync is one read (counted in a test). Hidden, extra and altered entries each stop the adapter at the first new position with an alert. Behind more than 256 blocks: unreadable without a history endpoint, caught up with one |
| T-043-05 | **Settle's re-scan removed.** `Adapter::rescan`, its timer, `rescan_every` and `rescan_window` (ignored with a warning for one release, then refused), `settle.json`; `ops`' restore step | `REQ-IN-5` | No code path re-reads a range on a timer. The restore resumes from the accepted head (`ops/tests/restore.rs`) |
| T-043-06 | **The core's re-scan path removed.** `Ledger::rescan`, the ingress route, `Incident::Missing` and `Incident::Unknown`, the indexes `RESCAN_RECORDED` and `INGESTED_AT`. `Call::Rescan` stays decodable and is refused | `REQ-IN-5`, `NFR-22` | An old input log with re-scans replays under its own code. The new code refuses `Rescan` |
| T-043-07 | **Incidents and gaps to the audit trail.** A new audit class; each settlement incident and each gap is an audit record in the batch that would have written its leaf; the `gaps` and `incidents` leaves go; the gap end (one `u64`) replaces `gap_within` in `proof`; export records updated | `REQ-IN-5`, `NFR-6`, `REQ-AT-1`, `REQ-SA-7` | Recording an incident or a gap writes no leaf that grows. `proof` gives the same answers as with the list over the existing tests. The heap test stays flat over 10,000 incidents |
| T-043-08 | **Running books upgrade in place.** The stateful migration for `T-037-19`: existing incidents and gaps written as audit records, the gap end set, the leaves dropped | `NFR-22`, `REQ-RT-17` | Depends on `T-037-19` and its replay gate. A book with incidents and gaps in the old layout upgrades in place with each one on the audit trail and the same `proof` answers |
| T-043-09 | **Readers and runbooks.** `crates/verify` reads a same-block superseded anchor from its `Anchored` event, and assigns a carried anchor to its sequence by block, not by position (`rpc.rs:540`); the alert becomes `AledgentSettlementChainMismatch`; every runbook naming the re-scan, and `F-018`'s restore step, rewritten | `REQ-PR-4`, `NFR-9` | `ops/tests/runbooks.rs` and `ops/tests/alerts.rs` pass. The verifier's reset tests pass on v3 |
| T-043-10 | **Rollout on test.** v3 by the licence owner, then the head reset for the current book, then settle and the core with `F-043` | `REQ-IN-5` | Two idle syncs of one read each, and one sync with new entries folded, on test |
| T-043-11 | **Rollout on prod.** v3 by the licence-owning community's referendum, then the reset as governance, then settle and the core | `REQ-IN-5` | As `T-043-10`, on prod |
| T-043-12 | **Contract v4.** `prune_inbox` and the storage v3 no longer uses (`inbox`, the legacy `anchors`, `anchor_migration`) removed | `REQ-SL-10` | Every instance has pruned its v2 entries before v4 ships. The upgrade is rehearsed as in `T-043-03` |

## Order

`T-043-01`. Then `T-043-02` and `T-043-06` side by side. `T-043-03` and `T-043-04` follow `T-043-02`, and
`T-043-05` goes with `T-043-04`. `T-043-07` follows `T-043-06`. `T-043-08` follows `T-043-07` and `T-037-19`.
`T-043-09` runs beside them. Then `T-043-10`, `T-043-11`, and later `T-043-12`.
