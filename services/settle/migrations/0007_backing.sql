-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store: the deposit backing check. Beside each kept entry that records a
-- deposit, what the adapter found when it verified it: that a transfer of its asset and amount from
-- its sender to the contract account paid it in its own extrinsic (`"Backed"`), that none did
-- (`{"Unbacked":{"detail":...}}`), or that its block came from the history endpoint, whose events
-- nothing proves (`{"Unverified":{"detail":...}}`), as JSON. Keyed by the entry's
-- position and digest, so an entry at a position reused after a reset is never taken for another.
-- A deposit with no row here (kept before this migration) is reported without a movement.
-- Removed with the entries, once accepted or on a reset.

CREATE TABLE settle_deposit_backing (
    instance  BIGINT NOT NULL,
    seq       BIGINT NOT NULL,
    digest    TEXT   NOT NULL,
    backing   TEXT   NOT NULL,
    PRIMARY KEY (instance, seq)
);
