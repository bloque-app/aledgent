-- F-037 (T-037-04, REQ-RT-20, amendment 0035): a way out of a scheduled upgrade overtakes the
-- forwarder's queue.
--
-- overtakes   true for a row that must reach the writer before every other row that is not already in
--             flight: a way-out input (reschedule_upgrade, cancel_upgrade) from a caller the core said
--             may take it, at the door. A fenced writer refuses every other input, and the forwarder
--             retries the refused one first for ever, so a way out logged behind it would never be
--             sent. The forwarder reads undecided rows already given a feed sequence first (they may
--             have run), then overtaking rows, then reservations, then the rest in arrival order.
--
-- Version 9, after version 8's roster. A constant default: PostgreSQL (11 and later) adds the column
-- to the catalogue only, with no rewrite and no scan, so it is instant on a log of any size; every row
-- logged before it is false. No index: the forwarder reads the undecided rows through
-- received_log_undecided (status <> 'Decided'), a small set, and orders them itself.
SET LOCAL lock_timeout = '5s';

ALTER TABLE received_log ADD COLUMN overtakes BOOLEAN NOT NULL DEFAULT FALSE;
