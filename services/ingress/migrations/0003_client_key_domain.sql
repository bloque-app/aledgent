-- F-009, T-009-16 (amendment 0011, REQ-OP-10, REQ-ID-7): the client key's domain is the integrating
-- party the initiating principal is tied to, or the principal itself where it is tied to none.
--
-- Every row logged before this migration was logged for a principal tied to no party (there were
-- no parties), so its domain is its principal. The principal stays a column of its own: the
-- request a key names includes it, and a key used by a principal before it was tied stays in
-- that principal's own domain.

ALTER TABLE received_log ADD COLUMN client_key_domain TEXT;

UPDATE received_log SET client_key_domain = principal WHERE input_kind = 'command';

ALTER TABLE received_log ADD CONSTRAINT received_log_command_domain
    CHECK (input_kind <> 'command' OR client_key_domain IS NOT NULL);

DROP INDEX received_log_client_key;

CREATE UNIQUE INDEX received_log_client_key_domain
    ON received_log (client_key_domain, surface, client_key)
    WHERE input_kind = 'command';

-- Ties ingress has learned (REQ-ID-7): a principal is tied to at most one party, for good.
CREATE TABLE party_ties (
    principal TEXT PRIMARY KEY,
    party     TEXT NOT NULL
);
