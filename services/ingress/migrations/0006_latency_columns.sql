-- F-009, T-009-25 (`NFR-1`, `NFR-15`, `NFR-16`; owner ruling of 2026-10-13): the latency bounds
-- measured from ingress's own received-log, queryable as it stands (docs/release/latency-from-ingress.md).
--
-- arrived_at    when a command's request arrived, before authentication: where NFR-1 and NFR-16
--               start. Written with the row's INSERT; no write of its own.
-- replied_at    when ingress first sent the HTTP reply to the command that logged the row: the
--               acknowledgement (NFR-1) or the reply carrying the decision (NFR-16). Best-effort,
--               stamped after the reply by a batched writer off the reply's path.
-- completed_at  when the command completed (NFR-15): set with the decision that completes it, in
--               the same UPDATE; for a withdrawal, at the first relay to the requester of Observed
--               or of a terminal state.
-- submitted_at  a withdrawal's only: the arrival of the submit_outbound that submitted its leg,
--               set when that command is decided. A withdrawal's NFR-15 clock starts here, not at
--               its acknowledgement (owner ruling on PR #235).
--
-- Version 6, after T-009-21's version 5. Nullable, no default: PostgreSQL adds each column to the
-- catalogue only, with no rewrite of the table and no scan, so it is instant on a log of any size.
-- Every row logged before it keeps NULL in all four; the queries leave those out. No index: at
-- today's volume the queries scan a window of received_at cheaply; at the capacity plan's rates
-- the doc names the index to build concurrently, outside the migrations.
--
-- Each ADD COLUMN takes the table's ACCESS EXCLUSIVE lock for an instant. Behind a long
-- transaction it would queue every append behind it; past the timeout the migration fails instead,
-- the replica does not start, and the next start tries again.
SET LOCAL lock_timeout = '5s';

ALTER TABLE received_log ADD COLUMN arrived_at BIGINT;
ALTER TABLE received_log ADD COLUMN replied_at BIGINT;
ALTER TABLE received_log ADD COLUMN completed_at BIGINT;
ALTER TABLE received_log ADD COLUMN submitted_at BIGINT;
