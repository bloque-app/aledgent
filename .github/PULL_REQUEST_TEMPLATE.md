<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

## What and why

<!-- One change. Cite the task (T-NNN-XX) and the requirement it serves (REQ-*, INV-*, AC-*). -->

## Tests

<!-- Named after the AC-*, INV-*, REQ-* or ERR-* they verify; an invariant at the layer SPEC.md §9 names. -->

## Checklist

- [ ] `cargo fmt`, `cargo clippy -D warnings` and `cargo test` pass for every workspace touched
- [ ] Every new file has an SPDX header (`tools/oss/spdx.py --check`)
- [ ] No `unsafe`, no floating point near an amount, no VOS dependency outside `agents/`
- [ ] Behaviour not in `SPEC.md` is not added (an amendment comes first)
- [ ] Every commit is signed off (`git commit -s`, Developer Certificate of Origin)
