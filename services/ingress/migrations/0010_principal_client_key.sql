-- F-009, T-009-16 (REQ-OP-10, REQ-ID-7, finding ingress-1): one command row per (principal, surface,
-- client key), whatever its domain, beside the per-domain index of version 3.
--
-- Version 3 dropped this index when the client key's domain became the principal's integrating
-- party. Uniqueness per domain alone lets one principal's key be logged twice, once in each domain,
-- by two attempts of one request that chose their domains on either side of the principal's tie.
-- With both indexes an append that conflicts on either takes the row already logged instead, and
-- its digest decides the answer (the first decision, or ERR-ClientKeyReused).
--
-- Version 10. Applied in one transaction like every migration: if the log already holds two
-- command rows of one principal under one surface and key, it stops here, names the problem, and
-- changes nothing. Those rows are two requests decided under one key, which a person reviews; no
-- migration picks one. To check a database first, read-only:
--
--   SELECT principal, surface, client_key, count(*) AS rows,
--          array_agg(client_key_domain ORDER BY seq) AS domains,
--          array_agg(encode(op_ref, 'hex') ORDER BY seq) AS op_refs
--     FROM received_log
--    WHERE input_kind = 'command'
--    GROUP BY principal, surface, client_key
--   HAVING count(*) > 1;
--
-- The index is built inside the migration's transaction, so appends to the log wait for it: a scan
-- of the command rows.

DO $$
DECLARE
    duplicated BIGINT;
BEGIN
    SELECT count(*) INTO duplicated FROM (
        SELECT 1 FROM received_log
         WHERE input_kind = 'command'
         GROUP BY principal, surface, client_key
        HAVING count(*) > 1
    ) d;
    IF duplicated > 0 THEN
        RAISE EXCEPTION
            'migration 0010: % (principal, surface, client_key) groups hold more than one command row; nothing was changed. List them with the query in 0010_principal_client_key.sql and have them reviewed before applying it',
            duplicated;
    END IF;
END
$$;

CREATE UNIQUE INDEX received_log_principal_client_key
    ON received_log (principal, surface, client_key)
    WHERE input_kind = 'command';
