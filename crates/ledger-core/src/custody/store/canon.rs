// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The custody records in an export (`F-014` T-014-01, T-014-03): every pending change as it
//! stands, the named successors, every account's custody epoch and the notices. The index of changes still pending is derived
//! again on import, as [`CustodyState::apply`] keeps it.

use alloc::vec::Vec;

use super::{CustodyState, Notice, PendingChange};
use crate::custody::machine::ChangeState;
use crate::export::canon::DecodeError;
use crate::id::{AccountId, IdentityId};

impl CustodyState {
    /// Every change recorded, in identity order.
    pub(crate) fn changes(&self) -> impl Iterator<Item = &PendingChange> {
        self.pending.values()
    }

    /// Every named successor, by account.
    pub(crate) fn successors(&self) -> impl Iterator<Item = (&AccountId, &IdentityId)> {
        self.successors.iter()
    }

    /// Every account's custody epoch, by account.
    pub(crate) fn epochs(&self) -> impl Iterator<Item = (&AccountId, &u64)> {
        self.epochs.iter()
    }

    /// Rebuild the records. At most one change of each kind may be pending per account; keys must
    /// be strictly increasing.
    pub(crate) fn restore(
        changes: Vec<PendingChange>,
        successors: Vec<(AccountId, IdentityId)>,
        epochs: Vec<(AccountId, u64)>,
        notices: Vec<Notice>,
    ) -> Result<CustodyState, DecodeError> {
        let mut out = CustodyState::new();
        for c in changes {
            if out
                .pending
                .last_key_value()
                .is_some_and(|(k, _)| *k >= c.id)
            {
                return Err(DecodeError("custody changes out of order"));
            }
            if c.state == ChangeState::Pending
                && out.open.insert((c.account, c.kind), c.id).is_some()
            {
                return Err(DecodeError("two changes of one kind pending"));
            }
            out.pending.insert(c.id, c);
        }
        for (account, successor) in successors {
            if out
                .successors
                .last_key_value()
                .is_some_and(|(k, _)| *k >= account)
            {
                return Err(DecodeError("successors out of order"));
            }
            out.successors.insert(account, successor);
        }
        for (account, epoch) in epochs {
            if out
                .epochs
                .last_key_value()
                .is_some_and(|(k, _)| *k >= account)
            {
                return Err(DecodeError("custody epochs out of order"));
            }
            out.epochs.insert(account, epoch);
        }
        for n in notices {
            if out.outbox.last_key_value().is_some_and(|(k, _)| *k >= n.id) {
                return Err(DecodeError("custody notices out of order"));
            }
            out.outbox.insert(n.id, n);
        }
        Ok(out)
    }
}
