<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-002 — tasks

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-002-01 | Fix the reference host and the pins. Write `spikes/README.md`, the CSV result format, and the `notes/MEASUREMENTS-F002.md` template with the decision rules filled in before any run | `NFR-1`, `DEC-4` | The template is committed with the rules. The host's specification is recorded |
| T-002-02 | S1: Criterion benches of `Book::apply` over the six posting shapes and batch sizes 1, 4, 10 and 64, with a split of the time per phase | `NFR-1`, `REQ-KER-2`, `REQ-KER-10` | CSVs and a summary table are recorded. Each run asserts `F-001`'s golden roots |
| T-002-03 | S2: build the kernel for the VOS guest target, and run the shapes in a Local Agent actor with and without `pvm-precompile`. Record `gas_used`, `execution_us`, and whether the `signing` feature builds | `NFR-1`, `DEP-5` | Figures are recorded, or "not available" with the exact cause |
| T-002-04 | A book generator with Zipf activity at 10 000, 100 000 and 1 000 000 accounts in `FileStore`, and growth projections at `NFR-2`'s volume for both `DEC-2` options | `NFR-2`, `NFR-3`, `DEC-2` | Books build reproducibly from a seed. The growth table is recorded |
| T-002-05 | S3: witness size, build time, verify-and-apply time (native and guest) and root-after from proofs, for a transfer and both reserve options. Also leaf point-read latency. Compare with the host's limits | `DEC-4`, `DEP-1`, `NFR-3` | Tables are recorded at the three sizes. The witness-derived root equals `FileStore`'s |
| T-002-06 | S4: a minimal Agent actor at about 64 KiB, 1 MiB, 2 MiB and 4 MiB of Linear state. At least 1 000 invocations each, with caller latency and the dispatch and runtime debug fields | `NFR-1`, `DEP-1`, `DEP-5` | p50, p95, p99 and max are recorded per size. The fixed cost and the cost per byte are separated |
| T-002-07 | Apply the decision rules. Write the recommendation on `DEC-4` and the holding-option numbers for `F-004`. Draft the section answering the upstream questions to @olanod (VOS's maintainer) | `DEC-4`, `DEC-2`, `DEP-1` | The owner's ruling on `DEC-4` is recorded, which closes the `M-0` gate |
