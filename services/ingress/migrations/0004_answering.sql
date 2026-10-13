-- F-009, T-009-18 (amendment 0013, REQ-CS-9): a principal's answering setting. Its transfers and
-- reservations are answered with their decision, waiting at most `wait_ms`, instead of acknowledged at
-- Received. No row is the default: acknowledged. The setting shapes ingress's replies, not the book,
-- so it lives here; who set it and when are kept with it (REQ-AU-7).
--
-- Version 4, after T-009-16's version 3 (#126).

CREATE TABLE principal_answering (
    principal  TEXT    PRIMARY KEY,
    decided    BOOLEAN NOT NULL,
    wait_ms    BIGINT  NOT NULL CHECK (wait_ms > 0),
    set_by     TEXT    NOT NULL,
    set_at     BIGINT  NOT NULL
);
