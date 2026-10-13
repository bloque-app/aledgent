<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-012 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-012-01 | Build `register_credential` and `remove_credential` over `F-003`'s set, with the scope-subset rule and the `HolderRecovery` kind | `REQ-AU-20`, `REQ-AU-4`, `REQ-AU-18`, `INV-22`, `AC-A5.4` | `AC-A5.4` passes. The `INV-22` property test finds no widening |
| T-012-02 | Build owner authority through set membership and scope. Retirement takes effect at commit | `REQ-AU-20`, `REQ-AU-3`, `ERR-CredentialInvalid`, `AC-A5.1`, `AC-A5.2` | A retired credential is refused on the next command. `AC-A5.1` and `AC-A5.2` pass |
| T-012-03 | Build `delegate_custody`: holder-only initiation, custody flag, holder attribution | `REQ-AU-11`, `REQ-AU-12`, `AC-A7.1`, `AC-A7.2` | `AC-A7.1` and `AC-A7.2` pass |
| T-012-04 | Build `reclaim_custody`: signed challenge, authority snapshot on each operation, encumbrance guard, grants untouched | `REQ-AU-13`, `REQ-AU-14`, `AC-A7.3`, `AC-A7.4`, `AC-A7.5` | `AC-A7.3`–`AC-A7.5` pass, including an in-flight capture after the switch |
| T-012-05 | Build `recovery_mechanisms(account)` and the acknowledgement digest on every path that makes an account owned | `REQ-AU-9`, `AC-A5.3` | `AC-A5.3` passes on `open_account` and on `reclaim_custody` |
| T-012-06 | Build the `PendingChange` state machine as a constant table, checked by the `F-007` harness. Build `initiate_recovery`, `cancel_recovery`, `complete_recovery` with injected time and recovery-authority approval | `REQ-AU-15`, `REQ-AU-16`, `AC-A8.1`–`AC-A8.4`, `ERR-RecoveryNotElapsed` | `AC-A8.1`–`AC-A8.4` pass. The enumeration finds no pair outside the table |
| T-012-07 | Build `name_successor` and the two-phase `claim_succession` | `REQ-AU-17`, `AC-A8.5` | `AC-A8.5` passes, including cancellation during the delay |
| T-012-08 | Build the holder-held recovery credential path and the start-up checks: recovery key present and distinct from operator keys, no guardian field | `REQ-AU-18`, `REQ-AU-15`, `REQ-AU-19`, `AC-A8.6` | `AC-A8.6` passes. A bad configuration fails at start-up |
| T-012-09 | Add visibility and audit: notifications to authority holders, account view fields in `F-011`, audit records naming every approver. Register all tests in the `F-015` suite | `INV-20`, `REQ-AT-1`, `REQ-AU-7`, `NFR-14` | A pending recovery shows on the holder's account view. Every `AC-A5.*`, `AC-A7.*` and `AC-A8.*` test is in the suite |
| T-012-10 | Item 10 of amendment 0006, ruled: the three rules become `recovery_rules`, with their tests named for the requirements they now serve | `REQ-AU-16`, `REQ-AU-17`, `REQ-AU-20`, `AC-A8.2` | The rules and their tests are unchanged in behaviour and named for `REQ-AU-20` and `AC-A8.2` |
