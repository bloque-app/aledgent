<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-001 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-001-01 | Create the Rust workspace and `crates/ledger-kernel`. Pin `cipher-clerk` at `e9a7b08` with `default-features = false`, and `helpers` as a dev-dependency only. Add the CI check that fails on `fmd-experimental`, `stealth-experimental` or `notes` | `DEC-15`, `DEC-11`, `DEF-4`, `RISK-3` | `cargo build` is green. The CI check fails on a branch that enables a forbidden feature |
| T-001-02 | Kernel account and posting builders: `KernelAccountSpec`, derived kernel account ids, `PostingDraft::balanced` (at least two entries, balanced per asset, `NonZeroU64` values, entry currency taken from the account), and `PostingMeta` encoding into `tx_template_id`, `user_data` and `external_id` | `REQ-KER-1`, `REQ-KER-2`, `REQ-KER-7`, `REQ-PO-1`, `INV-1`, `INV-4`, `INV-7` | Unit tests pass. A compile-fail test shows that an unbalanced draft cannot be built |
| T-001-03 | `LeafStore` trait, `SubTree`, `KeySet`, `BookDelta`, and the per-sub-tree write rules. `MemStore` over six `SparseMerkleTree`s | `REQ-KER-5`, `INV-2`, `INV-25`, `DEC-1` | Each forbidden write in the plan's table is refused. `subroots` fold to the composite root |
| T-001-04 | `WorkingSet` overlay implementing `LedgerState`: key discovery (including referenced transfers' accounts), savepoints, and poison-on-miss | `DEC-4`, `REQ-KER-10` | A batch with a withheld key commits nothing and returns `UnloadedKey` |
| T-001-05 | `OpeningVault` with in-memory plaintext running sums; the byte-matching oracle; per-chain negativity openings; `KeyRing` with derived account and registrar keys; blinding and nonce derivation from private inputs | `REQ-KER-4`, `REQ-RT-6`, `REQ-RT-7`, `DEC-3`, `DEC-11` | A flagged account's overdraw is refused with `BalanceMustNotBeNegative`. The same unit with the same inputs gives identical bytes twice |
| T-001-06 | `Book::bootstrap` and `Book::apply`: steps of creations and chains, seed derivation per step, result classification, gapless `posting_seq`, and the delta log | `REQ-KER-6`, `REQ-KER-10`, `REQ-PO-2`, `REQ-RT-3` | Every row of the classification table has a test. A repeated `external_id` is refused. A failed chain drops its creations |
| T-001-07 | `FileStore` on redb (leaf, node and delta-log tables, fsync per commit). Commit ordering with the vault, and rollback of the vault on restart | `INV-2`, `REQ-KER-5`, `REQ-KER-6`, `DEC-4` | Crash-injection tests at each point between the two commits recover to the last committed `seq` with matching root and vault |
| T-001-08 | Differential property test against `MemLedger` and `composite_state_root`, the chain-by-chain equivalence test, and committed golden vectors | `REQ-KER-1`, `REQ-KER-10`, `REQ-RT-3` | 10 000 cases pass on both stores. Golden roots reproduce |
| T-001-09 | The `conformance-ports` feature (`KernelPort`, `StorePort`), the CI check that production crates never enable it, and the kernel and store cases of `F-015` made green | `INV-1`, `INV-2`, `INV-3`, `INV-4`, `INV-7`, `INV-25`, `REQ-CS-5` | The `F-015` M-0 gate profile passes in CI |
| T-001-10 | Withdraw the `Adjustment` posting kind (amendment 0006 item 7): code 12 retired, never reused; the movement status `Adjusted` with it | §5.5, `REQ-PO-1`, `US-K4` | No posting kind or movement status names an adjustment. The codes of the others are unchanged |
