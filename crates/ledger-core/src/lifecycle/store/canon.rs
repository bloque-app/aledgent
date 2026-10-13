// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The operation store in an export (`F-014` T-014-01, T-014-03): every operation record as it
//! stands, the clock, the outbox and the client-key entries. The indexes — transition keys,
//! deadlines, open-since — are derived again from the records on import, by the same code that
//! keeps them now.

use alloc::vec::Vec;

use super::{Notification, Operation, Operations, RequestEntry};
use crate::account::Time;
use crate::export::canon::{Canonical, Dec, DecodeError, Enc};

impl Canonical for Operation {
    fn encode(&self, e: &mut Enc) {
        let Operation {
            reference,
            kind,
            class,
            origin,
            initiator,
            lifecycle,
            opened_at,
            overdue,
            flagged,
            transitions,
        } = self;
        e.put(reference)
            .put(kind)
            .put(class)
            .put(origin)
            .put(initiator)
            .put(lifecycle)
            .put(opened_at)
            .put(overdue)
            .put(flagged)
            .put(transitions);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Operation {
            reference: d.get()?,
            kind: d.get()?,
            class: d.get()?,
            origin: d.get()?,
            initiator: d.get()?,
            lifecycle: d.get()?,
            opened_at: d.get()?,
            overdue: d.get()?,
            flagged: d.get()?,
            transitions: d.get()?,
        })
    }
}

impl Operations {
    /// Every client-key entry, by operation.
    pub(crate) fn requests(&self) -> impl Iterator<Item = &RequestEntry> {
        self.requests.values()
    }

    /// Rebuild the store from its records. Each index is derived as [`Operations::apply`] keeps
    /// it; a transition key recorded twice, or a record out of order, is refused.
    pub(crate) fn restore(
        ops: Vec<Operation>,
        clock: Option<Time>,
        outbox: Vec<Notification>,
        requests: Vec<RequestEntry>,
    ) -> Result<Operations, DecodeError> {
        let mut out = Operations::default();
        out.clock.set(clock);
        for o in ops {
            let id = o.reference;
            if out.ops.last_key_value().is_some_and(|(k, _)| *k >= id) {
                return Err(DecodeError("operations out of order"));
            }
            for (i, t) in o.transitions.iter().enumerate() {
                let i = u32::try_from(i).map_err(|_| DecodeError("transitions"))?;
                if out.keys.insert(t.key, (id, i)).is_some() {
                    return Err(DecodeError("transition key recorded twice"));
                }
            }
            out.put(id, Some(o));
        }
        for n in outbox {
            if out.outbox.last_key_value().is_some_and(|(k, _)| *k >= n.id) {
                return Err(DecodeError("outbox out of order"));
            }
            out.outbox.insert(n.id, n);
        }
        for r in requests {
            if out
                .requests
                .last_key_value()
                .is_some_and(|(k, _)| *k >= r.op)
            {
                return Err(DecodeError("client-key entries out of order"));
            }
            out.requests.insert(r.op, r);
        }
        Ok(out)
    }
}
