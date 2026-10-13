-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store, F-017: the anchors the adapter was asked to publish, and the
-- extrinsics that carry them (REQ-SA-5, REQ-SA-6). One row per anchor number; its commitment
-- never changes.

CREATE TABLE settle_anchors (
    number      BIGINT PRIMARY KEY,
    commitment  TEXT   NOT NULL,
    body        TEXT   NOT NULL
);

CREATE FUNCTION settle_anchor_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.commitment <> OLD.commitment OR NEW.number <> OLD.number THEN
        RAISE EXCEPTION 'an anchor''s commitment never changes';
    END IF;
    RETURN NEW;
END
$$ LANGUAGE plpgsql;

CREATE TRIGGER settle_anchor_immutable BEFORE UPDATE ON settle_anchors
    FOR EACH ROW EXECUTE FUNCTION settle_anchor_immutable();
