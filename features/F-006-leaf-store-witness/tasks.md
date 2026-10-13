<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-006 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-006-01 | Define `DeltaRecord` and its canonical encoding in `crates/ledger-core`, with the log hash chain | `DEC-4`, `REQ-RT-1`, `REQ-RT-3` | Encoding is deterministic across runs and targets. Building a record over 64 KiB fails |
| T-006-02 | Storage engine behind one trait; SMT node store with proofs byte-compatible with CipherClerk's `SmtProof`; implements `F-001`'s storage trait | `DEC-4`, `INV-2`, `REQ-KER-5` | CipherClerk's reference root and proofs match the store's for the same batches |
| T-006-03 | Service tree for the library's growing records (Proposal) | `DEC-4`, `REQ-RT-1`, `INV-16` | Service root folds deterministically. Inclusion and non-inclusion proofs verify |
| T-006-04 | Witness builder: read set in, `clerk-witness`-format witness out | `DEC-4`, `REQ-RT-3` | Witnesses for random states verify. Tampered or incomplete witnesses are rejected |
| T-006-05 | Applier with gap refusal, root check, no-overwrite rule, idempotent re-apply; read views pinned to `applied_sequence`; read-after-acknowledge overlay | `INV-2`, `REQ-QR-2`, `REQ-BA-3` | A mismatching `roots_after` stops the applier. A read after acknowledgement reflects the command |
| T-006-06 | Delta archive: append-only segments, fsync before ack, chain verified against the Agent's head | `REQ-RT-1`, `NFR-4`, `DEC-4` | A truncated or altered segment is detected on read. No trim happens before fsync |
| T-006-07 | History and indexes: entries by (account, sequence), accounts by identity, authority holders by account, reserved totals | `REQ-PO-2`, `REQ-QR-5`, `INV-16`, `REQ-BA-6`, `NFR-3` | Index contents equal a full recomputation from leaves |
| T-006-08 | Openings store: sealed keyspace, separate backup, never in leaf store, record or archive | `REQ-RT-6`, `REQ-KER-8`, `DEC-11` | A scan of the leaf store, records and archive finds no opening bytes |
| T-006-09 | Rebuild from genesis, canonical dump and digest comparison; commitment checks for `INV-6` and `INV-5` | `INV-6`, `INV-5`, `AC-K5.1`, `REQ-RT-1` | Exit test green, including crash-injected runs |
| T-006-10 | Scale benchmarks: balance read at 150M entries with most of them on one account; witness, apply and rebuild rates | `NFR-3`, `NFR-10`, `NFR-2` | Numbers recorded in `notes/`. `NFR-3` p99 met |
