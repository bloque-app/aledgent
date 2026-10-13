<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-016 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-016-01 | Process skeleton: store schema, single-active lease, signer trait with a KMS implementation and a test implementation, core principal with three capabilities only | `REQ-AR-3`, `NFR-8`, `REQ-SA-8`, `REQ-SA-5`, `DEC-8` | A second instance cannot take the lease. The core refuses any command from the adapter's principal outside its three capabilities. A key-hygiene scan finds no key material in environment, logs or errors |
| T-016-02 | Light-client connection, metadata-driven encoding, start-up assertions and status heartbeat | `REQ-SA-2`, `REQ-AS-2`, `AC-E4.2` | A wrong genesis, an unlisted `spec_version`, a missing instance or a decimals mismatch each stops service. The core then refuses `submit_outbound` with `ERR-SettlementLayerUnavailable` |
| T-016-03 | Capability manifest, its registration through `CTR-OPS`, and the submission-time check test | `REQ-SA-1`, `REQ-OS-2`, `AC-E4.1` | An unsupported asset or operation is refused at submission with `ERR-UnsupportedLeg`, before any reservation is used |
| T-016-04 | Instruction intake: validation, durable insert, `ack_instruction` through ingress | `REQ-SO-5`, `REQ-SO-6`, `REQ-SA-5`, `INV-13` | The core reaches `Submitted` only after the row is durable. An instruction without a reservation is never acknowledged. A digest conflict raises an alarm and nothing else |
| T-016-05 | Nonce and era manager; sign, persist, broadcast; replay after restart | `REQ-SA-6`, `REQ-SO-7`, `REQ-SL-4`, `REQ-SO-8` | Fault injection at every step never yields two extrinsics for one `op_ref`. An uncertain nonce is never reused |
| T-016-06 | Inbox cursor reader, observation reports through ingress, verdict storage, explicit gaps | `REQ-SA-7`, `REQ-SI-1`–`SI-5`, `REQ-SL-3`, `INV-12`, `AC-D2.2` | The cursor never passes an unread sequence. A pruned range becomes a visible gap. A repeated coordinate gets "already applied" |
| T-016-07 | Dispositions, including non-inclusion proofs and the never-signed tombstone | `REQ-SA-3`, `REQ-SA-4`, `REQ-SO-4`, `INV-11`, `AC-E1.4`, `AC-E1.5` | Each disposition has a test on a local Kreivo. Unknown never produces a report that could release value |
| T-016-08 | Liquidity and fee checks, parking, one alert per position, automatic resume | `REQ-SN-2`, `AC-E3.1`, `AC-E3.2`, `AC-E3.3` | 20 withdrawals on a short position raise one alert and all resume after a top-up. A partly executed multi-leg operation is never parked |
| T-016-09 | Completeness re-scan against the core's recorded coordinates | `REQ-IN-5`, `REQ-IN-6`, `AC-D2.1` | A coordinate removed from the core's test copy is reported as an incident on the next run |
| T-016-10 | Restart matrix and end-to-end run against a local Kreivo with the pallet | `REQ-SA-5`, `AC-K3.1`, exit criterion | The exit criterion in `plan.md` passes in CI |
| T-016-11 | Report each signing key's account nonce with every finalised head, read at that block; record each signed transaction's key and nonce with the core before broadcasting it, and each tombstone (owner ruling, 2026-09-24, #88) | `REQ-SA-4`, `REQ-SA-6`, `REQ-SL-4`, `INV-11` | An included transaction whose inbox entry the reader misses goes to review on the fake chain; nothing is broadcast before the core holds its signing |
| T-016-12 | The adapter's tests expect a proved rejection released (amendment 0006 item 3); no adapter change | `REQ-OP-9`, `REQ-SA-4` | A refused call in a batch is released once final; the dispositions test says so |
| T-016-13 | The adapter's `profile` (ruled 2026-10-02): in production its store's connection string must say `sslmode=require`, checked as ingress checks it, before any connection | `REQ-SA-6`, `REQ-AU-8` | The profile has no default; the README's example and the canary's template are `production` and require TLS |
