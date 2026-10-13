// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Shipping the audit outbox to the audit store, and checking the store's head (`F-013` §2–§3,
//! T-013-01, T-013-02, `REQ-AT-2`).
//!
//! The core emits every record inside its commit ([`crate::audit::emit`]). A worker then copies
//! the outbox to the audit store, **at least once**: a record leaves the outbox only once the
//! store has taken it, and the store accepts a record it already holds unchanged, so a retry after
//! a lost acknowledgement is harmless. On a cadence the host compares the store's head with what
//! the core emitted and shipped; a store behind the core's acknowledged head, or holding another
//! record there, has lost or changed something, and that raises an incident.

use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::Ledger;
use crate::account::Time;
use crate::audit::{AuditRecord, AuditSink, SinkError};
use crate::operator::{Incident, IncidentKind, OpsWrite};
use crate::state::Write;

/// What the audit store's head says against the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditGap {
    /// The store holds fewer records than the core has seen it acknowledge: records were lost,
    /// or the store was truncated.
    Behind {
        /// The store's head.
        store: Option<u64>,
        /// The last record the core saw acknowledged.
        acknowledged: u64,
    },
    /// The store holds a different record at its head than the core emitted there.
    Diverged {
        /// The sequence number.
        seq: u64,
    },
    /// The store could not be read.
    Unreadable,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// Copy every record of the audit outbox to `sink`, in order, and drop from the outbox each
    /// one the sink took. Stops at the first refusal and returns it; what was taken stays taken.
    /// Returns how many records the sink took.
    pub fn ship_audit(&mut self, sink: &mut dyn AuditSink) -> Result<u64, SinkError> {
        let pending: Vec<AuditRecord> = self.state.audit().outbox().cloned().collect();
        let mut shipped = 0;
        for r in pending {
            sink.append(&r)?;
            self.state.audit_acknowledge(r.audit_seq);
            shipped += 1;
        }
        Ok(shipped)
    }

    /// The audit store holds every record through `head` (its last record's sequence number and
    /// hash): drop them from the outbox, without offering them again (`REQ-AT-2`, `T-013-13`).
    /// This is the second half of a shipment a host made itself: it copied the outbox to its
    /// sink outside the core's lock, and acknowledges once the sink confirmed the append. A host
    /// rebuilding its replica from its archive calls it after every input with the store's head,
    /// so the records the store already holds never accumulate again. Returns how many records
    /// left the outbox.
    ///
    /// Not a change to the book, and not recorded as a call: the Agent keeps no outbox
    /// (`crate::call`). `Err(AuditGap::Diverged)` if the record emitted at `head`'s number is not
    /// the one the store holds: nothing leaves the outbox then.
    pub fn acknowledge_audit(&mut self, head: (u64, [u8; 32])) -> Result<u64, AuditGap> {
        self.state
            .audit_acknowledge_through(head)
            .map(|n| n as u64)
            .map_err(|seq| AuditGap::Diverged { seq })
    }

    /// Compare the audit store's head with the core's (`F-013` §3). On a gap, raise an incident
    /// on the operator channel (`F-018` pages on it) and return the gap.
    pub fn check_audit_head(&mut self, sink: &dyn AuditSink, now: Time) -> Result<(), AuditGap> {
        let gap = self.audit_gap(sink);
        let Some(gap) = gap else {
            return Ok(());
        };
        let emitted = self.state.audit().emitted();
        let key = [&emitted.to_be_bytes()[..], &now.to_be_bytes()[..]].concat();
        let incident = Incident::new(IncidentKind::AuditGap, &key, None, now);
        // A second check at the same time and head raises the same incident once.
        let _ = self
            .state
            .commit(&[Write::Ops(OpsWrite::Incident(incident))]);
        Err(gap)
    }

    fn audit_gap(&self, sink: &dyn AuditSink) -> Option<AuditGap> {
        let head = match sink.head() {
            Ok(h) => h,
            Err(_) => return Some(AuditGap::Unreadable),
        };
        let log = self.state.audit();
        let in_outbox = |seq: u64, hash: [u8; 32]| {
            log.outbox()
                .find(|r| r.audit_seq == seq)
                .is_some_and(|r| r.hash() == hash)
        };
        match (log.shipped(), head) {
            (None, None) => None,
            (None, Some((seq, hash))) => {
                (!in_outbox(seq, hash)).then_some(AuditGap::Diverged { seq })
            }
            (Some((acknowledged, _)), None) => Some(AuditGap::Behind {
                store: None,
                acknowledged,
            }),
            (Some((acknowledged, want)), Some((seq, hash))) => {
                if seq < acknowledged {
                    Some(AuditGap::Behind {
                        store: Some(seq),
                        acknowledged,
                    })
                } else if seq == acknowledged {
                    (hash != want).then_some(AuditGap::Diverged { seq })
                } else {
                    (!in_outbox(seq, hash)).then_some(AuditGap::Diverged { seq })
                }
            }
        }
    }
}
