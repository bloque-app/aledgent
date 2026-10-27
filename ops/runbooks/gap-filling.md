<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: a chain mismatch, an unreadable range, and a gap

`F-018` T-018-08, `F-043` T-043-09 · `REQ-IN-5`, `REQ-SA-7`, `REQ-SL-9`, `AC-D2.2`, `NFR-9`

**Pages that lead here:** `AledgentSettlementChainMismatch`, `AledgentObservationGap`,
`AledgentObservationLag`. **Owner:** settlement-operations. **Rule that does not bend:** nothing here
writes the book or a store by hand. What the settlement layer recorded reaches the book only as the
adapter's observations through the core's surface; an operation that cannot settle is resolved with
evidence through `CTR-OPS` (`resolve_operation`), never by an edit (`REQ-AT-3`, `INV-11`).

## What the adapter checks

The settlement contract keeps one value for its entries: the position and the digest of the latest one,
where each entry's digest covers the one before it (`REQ-SL-9`, amendment 0040). It stores no entry.
The adapter keeps the last head it accepted for each instance, with the block it read it at, and at each
sync:

1. reads the head at the finalized block. If it has not moved, that is all: one read;
2. otherwise reads the events of the blocks since, **folds** the new entries from the accepted digest,
   and compares the result with the head. On a match it reports the entries to the core in order, and
   accepts the head once the core has taken in every entry up to it;
3. on a difference, reads once more. If the result still differs, it raises
   `AledgentSettlementChainMismatch` and does **not pass the first new position**. It keeps submitting and
   anchoring (`REQ-PR-9`): only observation past that position waits. No timer re-reads old ranges.

A full node keeps about 256 blocks of events, so routine settlement needs no archive node. An adapter
further behind catches up from a node that keeps history. If none can serve a block, the position is
unreadable: the adapter raises an alert and does not advance, until a reader serves it or the operator
records a gap (below).

Incidents and gaps are written to the audit trail (`NFR-6`, `REQ-AT-1`), not to the book's state.

## What the operator does

1. **Establish what stopped.** Read the adapter's open alerts (`aledgent_adapter_alerts`, or its store's
   `alerts`) and the audit trail's latest incident for the instance. Note the instance, the position the
   adapter accepted, the head the layer holds, and the first position it did not pass.
   `aledgent_observation_cursor_lag` says how far behind observation is.
2. **Let it re-read.** A difference the node caused (one that is behind the finalized head it reported,
   say) clears at the next sync with no action: the alert closes itself when the fold
   matches. Do not clear it by hand. If it persists across two syncs, it is real or the node is wrong.
3. **Check against a second node.** Run a second adapter process with the same configuration, a different
   `holder` name so it never takes the signing lease, against another node (an archive node for an old
   range). It reads only; it signs nothing. If it folds to the layer's head, the first node was wrong:
   point the primary at the second, or replace the first (`release/replace-node.md`). If it also differs,
   treat it as a divergence of the layer's record: page security and follow
   [divergence](release/divergence.md). Do not accept the head.
4. **A reset by governance is not a mismatch, but looks like one to a stale cursor.** The layer's reset
   (`AnchorHeadSet`) starts a new chain at position 0. The adapter's accepted head belongs to the chain
   before it, and is re-founded with the book (`F-043`). If the alert follows a reset nobody announced,
   treat it as step 3's second case.
5. **If no node can serve the range**, choose, and write the choice on the incident:
   - **Wait**, for a node that keeps the range. The rest of the book goes on.
   - **Record a gap.** Ask the owner of settlement-operations to have the adapter record the range as a gap
     through its own authoritative source (the core refuses a gap from any other, `REQ-AT-3`). The adapter
     then accepts the layer's head as it stands, **unverified for that range**, and the gap is on the audit
     trail. The book keeps only the end of its highest gap, so `proof` answers
     `Unprovable` for any range that starts below that end, not only for positions inside the gap
     (`REQ-SA-7`).
6. **Ingest what the gap hid.** `TODO(F-016)`: reporting a skipped range's records to the core from an
   archive is not built (`F-016` Deferred: "Filling a gap from an archive is not built"). Until it is:
   - a missing **withdrawal** record leaves its operation in `Submitted` until its deadline, then in
     `Needs-Review` (`REQ-OP-6`); resolve it with the archive's record as evidence:
     `POST /v1/ops/operations/{reference}/resolve` with a justification;
   - a missing **deposit** has no operation yet: escalate to the owner; do not issue by hand.
7. **Verify.** At the next sync the adapter accepts the head: the alert closes,
   `aledgent_observation_cursor_lag` returns to about 0, and the instance's accepted position equals the
   layer's. Clear nothing by hand.

## Exercised

In CI: `services/settle/tests` (`F-043`'s test plan: an idle sync is one read; an entry hidden, added or
altered by a reader stub stops the adapter at the first new position, with an alert; behind more than 256
blocks is unreadable without a history endpoint and caught up with one), `ops/tests/restore.rs` (a
restored adapter resumes from its accepted head), and `ops/tests/alerts.rs` (a gap, a chain mismatch and a
lag each page). The second-node step needs a live node and runs in staging: deferred.
