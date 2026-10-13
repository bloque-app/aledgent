-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store, F-027: a transaction is a batch of calls on the settlement contract, so
-- several operations share one transaction. Each still has exactly one, ever (REQ-SO-7: the
-- primary key on the operation stays); the transaction's hash is no longer unique per row, and
-- the rows of one batch share its lane, nonce and bytes.

ALTER TABLE settle_extrinsics DROP CONSTRAINT settle_extrinsics_hash_key;

CREATE INDEX settle_extrinsics_hash ON settle_extrinsics (hash);

-- One transaction, one lane and one nonce: rows sharing a hash never disagree on them.
CREATE FUNCTION settle_batch_consistent() RETURNS trigger AS $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM settle_extrinsics
        WHERE hash = NEW.hash AND (lane <> NEW.lane OR nonce <> NEW.nonce)
    ) THEN
        RAISE EXCEPTION 'the rows of one transaction share its lane and nonce';
    END IF;
    RETURN NEW;
END
$$ LANGUAGE plpgsql;

CREATE TRIGGER settle_batch_consistent BEFORE INSERT ON settle_extrinsics
    FOR EACH ROW EXECUTE FUNCTION settle_batch_consistent();
