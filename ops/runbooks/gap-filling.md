<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: a chain mismatch, an unreadable range, a reset, and a gap

`F-018` T-018-08, `F-043` T-043-09 and T-043-15 · `REQ-IN-5`, `REQ-SA-7`, `REQ-SL-9`, `REQ-SA-10`
(amendment 0041, `[PENDING AMENDMENT 0041]`), `AC-D2.2`, `NFR-9`

**Pages that lead here:** `AledgentSettlementChainMismatch`, `AledgentObservationStopped`,
`AledgentObservationGap`, `AledgentObservationLag`, `AledgentSettlementEpochRefused`. **Owner:** settlement-operations. **Rule that does not bend:** nothing here
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
unreadable: the adapter raises `unreadable:<instance>:<position>` and does not advance, until a reader
serves it or the operator records a gap (below). A reset of the instance (a new sequence of the record,
from position 0) is reported to the core, which follows it unless it refuses (amendment 0041, "After a
reset" below); while it does not, nothing of the new chain reaches the book. A chain mismatch pages as `AledgentSettlementChainMismatch` (alert kind `chain:<instance>:<position>`;
also a first-start cursor past the chain's start). `unreadable` and `reset` page as
`AledgentObservationStopped`: all three need an operator.

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
4. **A reset by governance** (`AnchorHeadSet`) starts a new chain at position 0. The adapter reports it to
   the core, which follows it when nothing of the chain before can be confused with the new one; then no
   alert is raised. Otherwise follow "After a reset" below. If nobody announced the reset, treat it as
   step 3's second case and page security, whatever the core did.
5. **If no node can serve the range**, choose, and write the choice on the incident:
   - **Wait**, for a node that keeps the range. The rest of the book goes on.
   - **Record a gap.** Have the adapter record the range as a gap through its own authoritative source
     (the core refuses a gap from any other, `REQ-AT-3`): stop the running adapter (the command takes its
     lease), then run once `aledgent-settle <config> gap <from> <to>` with the range the alert names, and
     start the adapter again. Use the range the command reports: it prints what it recorded, and if it
     refuses the range it says which range it can record. The adapter then accepts the layer's head as it
     stands, **unverified for that range**, and the gap is on the audit trail. The book keeps only the end of its highest gap, so `proof` answers
     `Unprovable` for any range that starts below that end, not only for positions inside the gap
     (`REQ-SA-7`).
6. **Ingest what the gap hid.** `TODO(F-016)`: reporting a skipped range's records to the core from an
   archive is not built (`F-016` Deferred: "Filling a gap from an archive is not built"). Until it is:
   - a missing **withdrawal** record leaves its operation in `Submitted` until its deadline, then in
     `Needs-Review` (`REQ-OP-6`); resolve it with the archive's record as evidence:
     `POST /v1/ops/operations/{reference}/resolve` with a justification;
   - a missing **deposit** has no operation yet: escalate to the owner; do not issue by hand.
7. **Verify.** At the next sync the adapter accepts the head: the `chain` or `unreadable` alert closes,
   `aledgent_observation_cursor_lag` returns to about 0, and the instance's accepted position equals the
   layer's. After an operator's gap, the `gap:` alert it raised (`AledgentObservationGap`) closes once the
   adapter has accepted the head past the range. If it stays open after that, it is the adapter's
   defect, not the operator's to clear: raise it with settlement-operations. Clear nothing by hand.

## After a reset

A reset of the instance starts a new sequence of its record (`anchor_epoch()`: its number, the block, and
the book it names). With `adapter.report_resets` and ingress's `settlement_epochs` on (both off by default;
on only once the core runs a build that knows the input, see the settle README), the adapter reports it
to the core (amendment 0041): live, when it reads the reset past the head it verified and nothing of the
chain before it was left unreported (it first reports any entry it verified before the reset that the core
had not taken in); and once per sequence,
when it moved to the new chain on its own at a first start. The core's answer is kept in the adapter's
store (`settle/epoch/<instance>`).

