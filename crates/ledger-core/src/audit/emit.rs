// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Emission inside the commit (`F-013` §2, T-013-01).
//!
//! The service store derives the audit record of every batch that carries an attribution record,
//! in that batch, under its own rules ([`crate::state::State`]). One batch is one command, so one
//! batch gives **one** record:
//!
//! 1. its `PutRecord` (a new operation's record), else
//! 2. its `PutTransition` (a transition command of an existing operation: capture, release), else
//! 3. its first lifecycle transition that names the authorities it relied on (`F-007`'s engine,
//!    `resume`, `F-010`'s evidence).
//!
//! A batch whose transitions name no authority (the deadline edges of `tick`) is the service's
//! own work, not a principal's, and gives none. An [`AuditNote`] in the same batch adds what the
//! attribution record does not carry — the justification, more subjects, a class — to the record
//! of its operation. A note with no record to join is a record of its own: a refused operator
//! command, a privileged read. Such a note must name its action and its principal, or the store
//! refuses the batch (`INV-20`).
//!
//! A settlement incident or a gap from [`crate::settlement::CHAINED_SETTLEMENT`] on is an
//! [`IncidentNote`] in the batch that raises it (`T-043-07`): always a record of its own, after
//! the batch's others, never joined to the batch's command record.
//!
//! Each record takes the next sequence number and the hash of the record before it. The records
//! wait in the store's outbox until the audit store has acknowledged them
//! ([`crate::Ledger::ship_audit`]).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::{AuditAction, AuditClass, AuditOutcome, AuditRecord, AuditSubject, Justification};
use crate::account::Time;
use crate::authority::{AuthorityKind, AuthorityUsed};
use crate::id::OperationId;
use crate::lifecycle::store::OpWrite;
use crate::state::{OperationRecord, StoreRefusal, Write};

/// What a command adds to its audit record, or a record of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditNote {
    /// The operation it is about: the record it joins.
    pub op: OperationId,
    /// The class, where the command's differs from its record's.
    pub class: Option<AuditClass>,
    /// What it is of; required when the note is a record of its own.
    pub action: Option<AuditAction>,
    /// The authorities; required when the note is a record of its own.
    pub authorities: Vec<AuthorityUsed>,
    /// Subjects beyond the record's accounts and postings.
    pub subjects: Vec<AuditSubject>,
    /// The justification.
    pub justification: Option<Justification>,
    /// Accepted, or the named refusal.
    pub outcome: AuditOutcome,
    /// The time input.
    pub at: Time,
}

/// A settlement incident or a gap, recorded on the audit trail and not in the book's state
/// (`T-043-07`, amendment 0040, `REQ-AT-1` as amended): one record of the class
/// [`AuditClass::SettlementIncident`], its action the incident whole.
///
/// It names no authority: the core detects an incident in what the settlement adapter reports,
/// and no principal asked for it. The record names the adapter's identity, the source whose
/// input raised it, as its subject, and the incident's operation where it has one; for one with
/// none (a record of no operation, a gap), its operation reference is derived from the incident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncidentNote {
    /// The incident.
    pub incident: crate::settlement::Incident,
    /// The source whose input raised it: the settlement configuration's authoritative source.
    pub source: crate::settlement::SourceId,
    /// The time input of the transition.
    pub at: Time,
    /// Moved from the book's state at the switch to the chained generation (`T-043-08`): the
    /// record carries the justification that says so, and its time is the book's clock then.
    pub moved: bool,
}

/// The justification of a record [`IncidentNote::moved`].
pub const MOVED_TO_AUDIT: &[u8] =
    b"moved from the book's state to the audit trail at the switch to the chained generation (F-043)";

impl IncidentNote {
    /// The operation the incident is of, if it names one.
    pub fn op(&self) -> Option<OperationId> {
        use crate::settlement::Incident as I;
        match self.incident {
            I::AmountMismatch { op, .. }
            | I::LateEvidence { op, .. }
            | I::FinalFailure { op, .. }
            | I::SettlementRefused { op, .. }
            | I::IncludedWithoutRecord { op, .. }
            | I::EpochRefused {
                refusal: crate::settlement::EpochRefusal::InFlight { op },
                ..
            } => Some(op),
            I::Unassociated { .. }
            | I::Unusable { .. }
            | I::Gap(_)
            | I::Missing { .. }
            | I::Unknown { .. }
            | I::BindRefused { .. }
            | I::EpochRefused { .. } => None,
        }
    }

