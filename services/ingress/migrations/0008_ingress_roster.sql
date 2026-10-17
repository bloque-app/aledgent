-- F-038 (DEC-27 (e'), T-038-21): the fleet's roster, so a core host dials every ingress instance of
-- its region directly.
--
-- ingress_roster   one row per live ingress instance: the address its region's cores reach it at
--                  (tls:<host:port> or unix:<path>), and when the entry lapses on the database's
--                  clock. Each instance renews its own row every few seconds and deletes it on drain;
--                  a crashed instance's row lapses. Every core-stream greeting carries the live rows.
--
-- Version 8, after version 7's feed lease. A new table: nothing existing is locked or rewritten.
SET LOCAL lock_timeout = '5s';

CREATE TABLE ingress_roster (
    instance   TEXT        PRIMARY KEY,
    address    TEXT        NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
