<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-008 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-008-01 | Define the `Transfer` and `Leg` command types. Write the fixed validation order, up to and including the authority call into `F-003` | `REQ-CS-1`, `REQ-AS-4`, `AC-B1.4`, `ERR-ZeroAmount`, `ERR-UnknownAsset`, `ERR-PrecisionExceeded`, `ERR-UnknownAccount`, `ERR-AccountFrozen`, `ERR-AccountClosed` | The validation-order test passes. `AC-B1.4` passes without a kernel call |
| T-008-02 | Build the kernel posting for one leg: two `Settled` entries, currency tag from the registry, signatures from the `DEC-3` key ring, `external_id` from `F-007` | `AC-B1.1`, `INV-1`, `REQ-KER-1`, `DEC-3` | A leg posts as one balanced kernel transfer. A test shows that a caller cannot set the currency tag |
| T-008-03 | Wire the lifecycle: `Received → Accepted → Settled`, or `Accepted → Rejected` on refusal. Write the kernel-status mapper | §5.6, `AC-B1.2`, `INV-3`, `REQ-ER-1` | `AC-B1.1` and `AC-B1.2` pass. Each mapped status has a test |
| T-008-04 | Assert no settlement contact on the transfer path | `AC-B1.3`, `REQ-SL-5` | After 1,000 transfers, the outbox holds no instruction and the simulated layer is empty |
| T-008-05 | Build multi-leg as a terminated linked chain, one operation reference, one posting per leg | `US-B3`, `AC-B3.1`, `AC-B3.2`, `REQ-KER-10` | `AC-B3.1` and `AC-B3.2` pass. The random failing-leg property test finds no partial unit |
| T-008-06 | Add `max_legs` to the deployment declaration and check it first | `AC-B3.3`, `ERR-BatchTooLarge` | `AC-B3.3` passes with zero store reads on the refused path |
| T-008-07 | Prove idempotency at both strengths: client-key replay, and kernel `external_id` refusal of a replayed posting | `REQ-OP-11`, `REQ-KER-6`, `REQ-OP-3` | A replayed unit returns the first reply. A direct replay to the kernel is refused |
| T-008-08 | Register the `AC-B1.*`, `AC-B3.*`, `INV-1` and `INV-3` tests in the `F-015` conformance suite | `NFR-14` | The suite lists every test and passes |
| T-008-09 | `max_legs` becomes a deployment setting in the genesis configuration, published by `limits()` (amendment 0020 R3, `REQ-CS-10`); `AC-B3.3` keeps refusing one leg more with `ERR-BatchTooLarge` before anything is read | `REQ-CS-10`, `AC-B3.3`, `AC-B3.4` | A deployment configured with `max_legs = n` takes in `n` legs and refuses `n + 1`. Production keeps 10 until it is measured on the production core (`a0020-q3`, ruled 2026-10-05) |