**While held, withdrawals are refused.** While `reset:<instance>`, `epoch:` or `epoch-unsupported:` is open,
the adapter keeps stepping but declares to the core that it does not serve (`aledgent_adapter_serving` stays
1; the core's status shows it unavailable). New withdrawals, and the settlement comparison, are refused with
`ERR-SettlementLayerUnavailable`: this is not an adapter outage. It lifts when a start is taken (every
`epoch` alert of the instance clears) or with `refound`.

**A head reported and not taken** (amendment 0041's guard, `T-043-17`, a core at `SETTLEMENT_EPOCHS`). When
the adapter goes on sending heads of a new chain the core has not followed (a first start on it, test's
case), the core records the first such head and refuses new withdrawals with the same error while it is
set, though the adapter declares itself serving. Read it on `GET /v1/ops/settlement/head` (a 503 `ERR-DecisionUnavailable` from it with a healthy core means ingress's
`settlement_head` is off: the deployment has not said its core host decodes the query, `T-043-19`): `reported` is
that head, `head` the stale one the core holds. It clears only when the core takes a head again: when a
start is taken, or when the new chain's position passes the stale head's by itself (on test, 11 positions).
Nothing clears it on a timer. While it is set and a start is refused `in_flight`, the instruction in flight
cannot expire (no head of the new chain is taken): resolve it through `CTR-OPS` (below) to get out.

By the core's answer:

- **It follows it** (`started`, or `recorded`): the core's finalised head restarts at the reset's block
  and position 0, and the new chain's entries and heads reach it. Nothing to do. Check it with
  `GET /v1/ops/settlement/head`: the sequence and a head on the new chain.
- **It refused it** (`AledgentSettlementEpochRefused`, `epoch:<instance>:<epoch>`, with the reason). Nothing
  of the new chain reaches the core, and new withdrawals are refused (the adapter is held). The alert names
  the operation in flight or the position of the final record. The adapter reports the start again every
  ten minutes, and goes on by itself once the core takes it. By reason:
  - `in_flight`: a withdrawal instructed before the reset is not decided. It cannot be proven against the
    new chain: resolve it with evidence through `CTR-OPS` (`POST /v1/ops/operations/{reference}/resolve`,
    with the layer's record or its absence from the chain before the reset as evidence). Once nothing is in
    flight, the next attempt is taken.
  - `earlier_final`: the core holds final records of the chain before at positions the new one reuses. The
    book cannot follow this reset (amendment 0041's trade-off): it needs a re-founding. Escalate to the
    owner; do not record a gap for it.
  - `not_later`: the reset is not later than one the core holds. Treat it as step 3's second case.
  - an `ERR-` code (the adapter's authority lost, a limit): the command was refused before the core could
    decide it. Fix the cause; the next attempt is taken. `ERR-NotAuthorised` can also mean that ingress is
    older than amendment 0041 and has no epoch route: check its version before the adapter's grant.
- **Another book's reset** (`reset:<instance>`, the reason `other_book`): the reset names another book, as
  a re-founding does. (A book re-founded after a reset that named the book before it stays here: `refound`
  cannot help while the chain names the earlier book. Turn `report_resets` off, and escalate.) Only with the
  owner's announcement of the book's re-founding:
  1. The book has been re-founded and the adapter's configuration points at it. Stop the running adapter.
  2. Run once `aledgent-settle <config> refound`. It takes the lease, resets the instance's cursor and
     accepted head, forgets the core's answer, and exits.
  3. Start the adapter. It starts on the new chain at position 0, reports the sequence to the new book's
     core, and nothing of the old chain is reported.
  4. Verify that `reset:` closes and `AledgentObservationStopped` clears, then that
     `aledgent_observation_cursor_lag` is about 0 and the next anchor goes final.
- **Entries never read** (`reset:<instance>`, "entries of the chain before it past the verified head were
  never read"): the reset is not followed, and the adapter takes no gap while `reset:` is open. Escalate to
  the owner: the book needs a re-founding (as above), and the operations those entries settled are
  resolved as in step 6.
- **Entries verified and not taken in** (`reset:<instance>`, "verified ... were not taken in by the core"):
  with `report_resets` on the adapter reports them first, every step; the `report:` alerts say why the core
  does not take them. Once taken they are final records at positions the new chain reuses, so the start is
  then refused (`earlier_final`): the book needs a re-founding, as for `earlier_final` above. With it off, the reset stops as before. `refound`
  drops them: only with the owner, as for entries never read.
- **A reset found too late** (`reset:<instance>`, "the chain before it could not be read up to the reset"):
  the adapter could not read the blocks since its verified head across a reset (no history endpoint, or a
  node that failed a read). It is held, reports nothing more, and reads the range again every step; once
  the range reads, it takes the reset as a live one (above) and the alert closes. Do **not** record a gap:
  the adapter refuses one that spans a reset, since it would take the new chain's head as the old one's and
  lose the new chain's entries. If the range stays unreadable, give the adapter a history endpoint that
  reaches back to the position named, or escalate to the owner for a re-founding.
- **Switch off** (`reset:<instance>`, "`report_resets` is off"): the core was not told. A reset belongs to a
  re-founding window: follow "Another book's reset" with the owner, or turn the switches on once the core
  can take the start.
- **A core that cannot take it yet** (`AledgentSettlementEpochUnsupported`, which pages, since withdrawals are off,
  `epoch-unsupported:<instance>:<epoch>`): the core runs code before amendment 0041's rules version. On a first
  start the adapter goes on, and the core's head stays where it was; the adapter is held, so withdrawals are
  refused: upgrade the core, or open ingress's `settlement_epochs` if it is the route that is closed (the release order is the core first, then the adapter). On a live reset the
  adapter also stops (`reset:`) until the upgrade; it then goes on by itself.

With `report_resets` off, every reset under a running adapter is `reset:<instance>`, as before: follow
"Another book's reset" above.

A reset the owner did not announce is not re-founded: the adapter stays stopped for the book, and the
case goes to security ([divergence](release/divergence.md)).

## Exercised

In CI: `services/settle/tests` (`F-043`'s test plan: an idle sync is one read; an entry hidden, added or
altered by a reader stub stops the adapter at the first new position, with an alert; behind more than 256
blocks is unreadable without a history endpoint and caught up with one; a gap recorded for the named range
lets it accept the head; a reset under a running adapter stops its reports until `refound`; and, for amendment
0041, `services/settle/tests/epochs.rs`: a live reset followed, a refused start stopping the adapter and asked
again every ten minutes, a core upgraded later following the reset, another book's reset stopping until
`refound`, unreported entries keeping a reset from being followed, and test's first-start case), `ops/tests/restore.rs` (a
restored adapter resumes from its accepted head), and `ops/tests/alerts.rs` (a gap, a chain mismatch, a stopped
observation of either kind, a refused sequence and a lag each page; a sequence the core cannot take yet warns). The second-node step needs a live node and runs in staging: deferred.
