-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store: the deposit backing check. Beside each kept entry that records a
-- deposit, what the adapter proved of it from chain state when it verified it: that its block
-- raised the deployment's real holding of the asset at least as much as the position its contract
-- records (`"Backed"`), or not (`{"Unbacked":{"detail":...}}`, JSON). Keyed by the entry's
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
