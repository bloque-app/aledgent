-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- aledgent-settle's store: the deposit backing check's rule changed after 0007 was first written.
-- A verdict stored under the earlier rule (a block-level comparison of holding and recorded
-- position) is not trusted: every one is removed here, once. A kept deposit with no verdict is
-- reported without a movement, so nothing is credited on the strength of the earlier rule; the
-- `deposit-unbacked` alert it raises says the backing was never proved.

DELETE FROM settle_deposit_backing;
