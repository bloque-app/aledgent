-- F-038 (DEC-27, T-038-03, T-038-10): ingress as its own fleet, with each core host dialing out to it.
--
-- feed_lease       one row per partition the forwarder feeds: who holds the feed, the epoch that rises
--                  every time another holder takes it (the core host fences a lower one), its expiry on
--                  the database's clock, and the next feed sequence. A holder renews it while it can
--                  reach a core for the partition, and expires it on drain.
-- feed_seq         a row's feed sequence, gapless per partition, fixed in the transaction that first
--                  marks it forwarded under the lease (one partition today, the book). Every resend
--                  carries it; the core host orders and de-duplicates by it.
-- feed_epoch       the epoch under which that sequence was given.
-- verifier_copy    the public material of each credential the book last answered for (an API key's
--                  hash, a passkey's encoded key, a key credential's public key), read only when no core
--                  answers. Public material only (REQ-AU-8); "no working credential" deletes the row.
--
-- Version 7, after T-009-25's version 6. The columns are nullable with no default: PostgreSQL adds
-- them to the catalogue only, with no rewrite and no scan. No index on received_log: building one
-- scans the whole log under a lock that holds appends back. The forwarder finds its in-flight rows
-- through received_log_undecided (status <> 'Decided'), a small set; a sequence's uniqueness comes from
-- the lease row, locked FOR UPDATE by every assignment.
SET LOCAL lock_timeout = '5s';

CREATE TABLE feed_lease (
    partition  TEXT        PRIMARY KEY,
    holder     TEXT        NOT NULL,
    epoch      BIGINT      NOT NULL CHECK (epoch > 0),
    expires_at TIMESTAMPTZ NOT NULL,
    next_seq   BIGINT      NOT NULL CHECK (next_seq > 0)
);

ALTER TABLE received_log ADD COLUMN feed_seq BIGINT;
ALTER TABLE received_log ADD COLUMN feed_epoch BIGINT;

CREATE TABLE verifier_copy (
    credential TEXT   PRIMARY KEY,
    identity   TEXT   NOT NULL,
    kind       TEXT   NOT NULL,
    scheme     TEXT   NOT NULL,
    verifier   TEXT   NOT NULL,
    read_at    BIGINT NOT NULL
);