    /// The record's operation reference: the incident's operation, or one derived from the
    /// incident (`aledgent/settlement-incident/v1`).
    fn operation_ref(&self) -> OperationId {
        self.op().unwrap_or_else(|| {
            let d = cipher_clerk::crypto::blake2b_256(
                b"aledgent/settlement-incident/v1",
                &[&crate::export::canon::to_bytes(&self.incident)],
            );
            let mut r = [0u8; 16];
            r.copy_from_slice(&d[..16]);
            OperationId::from_ref(ledger_kernel::posting::OperationRef(r))
        })
    }

    fn draft(&self) -> Draft {
        let op = self.operation_ref();
        let mut subject = alloc::vec![AuditSubject::Identity(self.source.identity())];
        if let Some(o) = self.op() {
            subject.push(AuditSubject::Operation(o));
        }
        Draft {
            class: AuditClass::SettlementIncident,
            action: AuditAction::Incident(self.incident),
            authorities: Vec::new(),
            subject,
            op,
            at: self.at,
            justification: self.moved.then(|| Justification(MOVED_TO_AUDIT.to_vec())),
            outcome: AuditOutcome::Accepted,
        }
    }
}

/// A record before it takes its place in the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Draft {
    class: AuditClass,
    action: AuditAction,
    authorities: Vec<AuthorityUsed>,
    subject: Vec<AuditSubject>,
    op: OperationId,
    at: Time,
    justification: Option<Justification>,
    outcome: AuditOutcome,
}

fn of_record(r: &OperationRecord) -> Draft {
    let mut subject = alloc::vec![AuditSubject::Operation(r.op)];
    subject.extend(r.accounts.iter().copied().map(AuditSubject::Account));
    subject.extend(r.postings.iter().copied().map(AuditSubject::Posting));
    Draft {
        class: AuditClass::of(r.command),
        action: AuditAction::Command(r.command),
        authorities: r.authorities.clone(),
        subject,
        op: r.op,
        at: r.at,
        justification: None,
        outcome: AuditOutcome::Accepted,
    }
}

impl Draft {
    fn join(&mut self, n: &AuditNote) {
        if let Some(c) = n.class {
            self.class = c;
        }
        if let Some(a) = n.action {
            self.action = a;
        }
        if !n.authorities.is_empty() {
            self.authorities = n.authorities.clone();
        }
        for s in &n.subjects {
            if !self.subject.contains(s) {
                self.subject.push(*s);
            }
        }
        if n.justification.is_some() {
            self.justification = n.justification.clone();
        }
        self.outcome = n.outcome;
    }

    fn alone(n: &AuditNote) -> Result<Draft, StoreRefusal> {
        let action = n.action.ok_or(StoreRefusal::Unattributed)?;
        if n.authorities.is_empty() {
            return Err(StoreRefusal::Unattributed);
        }
        let class = n.class.ok_or(StoreRefusal::Unattributed)?;
        let mut subject = alloc::vec![AuditSubject::Operation(n.op)];
        for s in &n.subjects {
            if !subject.contains(s) {
                subject.push(*s);
            }
        }
        Ok(Draft {
            class,
            action,
            authorities: n.authorities.clone(),
            subject,
            op: n.op,
            at: n.at,
            justification: n.justification.clone(),
            outcome: n.outcome,
        })
    }
}

