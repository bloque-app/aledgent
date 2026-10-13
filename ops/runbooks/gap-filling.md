<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: an observation gap, and filling it from an archive

`F-018` T-018-08 · `REQ-IN-5`, `REQ-SA-7`, `AC-D2.2`, `NFR-9`

**Pages that lead here:** `AledgentObservationGap`, `AledgentRescanDifference`, `AledgentObservationLag`.
**Owner:** settlement-operations. **Rule that does not bend:** nothing here writes the book or a store by
hand. What the settlement layer recorded reaches the book only as the adapter's observations through the
core's surface; an operation that cannot settle is resolved with evidence through `CTR-OPS`
(`resolve_operation`), never by an edit (`REQ-AT-3`, `INV-11`).

## What a gap is

The adapter reads the settlement layer's inbox in order. When a range below the finalized head is pruned
by the node it reads, it records the range as a gap, raises `gap:<instance>:<from>`, reports it to the
core, and only then moves past it (`F-016` decision 16). A re-scan compares the whole inbox with what the
adapter stored and what the core ingested; each difference raises `rescan:` or `incident:` alerts.

## Procedure

1. **Establish the range.** Read the adapter's open alerts and gaps (its store: `gaps`, `alerts`, or the
   `aledgent_adapter_alerts` panel). Note the instance and each `[from, to)`.
2. **Read the range from an archive node.** Run a second adapter process — same configuration, a
   different `holder` name so it never takes the signing lease — against an archive node that holds the
   range, and run its re-scan over it. It reads only; it signs nothing. The re-scan lists every position
   the layer holds that the core never ingested (`incident:missing:*`).
3. **Ingest.** `TODO(F-016)`: reporting a skipped range's records to the core from an archive is not
   built (`F-016` Deferred: "Filling a gap from an archive is not built"). Until it is:
   - a missing **withdrawal** record leaves its operation in `Submitted` until its deadline, then in
     `Needs-Review` (`REQ-OP-6`); resolve it with the archive's record as evidence:
     `POST /v1/ops/operations/{reference}/resolve` with a justification;
   - a missing **deposit** has no operation yet: escalate to the owner; do not issue by hand.
4. **Verify.** Re-run the re-scan against the primary node. Every `incident:missing:*` for the range is
   accounted for. Clear nothing by hand: the adapter clears what its next reads resolve.

## Exercised

In CI: `services/settle/tests/flow.rs` (a pruned range recorded as a gap, reported, passed; the re-scan's
differences raised), `ops/tests/restore.rs` (the restored adapter's re-scan reconciles), and
`ops/tests/alerts.rs` (a gap, a re-scan difference and a lag each page). The archive-node step needs a
live archive node and runs in staging: deferred.
