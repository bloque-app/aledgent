-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store, F-029 (amendment 0011, REQ-AS-8): the bindings of book-only assets the
-- adapter was asked to make, and the extrinsics that carry them (REQ-SA-5, REQ-SA-6). One row per
-- bind call's reference; its asset, location and amount never change.

CREATE TABLE settle_binds (
    reference   TEXT PRIMARY KEY,
    body        TEXT NOT NULL
);