/// The records one batch gives. See the module documentation.
pub(crate) fn derive(writes: &[Write]) -> Result<Vec<Draft>, StoreRefusal> {
    let mut primary: Option<Draft> = None;
    for w in writes {
        if let Write::PutRecord(r) = w {
            primary = Some(of_record(r));
            break;
        }
    }
    if primary.is_none() {
        for w in writes {
            if let Write::PutTransition(r) = w {
                primary = Some(of_record(r));
                break;
            }
        }
    }
    if primary.is_none() {
        for w in writes {
            if let Write::Op(OpWrite::Transition(t)) = w {
                if t.authorities.is_empty() {
                    continue;
                }
                let class = if t
                    .authorities
                    .iter()
                    .any(|a| a.kind == AuthorityKind::Operator)
                {
                    AuditClass::OperatorIntervention
                } else {
                    AuditClass::ValueMoving
                };
                let mut subject = alloc::vec![AuditSubject::Operation(t.op)];
                subject.extend(t.postings.iter().copied().map(AuditSubject::Posting));
                primary = Some(Draft {
                    class,
                    action: AuditAction::Transition {
                        from: t.from,
                        to: t.to,
                        trigger: t.trigger,
                    },
                    authorities: t.authorities.clone(),
                    subject,
                    op: t.op,
                    at: t.at,
                    justification: None,
                    outcome: AuditOutcome::Accepted,
                });
                break;
            }
        }
    }
    let mut out = Vec::new();
    let mut joined = false;
    let mut alone = Vec::new();
    for w in writes {
        if let Write::Audit(n) = w {
            match &mut primary {
                Some(p) if p.op == n.op && !joined => {
                    p.join(n);
                    joined = true;
                }
                _ => alone.push(Draft::alone(n)?),
            }
        }
    }
    out.extend(primary);
    out.extend(alone);
    out.extend(writes.iter().filter_map(|w| match w {
        Write::Incident(n) => Some(n.draft()),
        _ => None,
    }));
    Ok(out)
}

/// How to take back one appended record.
#[derive(Clone, Debug)]
pub(crate) struct AuditUndo {
    seq: u64,
    prev: [u8; 32],
}

/// The store's audit outbox and the head of its chain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditLog {
    next: u64,
    head: [u8; 32],
    outbox: BTreeMap<u64, AuditRecord>,
    shipped: Option<(u64, [u8; 32])>,
}

impl AuditLog {
    /// An empty log.
    pub fn new() -> AuditLog {
        AuditLog::default()
    }

    /// A log that continues a chain another deployment emitted (`F-014`): `emitted` records so
    /// far, the last hashing to `head`, every one of them acknowledged up to `shipped`. The
    /// outbox is empty: an export is taken with nothing unshipped.
    pub(crate) fn continuing(
        emitted: u64,
        head: [u8; 32],
        shipped: Option<(u64, [u8; 32])>,
    ) -> AuditLog {
        AuditLog {
            next: emitted,
            head,
            outbox: BTreeMap::new(),
            shipped,
        }
    }

    /// The hash of the last record emitted: zeroes before the first.
    pub fn head_hash(&self) -> [u8; 32] {
        self.head
    }

    /// How many records the store has emitted: the next record's sequence number.
    pub fn emitted(&self) -> u64 {
        self.next
    }

    /// The last record emitted: its sequence number and hash.
    pub fn head(&self) -> Option<(u64, [u8; 32])> {
        self.next.checked_sub(1).map(|s| (s, self.head))
    }

    /// The records emitted and not yet acknowledged by the audit store, in order.
    pub fn outbox(&self) -> impl Iterator<Item = &AuditRecord> {
        self.outbox.values()
    }

    /// The last record the audit store acknowledged.
    pub fn shipped(&self) -> Option<(u64, [u8; 32])> {
        self.shipped
    }

    pub(crate) fn push(&mut self, d: Draft) -> AuditUndo {
        let seq = self.next;
        let record = AuditRecord {
            audit_seq: seq,
            prev_hash: self.head,
            class: d.class,
            action: d.action,
            authorities: d.authorities,
            subject: d.subject,
            operation_ref: d.op,
            at: d.at,
            justification: d.justification,
            outcome: d.outcome,
        };
        let undo = AuditUndo {
            seq,
            prev: self.head,
        };
        self.head = record.hash();
        self.next = seq + 1;
        self.outbox.insert(seq, record);
        undo
    }

    pub(crate) fn undo(&mut self, u: AuditUndo) {
        self.outbox.remove(&u.seq);
        self.next = u.seq;
        self.head = u.prev;
    }

    /// How many records wait in the outbox for the audit store.
    pub fn unshipped(&self) -> usize {
        self.outbox.len()
    }

