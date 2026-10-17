-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store, F-027 T-027-12 (amendment 0031, REQ-SA-9): the settlement layer's
-- runtimes the adapter has served, each once the layer confirmed a transaction signed in the
-- format chosen for it. One row per runtime (`<spec_version>:<surface fingerprint>:<format>`),
-- written once and never removed, so a restart does not ask the layer again.

CREATE TABLE settle_runtimes (
    runtime     TEXT PRIMARY KEY,
    body        TEXT NOT NULL
);
