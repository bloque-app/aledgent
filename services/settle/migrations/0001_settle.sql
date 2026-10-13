-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store (F-016, REQ-SA-5): a database and a role of its own, apart from
-- ingress's. Rows are JSON documents keyed by the columns the store's rules need.

-- The single-active lease (Kreivo's runtime): one row.
CREATE TABLE settle_lease (
    id          SMALLINT PRIMARY KEY CHECK (id = 1),
    holder      TEXT     NOT NULL,
    epoch       BIGINT   NOT NULL,
    expires_at  BIGINT   NOT NULL
);

-- What the adapter was asked: one instruction per operation reference.
CREATE TABLE settle_instructions (
    operation  TEXT PRIMARY KEY,
    digest     TEXT NOT NULL,
    body       TEXT NOT NULL
);

-- Signed extrinsics, stored before they are broadcast (REQ-SA-6). One per operation reference,
-- ever (REQ-SO-7): the primary key refuses a second, whoever writes it.
CREATE TABLE settle_extrinsics (
    operation  TEXT   PRIMARY KEY REFERENCES settle_instructions (operation),
    lane       TEXT   NOT NULL,
    nonce      BIGINT NOT NULL,
    hash       TEXT   NOT NULL UNIQUE,
    body       TEXT   NOT NULL
);

-- A stored extrinsic's bytes never change: only its status does.
CREATE FUNCTION settle_extrinsic_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.hash <> OLD.hash OR NEW.operation <> OLD.operation
       OR NEW.lane <> OLD.lane OR NEW.nonce <> OLD.nonce THEN
        RAISE EXCEPTION 'a stored extrinsic never changes';
    END IF;
    RETURN NEW;
END
$$ LANGUAGE plpgsql;

CREATE TRIGGER settle_extrinsic_immutable BEFORE UPDATE ON settle_extrinsics
    FOR EACH ROW EXECUTE FUNCTION settle_extrinsic_immutable();

-- Nonce lanes: one key each.
CREATE TABLE settle_lanes (
    lane  TEXT PRIMARY KEY,
    body  TEXT NOT NULL
);

-- What the network did: inbox entries as read.
CREATE TABLE settle_observations (
    instance  BIGINT NOT NULL,
    seq       BIGINT NOT NULL,
    digest    TEXT   NOT NULL,
    body      TEXT   NOT NULL,
    PRIMARY KEY (instance, seq, digest)
);

-- What the adapter told the core, and the verdict.
CREATE TABLE settle_reports (
    key   TEXT PRIMARY KEY,
    body  TEXT NOT NULL
);

-- The inbox cursor: never goes back.
CREATE TABLE settle_cursors (
    instance  BIGINT PRIMARY KEY,
    next_seq  BIGINT NOT NULL
);

-- Explicit gaps (REQ-SA-7).
CREATE TABLE settle_gaps (
    instance  BIGINT NOT NULL,
    from_seq  BIGINT NOT NULL,
    body      TEXT   NOT NULL,
    PRIMARY KEY (instance, from_seq)
);

-- Alerts: one per position.
CREATE TABLE settle_alerts (
    key   TEXT PRIMARY KEY,
    body  TEXT NOT NULL
);