    /// The oldest record the audit store has not acknowledged.
    pub fn oldest_unshipped(&self) -> Option<&AuditRecord> {
        self.outbox.values().next()
    }

    /// The audit store holds every record through `head` (its sequence number and hash): they
    /// leave the outbox, and `shipped` moves to the last of them. `Err(seq)` if the record the
    /// outbox (or the last acknowledgement) holds at `head`'s number is not the store's: nothing
    /// is acknowledged then. A `head` past what was emitted acknowledges everything emitted so
    /// far; its own record is checked when it is emitted and acknowledged again.
    pub(crate) fn acknowledge_through(&mut self, head: (u64, [u8; 32])) -> Result<usize, u64> {
        let (seq, hash) = head;
        match (self.outbox.get(&seq), self.shipped) {
            (Some(r), _) if r.hash() != hash => return Err(seq),
            (None, Some((s, h))) if s == seq && h != hash => return Err(seq),
            _ => {}
        }
        let keep = match seq.checked_add(1) {
            Some(next) => self.outbox.split_off(&next),
            None => BTreeMap::new(),
        };
        let gone = core::mem::replace(&mut self.outbox, keep);
        if let Some((&last, r)) = gone.last_key_value()
            && self.shipped.is_none_or(|(s, _)| s < last)
        {
            let h = if last == seq { hash } else { r.hash() };
            self.shipped = Some((last, h));
        }
        Ok(gone.len())
    }

    /// The audit store holds record `seq`: it leaves the outbox.
    pub(crate) fn acknowledge(&mut self, seq: u64) {
        if let Some(r) = self.outbox.remove(&seq) {
            if self.shipped.is_none_or(|(s, _)| s < seq) {
                self.shipped = Some((seq, r.hash()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::AuthorityKind;
    use crate::id::{CredentialId, IdentityId};
    use crate::state::Command;
    use ledger_kernel::posting::OperationRef;

    fn log(n: u8) -> AuditLog {
        let op = OperationId::from_ref(OperationRef([7; 16]));
        let mut log = AuditLog::new();
        for i in 0..n {
            log.push(Draft {
                class: AuditClass::ValueMoving,
                action: AuditAction::Command(Command::Transfer),
                authorities: alloc::vec![AuthorityUsed {
                    kind: AuthorityKind::Owner,
                    grant: None,
                    principal: IdentityId::mint(&op, 0),
                    credential: CredentialId::mint(&op, 1),
                    on_behalf_of: None,
                }],
                subject: Vec::new(),
                op,
                at: u64::from(i),
                justification: None,
                outcome: AuditOutcome::Accepted,
            });
        }
        log
    }

    /// `REQ-AT-2`, `T-013-13`: the store's head takes every record through it out of the outbox,
    /// once; a head the outbox does not hold at that number takes nothing; a head past what was
    /// emitted takes everything emitted, and its own record is checked when it comes.
    #[test]
    fn req_at_2_acknowledging_through_the_stores_head_trims_the_outbox_once() {
        let mut l = log(5);
        let hashes: Vec<[u8; 32]> = l.outbox().map(AuditRecord::hash).collect();
        assert_eq!(l.unshipped(), 5);
        assert_eq!(l.acknowledge_through((1, [0; 32])), Err(1));
        assert_eq!(l.unshipped(), 5);
        assert_eq!(l.acknowledge_through((2, hashes[2])), Ok(3));
        assert_eq!(l.shipped(), Some((2, hashes[2])));
        assert_eq!(l.oldest_unshipped().map(|r| r.audit_seq), Some(3));
        // Again: nothing left to take, and the same head still matches.
        assert_eq!(l.acknowledge_through((2, hashes[2])), Ok(0));
        assert_eq!(l.acknowledge_through((2, [1; 32])), Err(2));
        // Past the head of the chain: all of it; `shipped` is the last record held.
        assert_eq!(l.acknowledge_through((9, [3; 32])), Ok(2));
        assert_eq!(l.shipped(), Some((4, hashes[4])));
        assert_eq!(l.unshipped(), 0);
        assert_eq!(l.emitted(), 5);
    }
}
