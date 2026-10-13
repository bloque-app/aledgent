<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

### Book generation, 10000 accounts, seed 1, store file (FileStore on disk)

Transcribed from the terminal output of `s3 --accounts 10000 --gen-only` (the first run, before the
store name entered the result file name); that run's CSV was overwritten by the measuring run that
reused the book. The same root is re-derived by `s3 --store index --accounts 10000`
(s3-index-10000.md).

| Accounts | Postings | Units | Wall time s | Unit p50 ms | Unit p99 ms | book.redb MiB | vault.redb MiB | Root |
|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 10000 | 20000 | 20 | 180 | 4164 | 35590 | 1028 | 8 | `8de03ac9adc20b64617c7ea74d832c861b4d02fbbd6c3349388864de3ed2da67` |
