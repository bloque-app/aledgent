<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-011 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-011-01 | Build `ReadView` fixed at a committed sequence. Make every query take `&ReadView`, with no `&mut` read method | `REQ-QR-2`, `REQ-QR-3`, `AC-H4.1` | The `AC-H4.1` fuzz run shows root and sequence unchanged after 10k queries |
| T-011-02 | Add `AuthorisedScope` from `F-003` to every storage read method, and filter inside the store | `INV-16`, `REQ-PV-1`, `ERR-UnknownAccount` | The `INV-16` test passes with the check at the store layer |
| T-011-03 | Build `balances` for an account and an identity: `available` from the kernel, `reserved` from holding positions, `encumbered`, labelled `total`. Share `available()` with `F-004` and `can_spend` | `REQ-BA-2`, `REQ-BA-3`, `REQ-BA-5`, `REQ-BA-6`, `AC-H1.1`, `AC-H1.2` | `AC-H1.1` and `AC-H1.2` pass, including with writers running |
| T-011-04 | Build `movements`: the `account_postings` index, the total posting-kind status mapping, authority attribution, counterparty | `REQ-QR-1`, `REQ-QR-4`, `REQ-PO-1`, `AC-H2.2`, `AC-H2.3`, `AC-H5.1`, `INV-20` | `AC-H2.2`, `AC-H2.3` and `AC-H5.1` pass |
| T-011-05 | Build the cursor: `(last_sequence, filter_digest, view_sequence)`, no offset, page maximum | `REQ-QR-5`, `REQ-PO-2`, `AC-H2.1` | `AC-H2.1` passes with no duplicate and no gap under concurrent appends |
| T-011-06 | Build the time-to-sequence index. Rely on `at` never decreasing (the `F-007` commit), with a test that checks it | `REQ-QR-1`, `REQ-PO-2` | Time-range results equal a brute-force scan on random workloads |
| T-011-07 | Build `statement`: resolve `[s0, s1]`, derive from entries, compare with the stored balance, raise an `INV-6` incident on a difference, balance checkpoints | `INV-6`, `AC-H3.1`, `AC-H3.2` | `AC-H3.1` and `AC-H3.2` pass. An injected drift raises the incident |
| T-011-08 | Build `trial_balance` on commitments and on openings. Build `operation(reference)`, `can_spend` and `assets()` | `INV-5`, `REQ-QR-1`, `REQ-QR-3`, `REQ-OP-7`, `REQ-AS-1` | Both trial-balance checks are zero after random workloads. `can_spend` returns a named reason for every refusal |
| T-011-09 | Enforce query bounds with `ERR-QueryUnbounded`. State the index contracts for `F-006` in the storage trait's rustdoc. Register all tests in the `F-015` suite | `REQ-CS-5`, `REQ-QR-6`, `NFR-14`, `DEC-4` | An unbounded request is refused before any read. The trait states each contract, and every `AC-H*` test is in the suite |
| T-011-10 | Movements and `operation(reference)` echo pass-through data to principals of the submitting principal's party only (amendment 0020 R1); `limits()` answered from the deployment's settings | `REQ-PO-5`, `REQ-CS-10`, `REQ-QR-4`, `AC-M8.1`, `AC-M8.4`, `AC-B3.4` | Another party's read of the same movement carries no pass-through data. The echo is **done** at ingress with `T-009-21` (`F-009` `NOTES.md` decision 31): the core's movements and `operation(reference)` carry none, ingress adds it per reader. `limits()` is still open (`T-009-23`). Export and import carry it from ingress's log (ruling `pq8-export` (a), `F-009` decision 31) |
