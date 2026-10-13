-- F-009, T-009-02: the received-log, delivery attempts and the recipient registry.
--
-- One row per core input: a principal's command, a tick, or a delivery record. A command's row is
-- unique on (principal, surface, client key) (REQ-OP-10, DEP-10). Rows are never deleted.

CREATE TABLE received_log (
    seq            BIGSERIAL NOT NULL UNIQUE,
    op_ref         BYTEA     PRIMARY KEY CHECK (octet_length(op_ref) = 16),
    input_kind     TEXT      NOT NULL CHECK (input_kind IN ('command', 'tick', 'delivery')),
    principal      TEXT,
    credential     TEXT,
    on_behalf_of   TEXT,
    surface        SMALLINT  NOT NULL,
    client_key     TEXT,
    key_derived    BOOLEAN   NOT NULL,
    body_digest    BYTEA     NOT NULL CHECK (octet_length(body_digest) = 32),
    body           BYTEA     NOT NULL,
    reserves       BOOLEAN   NOT NULL,
    synchronous    BOOLEAN   NOT NULL,
    received_at    BIGINT    NOT NULL,
    status         TEXT      NOT NULL CHECK (status IN ('Received', 'Forwarded', 'Decided')),
    forwarded_at   BIGINT,
    core_input_seq BIGINT    UNIQUE,
    core_reply     BYTEA,
    decided_at     BIGINT,
    CHECK (input_kind <> 'command' OR (principal IS NOT NULL AND credential IS NOT NULL
                                       AND client_key IS NOT NULL)),
    CHECK ((status = 'Decided') = (core_input_seq IS NOT NULL AND core_reply IS NOT NULL))
);

-- The client-key domain (DEP-10): (principal, surface, key), for commands.
CREATE UNIQUE INDEX received_log_client_key
    ON received_log (principal, surface, client_key)
    WHERE input_kind = 'command';

-- The forwarder's queue: reservations first, then arrival order.
CREATE INDEX received_log_undecided
    ON received_log (reserves DESC, seq)
    WHERE status <> 'Decided';

CREATE INDEX received_log_pending_kind
    ON received_log (input_kind, client_key)
    WHERE status <> 'Decided';

-- Delivery attempts that did not end in a recorded delivery (REQ-EV-6). Kept for ever.
CREATE TABLE delivery_attempt (
    id           BIGSERIAL PRIMARY KEY,
    notification TEXT      NOT NULL,
    at           BIGINT    NOT NULL,
    outcome      TEXT      NOT NULL,
    next_at      BIGINT    NOT NULL
);

CREATE INDEX delivery_attempt_notification ON delivery_attempt (notification, id);

-- Recipients (REQ-EV-4). The verification secret is sealed with the deployment's key: never
-- stored in the clear.
CREATE TABLE recipient (
    principal     TEXT  PRIMARY KEY,
    endpoint      TEXT  NOT NULL,
    secret_nonce  BYTEA,
    secret_sealed BYTEA,
    CHECK ((secret_nonce IS NULL) = (secret_sealed IS NULL))
);
