-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store, F-043 T-043-04 (amendment 0040, REQ-IN-5): beside each instance's
-- cursor, the head of its chain of entries the adapter last accepted, and the finalized block it
-- read that head at. The adapter folds only the entries after it, from that block on. All three are
-- NULL on a row written before this migration (a cursor of the contract's earlier code, from
-- before the reset that re-founded the book): the adapter then has no accepted head, finds the
-- chain's start, and resets the cursor to it.

ALTER TABLE settle_cursors
    ADD COLUMN head_seq    BIGINT,
    ADD COLUMN head_digest TEXT,
    ADD COLUMN head_block  BIGINT;
