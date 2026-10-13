-- F-028 (DEC-19): durable passkey state at ingress, shared by every replica.

-- Each passkey's sign count, as its last accepted sign-in reported it. It only ever rises.
CREATE TABLE passkey_counters (
    credential TEXT PRIMARY KEY,
    count BIGINT NOT NULL CHECK (count >= 0)
);

-- Every challenge already used, until it expires: a challenge is good for one ceremony.
CREATE TABLE used_challenges (
    nonce BYTEA PRIMARY KEY,
    expires_at BIGINT NOT NULL
);
CREATE INDEX used_challenges_expiry ON used_challenges (expires_at);
