-- F-009, T-009-21 (amendment 0020 R1, REQ-PO-5, `prq-08`): a command's pass-through data, kept
-- with its row, exactly as the caller sent it. One JSON value of at most 1 KiB of UTF-8; ingress
-- checks both before it appends. Part of the request's digest, never part of the body the core
-- receives: nothing that decides reads it (REQ-PO-4). Every row logged before this migration
-- carried none, so the bound is added NOT VALID: it holds for every new row, and an existing
-- received-log is not scanned under an exclusive lock to prove what is already true.

ALTER TABLE received_log ADD COLUMN pass_through TEXT;

ALTER TABLE received_log ADD CONSTRAINT received_log_pass_through_bound
    CHECK (pass_through IS NULL OR (input_kind = 'command' AND octet_length(pass_through) <= 1024))
    NOT VALID;
