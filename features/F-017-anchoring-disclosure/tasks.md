<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-017 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-017-01 | Checkpoint type and commitment in `ledger-core`. Creation in the core, with the anchor instruction in the outbox in the same commit | `INV-19`, `REQ-PR-7`, `REQ-PR-2`, `REQ-RT-1`, `AC-J1.2`, `AC-I1.1` | The `INV-19` property test passes. The anchor instruction type holds only a number and a 32-byte commitment |
| T-017-02 | Cadence trigger on ticks and the on-demand `anchor_checkpoint` operator command | `REQ-PR-1`, `REQ-PR-9`, `REQ-OS-1`, `AC-J1.1`, `AC-J1.4` | A checkpoint appears within one interval when entries exist, and none without. Holder commands run unchanged with the adapter stopped |
| T-017-03 | Anchor finality tracking, commitment match, re-emission after "provably not executed", one anchor in flight | `REQ-PR-8`, `REQ-SL-1`, `AC-J1.3` | Only a matching finalized observation makes a checkpoint `Final`. Citing any other gives `ERR-AnchorNotFinal` |
| T-017-04 | The four assertion shapes and their truth checks at a checkpoint's entry sequence | `REQ-DS-1`, `REQ-DS-4` | A false claim of each shape is refused with `ERR-DisclosureFalse`. An unknown shape gets `ERR-DisclosureUnsupported` |
| T-017-05 | `issue_disclosure`: authority, envelope, clerk-key signature through the private-input path, assurance level, audit record | `REQ-DS-2`, `REQ-DS-5`, `REQ-AR-5`, `REQ-AR-6`, `REQ-PR-6`, `REQ-PV-6`, `REQ-AT-1`, `AC-I2.2`, `AC-I2.3`, `AC-I3.2`, `AC-I3.3` | An unauthorised request gets `ERR-NotDisclosable`. Every disclosure carries the label and the assurance digest, and has an audit record. The recipient gains no read access |
| T-017-06 | `verify` query and the offline verifier crate and CLI | `REQ-DS-3`, `REQ-PR-4`, `AC-I2.1`, `AC-I3.1` | The CLI verifies a disclosure against a local Kreivo with every aledgent process stopped. A tampered disclosure fails |
| T-017-07 | Settlement-position comparator at an inbox cut | `REQ-PR-5`, `AC-J3.1`, `AC-J3.2` | One minor unit sent to the position outside the pallet's calls is reported as a difference of one. Every anchor has a recorded comparison |
| T-017-08 | End-to-end run with the adapter and the pallet on a local Kreivo | `AC-J1.*`, `AC-J3.*`, `AC-I2.*`, `AC-I3.*` | The exit criterion in `plan.md` passes in CI |
| T-017-09 | The second on-demand anchor trigger (amendment 0009, R8, confirmed by the owner on 2026-10-02): a disclosure requested about entries that no final anchor covers yet takes an anchor, and is issued against it once it is final | `REQ-PR-1`, `REQ-DS-2`, `REQ-PR-8` | A disclosure over an entry past the last final anchor causes a checkpoint without an operator command, and is issued citing that checkpoint once it is `Final`, never an earlier one |
