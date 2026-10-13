<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->


### Book generation, 1000000 accounts, seed 1, store index

| Accounts | Postings + creations | Units | Wall time s | Unit p50 ms | Unit p99 ms | Root |
|---:|---:|---:|---:|---:|---:|---|
| 1000000 | 3000000 | 2000 | 1611 | 802 | 1258 | `2ebcbf6c901db2550dbce7841b4e261daf6da53af18a0662ccdf3ab71e98b607` |

### S3 at 1000000 accounts (IndexStore in memory; release; n = 1000 samples per timing)

Book: 5000003 leaves [1000002, 2000000, 1, 2000000, 0, 0] (accounts, transfers, journals, external ids, voided, pending). Account leaf content 313 B.

| Case | Witness B | Frontier nodes (acc/xfer/jrnl/eid/void/pend) | Proofs B | Events B | Openings B | Creation B | Build p50 µs | Build p99 µs | Verify+apply p50 µs | Verify+apply p99 µs | Root-after p50 µs | Root-after p99 µs |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| capture-for-less | 6936 | 50/23/1/21/0/0 | 4804 | 624 | 440 | 0 | 256.6 | 1080.5 | 1010.2 | 2786.2 | 199.2 | 342.3 |
