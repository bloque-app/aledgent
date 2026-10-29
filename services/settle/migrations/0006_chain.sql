-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store, F-043 T-043-04 (amendment 0040, REQ-IN-5). Beside each instance's
-- cursor, two heads of its chain of entries, each a JSON document `{ position, digest, block,
-- epoch }`:
--
-- - `verified`: the head the adapter's fold of the layer's events last reached, and the finalized
--   block it read it at. The next sync folds only the blocks after it, however long the core takes.
-- - `accepted`: the head the core has taken in every entry below. Never past `verified`.
--
-- Both are NULL on a row written before this migration (a cursor of the contract's earlier code,
-- from before the reset that re-founded the book): the adapter then has no head, and finds the
-- chain's start.
--
-- The entries verified and not yet accepted, by position, as their SCALE bytes: what the adapter
-- reports from, so a core that is down or refusing never sends the adapter back to the layer for
-- them. Removed once accepted.

ALTER TABLE settle_cursors
    ADD COLUMN verified TEXT,
    ADD COLUMN accepted TEXT;

CREATE TABLE settle_chain_entries (
    instance  BIGINT NOT NULL,
    seq       BIGINT NOT NULL,
    raw       TEXT   NOT NULL,
    PRIMARY KEY (instance, seq)
);
